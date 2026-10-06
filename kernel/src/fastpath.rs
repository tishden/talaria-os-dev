//! The zero-syscall fast path (the Talaria/Track A model on a self-hosting kernel).
//! A cell on an AP (ring 0) bridges virtio <-> two SPSC rings in SHARED memory;
//! the application on the BSP (ring 3) polls the rings and processes the packets WITHOUT syscalls.
//! The rings lie in a user-accessible region (>=32MB, U/S=1), so they are visible both to ring0 and to ring3.
use core::ptr::{addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};
use crate::l1::NicQ;
use crate::netdev::NetDev;
use crate::tsc;
static mut STAT_N: u64 = 0;
static mut STAT_SUM: u64 = 0;
static mut STAT_MIN: u64 = u64::MAX;
static mut STAT_MAX: u64 = 0;
static mut DIAG_TX_N: u64 = 0; static mut DIAG_TX_CYC: u64 = 0; static mut DIAG_WAIT_CYC: u64 = 0; static mut DIAG_RX_N: u64 = 0;
    static mut DIAG_RX_FULL: u64 = 0;
static mut DIAG_TX_KICKS: u64 = 0; // how many batches really had to be kicked (DEVELOPMENT-GUIDE §6b, the batched virtio notify)

// --- the layout of the shared memory (it mirrors fastapp.c) ---
//
// The layout, the index arithmetic and the ARP the cell answers itself live in
// `fastring.rs`: they are pure byte and index arithmetic whose mistake is silent (a
// wrongly written wrap drops frames under load, a wrong ARP offset answers with a
// plausible frame naming the wrong address), so they are tested on the host.
use crate::fastring::{self, HDR, MASK, RX_OFF, TX_OFF};
pub const FN_SLOTS: u32 = fastring::SLOTS;
pub const REGION_SZ: usize = fastring::REGION_SZ;

// DMAP: dual — the region is a frame (`alloc_contig`) the cell reads through kptr, and ring 3
// polls it at the same identity address (`talaria_fastmap`, syscall.rs); stage 4 splits them.
#[inline] fn p_prod(base: usize, ring: usize) -> *mut u32 { crate::mem::kptr((base + ring) as u64) }
#[inline] fn p_cons(base: usize, ring: usize) -> *mut u32 { crate::mem::kptr((base + ring + fastring::CONS_OFF) as u64) }
#[inline] fn p_slot(base: usize, ring: usize, i: u32) -> usize { base + ring + fastring::slot_off(i) }

/// Zero the rings' heads/tails (it is called on the BSP before the start of the cell/application).
pub fn init_region(base: usize) {
    unsafe {
        write_volatile(p_prod(base, RX_OFF), 0); write_volatile(p_cons(base, RX_OFF), 0);
        write_volatile(p_prod(base, TX_OFF), 0); write_volatile(p_cons(base, TX_OFF), 0);
    }
}

const BUF: usize = crate::l1::BUF;
const NRX: usize = 16;
const NTX: usize = 48; // deeper TX batches (a kick once per batch); < QS=64 with room to spare for RX
static mut RXB: [[u8; BUF]; NRX] = [[0; BUF]; NRX];
static mut TXB: [[u8; BUF]; NTX] = [[0; BUF]; NTX];
static mut RX_TS: [u64; FN_SLOTS as usize] = [0; FN_SLOTS as usize];

/// The cell: an endless bridge card<->rings. It works on a dedicated core (an AP).
/// §7.4: the card is any one (`dyn NetDev`), the queues/buffers go through `NicQ`, so the
/// zero-syscall path works the same way under QEMU (virtio) and on EC2 (the ENA).
pub fn run_cell(dev: &'static mut dyn NetDev, mac: [u8; 6], base: usize) -> ! {
    let (rb, tb) = unsafe { (&mut *addr_of_mut!(RXB), &mut *addr_of_mut!(TXB)) };
    // We keep the NicQ in a STATIC rather than on the stack: inside it lies the whole virtio device
    // (the rings at QS=256), and a pair of its copies at the construction ate the AP's stack
    // (32 KB) — a #UD with a garbage rip and an "index out of bounds" in task_cr3.
    static mut CELLQ: Option<NicQ> = None;
    let q = unsafe {
        core::ptr::write(core::ptr::addr_of_mut!(CELLQ), Some(NicQ::new(dev, rb, tb)));
        (*core::ptr::addr_of_mut!(CELLQ)).as_mut().unwrap()
    };
    // A2.2 hot-path enforcement: a snapshot of the counters at the entry into the steady-state window.
    // The datapath (the ring3 application + the cell) on the hot path does NOT make syscalls/faults —
    // below we check that by the delta per processed packet.
    let mut base_sc = 0u64; let mut base_pf = 0u64; let mut armed = false;
    loop {
        q.reap_tx();
        // the card's RX -> the rx ring (the cell = the producer)
        while let Some((slot, hdr, plen)) = q.poll_rx() {
            let plen = fastring::clamp_len(plen);
            // an ARP request to our IP -> answer it ourselves (the fastpath is self-sufficient)
            let my_ip = crate::netsys::cfg_ip(); // our IP (talaria.ip=, by default 10.0.0.2)
            let is_arp_req = fastring::is_arp_request_for(&q.rx_buf(slot)[hdr..hdr + plen], my_ip);
            if is_arp_req {
                if let Some(fs) = q.tx_alloc() {
                    let (reqmac, reqip) = fastring::arp_sender(&q.rx_buf(slot)[hdr..hdr + plen]);
                    let h2 = q.tx_hdr(fs);
                    fastring::build_arp_reply(&mut q.tx_buf(fs)[h2..], mac, my_ip, reqmac, reqip);
                    q.tx_send(fs, h2 + fastring::ARP_LEN);
                    if q.flush() { unsafe { DIAG_TX_KICKS += 1; } }
                }
                q.rx_done(slot);
                continue;
            }
            unsafe {
                let prod = read_volatile(p_prod(base, RX_OFF));
                let cons = read_volatile(p_cons(base, RX_OFF));
                if !fastring::can_produce(prod, cons) { crate::telemetry::inc_gap(); DIAG_RX_FULL += 1; } // A2.3 gap
                DIAG_RX_N += 1;
                if fastring::can_produce(prod, cons) {
                    let s = p_slot(base, RX_OFF, prod);
                    core::ptr::copy_nonoverlapping(q.rx_ptr(slot).add(hdr), crate::mem::kptr::<u8>((s + HDR) as u64), plen);
                    write_volatile(crate::mem::kptr::<u32>(s as u64), plen as u32);
                    RX_TS[(prod & MASK) as usize] = tsc::rdtsc();
                    fence(Ordering::Release);
                    write_volatile(p_prod(base, RX_OFF), prod.wrapping_add(1));
                }
            }
            q.rx_done(slot); // repost the buffer onto RX (the doorbell goes into flush, one per batch)
        }
        // the tx ring -> the card (the cell = the consumer)
        unsafe {
            let tp = read_volatile(p_prod(base, TX_OFF));
            let mut tc = read_volatile(p_cons(base, TX_OFF));
            let mut added = false;
            while fastring::can_consume(tp, tc) {
                if let Some(fslot) = q.tx_alloc() {
                    let s = p_slot(base, TX_OFF, tc);
                    fence(Ordering::Acquire);
                    let len = fastring::clamp_len(read_volatile(crate::mem::kptr::<u32>(s as u64)) as usize);
                    let hdr = q.tx_hdr(fslot);
                    core::ptr::copy_nonoverlapping(crate::mem::kptr::<u8>((s + HDR) as u64), q.tx_ptr(fslot).add(hdr), len);
                    let t_tx0 = tsc::rdtsc();
                    // the batch notify (§7.2): the frames are put into the queue WITHOUT a doorbell,
                    // one notify for the whole batch after the loop — the VMEXIT is amortised
                    q.tx_send(fslot, hdr + len);
                    DIAG_TX_CYC += tsc::rdtsc().wrapping_sub(t_tx0); DIAG_TX_N += 1;
                    if DIAG_TX_N.is_multiple_of(20_000) {
                        let hz = tsc::calibrate_cached();
                        let (n, cyc, wait) = (DIAG_TX_N, DIAG_TX_CYC, DIAG_WAIT_CYC);
                        let (kicks, rx, full) = (DIAG_TX_KICKS, DIAG_RX_N, DIAG_RX_FULL);
                        crate::println!("cell-diag: tx={} avg_transmit_begin={}ns wait_free={}ns/frame kicks={} rx={} rx_ring_full={}",
                            n, cyc / n * 1_000_000_000 / hz, wait / n * 1_000_000_000 / hz, kicks, rx, full);
                    }
                    added = true;
                    let lat = tsc::rdtsc().wrapping_sub(RX_TS[(tc & MASK) as usize]);
                    STAT_N += 1; STAT_SUM += lat;
                    crate::telemetry::record_lat(lat); // A2.3 the telemetry
                    // The first packet = the end of the warm-up (the application's start-up syscalls are behind);
                    // we take the counters' base here, further on we measure ONLY the steady-state delta.
                    if !armed { let (sc, pf) = crate::metrics::snapshot(); base_sc = sc; base_pf = pf; armed = true; }
                    if STAT_N <= 6 || STAT_N.is_multiple_of(20_000) { crate::telemetry::dump(); }
                    if armed && (STAT_N <= 6 || STAT_N.is_multiple_of(20_000)) {
                        let (sc, pf) = crate::metrics::snapshot();
                        let dsc = sc - base_sc; let dpf = pf - base_pf;
                        let ok = dsc == 0 && dpf == 0;
                        let n = STAT_N;
                        crate::println!("hotpath enforce: packets={} Δsyscalls={} Δfaults={} -> {}",
                            n, dsc, dpf, if ok { "OK zero-syscall" } else { "A VIOLATION" });
                    }
                    if lat < STAT_MIN { STAT_MIN = lat; }
                    if lat > STAT_MAX { STAT_MAX = lat; }
                    if STAT_N.is_multiple_of(20_000) {
                        let hz = tsc::calibrate_cached();
                        let (n, lo, hi, sum) = (STAT_N, STAT_MIN, STAT_MAX, STAT_SUM);
                        crate::println!("fastpath cell: n={} the rt latency min={}c({}ns) avg={}c({}ns) max={}c({}ns)",
                            n, lo, lo*1_000_000_000/hz,
                            sum/n, (sum/n)*1_000_000_000/hz,
                            hi, hi*1_000_000_000/hz);
                    }
                    tc = tc.wrapping_add(1);
                    // The slot's bytes were READ above; the producer may overwrite them the moment
                    // it sees the new `cons`. A release fence keeps those loads before the publish
                    // — free on x86 (TSO), a `dmb` on arm64, where a load may otherwise complete
                    // after a later store (docs/ARM64-PLAN.md §4.13).
                    fence(Ordering::Release);
                    write_volatile(p_cons(base, TX_OFF), tc);
                } else {
                    // there are no free TX buffers — let them be freed
                    let t_w0 = tsc::rdtsc();
                    q.reap_tx();
                    DIAG_WAIT_CYC += tsc::rdtsc().wrapping_sub(t_w0);
                    // the queue is full of unnotified frames — kick it, so that the host takes them
                    if added && q.flush() { DIAG_TX_KICKS += 1; }
                    added = false;
                }
            }
            // one kick per batch (or zero, if the host polls/the EVENT_IDX has suppressed it)
            if q.flush() && added { DIAG_TX_KICKS += 1; }
        }
        core::hint::spin_loop();
    }
}

// the address of the shared region for the fastmap syscall
use core::sync::atomic::AtomicU64;
pub static REGION_BASE: AtomicU64 = AtomicU64::new(0);
pub fn set_region(base: u64) { REGION_BASE.store(base, Ordering::SeqCst); }
pub fn region() -> u64 { REGION_BASE.load(Ordering::SeqCst) }
