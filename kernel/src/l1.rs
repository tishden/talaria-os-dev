//! L1: a full-blown network stack (smoltcp) on top of our virtio path.
//! A UDP echo through udp::Socket — we measure the honest price of the stack against
//! the raw mode (an in-place echo in main.rs).

use core::ptr::addr_of_mut;

use smoltcp::iface::{Config, Interface, SocketSet, SocketStorage};
use smoltcp::phy::{Checksum, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{udp, tcp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

use crate::netdev::NetDev;
use crate::tsc;


// The network card is a `dyn NetDev` (see `netdev.rs`). Only the frame buffers
// and the ring plumbing common to all cards are left here.

/// §A1: talaria.ena=txbench=N — how many frames to drive through the transmit path right
/// after the card is brought up (0 — do not drive any). See NicQ::tx_bench.
pub static TXBENCH: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// A3: talaria.ena=rxbench=N — print the RECEIVE rate once every N seconds
/// (0 — do not print). Unlike `txbench` this is not a separate run but a counter
/// in the datapath itself: the receive path cannot be measured from the inside, it is created by a neighbour
/// (`bench/udp_flood.c`), while we count how many frames really reached the
/// driver. So the mode is compatible with any datapath (the smoltcp echo, a cell) and
/// does not interfere with the RTT measurement — counting frames is free, the printing happens only when
/// something arrived during the interval (the serial buffer on EC2 is small, extra lines
/// overwrite the useful ones — the pitfall of session 3).
pub static RXBENCH: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

// §4.5: with virtio the ceiling is the virtqueue's size (QS=256), with ENA it is RX_RING.
// The more buffers are posted, the more descriptors the device has to
// prefetch (the hypothesis of step 3 on the datapath's gap).
const NRX: usize = 256;
const NTX: usize = 64;
pub const BUF: usize = crate::netdev::FRAME;

static mut RXB: [[u8; BUF]; NRX] = [[0; BUF]; NRX];
static mut TXB: [[u8; BUF]; NTX] = [[0; BUF]; NTX];

// V1.11: the frame buffers FOR THE REST of the cards. The first card's rings stayed as they
// were (it is the datapath's card — its depth was chosen by measurement, §4.5), while the control
// interfaces do not need that many: 64 receive buffers are more than enough, while an extra
// 512 KB per card is both the image's .bss and a footprint in L3 (the leaky DMA rule).
const NRX2: usize = 64;
const NTX2: usize = 32;
pub const MAXEXTRA: usize = 3;
static mut RXB2: [[[u8; BUF]; NRX2]; MAXEXTRA] = [[[0; BUF]; NRX2]; MAXEXTRA];
static mut TXB2: [[[u8; BUF]; NTX2]; MAXEXTRA] = [[[0; BUF]; NTX2]; MAXEXTRA];

/// §7.4: slot-oriented rings on top of ANY card (`dyn NetDev`). It hides the difference of the
/// models — with virtio a descriptor is addressed by a "token" from the virtqueue, with ENA the slot
/// is set by the driver itself (req_id) — and it owns the accounting of the frame buffers. The users are:
/// `SmolDev` (smoltcp: the kernel modes and netsys) and `fastpath::run_cell` (the zero-syscall
/// cell on an AP). The buffers are given by the caller (they are in the kernel's .bss — identity,
/// phys == virt, the DMA is correct).
///
/// The calls to the device (the doorbell/kick) ARE ACCUMULATED: `flush()` is called once per pass
/// (§7.2) rather than per frame.
pub struct NicQ {
    // §1.1: the card BY REFERENCE. Previously `Nic` travelled by value (BSP -> run_datapath ->
    // run_cell -> NicQ::new) and was copied at every step: a virtio device with
    // QS=256 rings inside is tens of kilobytes, because of which the AP's stack had to be
    // grown to 128 KB. The card itself lives in a static of ITS OWN DRIVER (netdev.rs).
    pub dev: &'static mut dyn NetDev,
    rxb: *mut [u8; BUF],
    txb: *mut [u8; BUF], ntx: usize,
    // A3 (rxbench): the frames and bytes received during the current interval + the mark of its start.
    rx_n: u64,
    rx_b: u64,
    rx_t0: u64,
    rx_best: u64,
    // The same for the TRANSMIT path. The receive path has been counted since session A3, the transmit path has not, and `netstat`
    // could say "12 million frames received" without saying whether they were answered.
    // Two writes into the hot fields of a structure that the transmit path writes anyway.
    tx_n: u64,
    tx_b: u64,
}

impl NicQ {
    /// Take the card and the buffers, post the whole receive ring.
    pub fn new(dev: &'static mut dyn NetDev, rxb: &'static mut [[u8; BUF]], txb: &'static mut [[u8; BUF]]) -> Self {
        let (nrx, ntx) = (rxb.len(), txb.len().min(crate::netdev::MAXTX));
        let (mut rxp, mut txp) = (rxb.as_mut_ptr(), txb.as_mut_ptr());
        // §4.5: talaria.ena=hibuf — move the frame buffers into the memory above 4 GB.
        // The rings have been there since session 2 (otherwise the device rejected CREATE_CQ), while the buffers
        // lived in the kernel's .bss, i.e. in the first 32 MB of physical memory.
        if crate::ena::WANT_HIBUF.load(core::sync::atomic::Ordering::SeqCst) {
            let pr = (nrx * BUF).div_ceil(4096) as u64;
            let pt = (ntx * BUF).div_ceil(4096) as u64;
            match (crate::mem::alloc_high(pr, 4096), crate::mem::alloc_high(pt, 4096)) {
                (Some(a), Some(b)) => {
                    rxp = crate::mem::kptr::<[u8; BUF]>(a); txp = crate::mem::kptr::<[u8; BUF]>(b);
                    crate::println!("l1: the frame buffers above 4 GB: rx={:#x} tx={:#x}", a, b);
                }
                _ => crate::println!("l1: there is no memory above 4 GB — the buffers stay in .bss"),
            }
        }
        // Post the receive ring. How many buffers the card is ready to accept it knows
        // itself (`rx_capacity`), while the call is one for the whole ring rather than per buffer.
        let cap = dev.rx_capacity(nrx);
        // The NetDev buffer address is the kernel's VIRTUAL one (virtnet/nicloop make a slice of it);
        // a driver that hands it to a device translates it itself (ena.rs: `mem::kpa`).
        for i in 0..cap { dev.rx_post(i, crate::mem::vaddr(unsafe { rxp.add(i) })); }
        dev.flush();
        let mut q = NicQ { dev, rxb: rxp, txb: txp, ntx,
               rx_n: 0, rx_b: 0, rx_t0: 0, rx_best: 0, tx_n: 0, tx_b: 0 };
        let tb = TXBENCH.load(core::sync::atomic::Ordering::SeqCst);
        if tb != 0 { q.tx_bench(tb); }
        q
    }

    /// §A1: talaria.ena=txbench=N — a run of THE TRANSMIT PATH. We had not had a single measurement
    /// of the transmit throughput on EC2, and it is exactly what the write-combining
    /// of the LLQ window changes (for the neighbours in the rack 345k -> 1.26M packets/s with
    /// the same descriptor-writing code — only the memory type changed).
    /// The frame is addressed to OUR OWN MAC with the experimental ethertype 0x88B5:
    /// the Nitro switch drops such a frame, the network is not polluted.
    pub fn tx_bench(&mut self, n: u64) {
        const LEN: usize = 64;
        let mac = self.mac();
        for s in 0..self.ntx {
            let b = self.tx_buf(s);
            b[0..6].copy_from_slice(&mac);
            b[6..12].copy_from_slice(&mac);
            b[12] = 0x88; b[13] = 0xB5;
            for i in 14..LEN { b[i] = i as u8; }
        }
        let hz = crate::tsc::calibrate_cached();
        let db0 = self.db_writes().unwrap_or(0);
        let t0 = crate::tsc::rdtsc();
        let (mut sent, mut idle) = (0u64, 0u64);
        while sent < n {
            match self.tx_alloc() {
                Some(s) => {
                    self.tx_send(s, LEN);
                    sent += 1;
                    if sent & 31 == 0 { self.flush(); }   // a batch of calls, as in the datapath
                }
                // the ring is in flight: we wait until the device acknowledges the transmissions
                None => { self.reap_tx(); idle += 1; if idle > 200_000_000 { break; } }
            }
        }
        self.flush();
        let ns = ((crate::tsc::rdtsc() - t0) * 1_000_000_000 / hz).max(1);
        crate::println!("txbench: {} frames of {} B in {} us -> {} packets/s, {} ns/frame, calls {}, idle turns {}",
            sent, LEN, ns / 1000, sent * 1_000_000_000 / ns, ns / sent.max(1),
            self.db_writes().unwrap_or(0) - db0, idle);
    }
    #[inline] pub fn rx_ptr(&self, slot: usize) -> *mut u8 { unsafe { self.rxb.add(slot) as *mut u8 } }
    #[inline] pub fn tx_ptr(&self, slot: usize) -> *mut u8 { unsafe { self.txb.add(slot) as *mut u8 } }
    #[inline] pub fn rx_buf(&self, slot: usize) -> &'static mut [u8; BUF] { unsafe { &mut *self.rxb.add(slot) } }
    #[inline] pub fn tx_buf(&self, slot: usize) -> &'static mut [u8; BUF] { unsafe { &mut *self.txb.add(slot) } }

    /// A received frame: (the slot, the payload's offset in the buffer, the length). None — empty.
    pub fn poll_rx(&mut self) -> Option<(usize, usize, usize)> {
        let got = self.dev.rx_poll();
        // A3: the receive counter. The increment is two writes into its own fields, they are in the cache
        // (the structure is hot), so we count always; the clock is read only when
        // the mode is on, in order not to drag an rdtsc into the RTT measurement path.
        match got {
            Some((_, _, len)) => { self.rx_n += 1; self.rx_b += len as u64; }
            None => if RXBENCH.load(core::sync::atomic::Ordering::Relaxed) != 0 { self.rx_report(); }
        }
        got
    }

    /// A3: the receive report once per interval. We print only non-empty intervals:
    /// in silence the line is not needed, while the serial buffer on EC2 is small.
    fn rx_report(&mut self) {
        let secs = RXBENCH.load(core::sync::atomic::Ordering::Relaxed);
        let hz = crate::tsc::calibrate_cached();
        let now = crate::tsc::rdtsc();
        if self.rx_t0 == 0 { self.rx_t0 = now; return; }
        if now - self.rx_t0 < secs * hz { return; }
        let ns = ((now - self.rx_t0) * 1_000_000_000 / hz).max(1);
        let (n, b) = (self.rx_n, self.rx_b);
        self.rx_t0 = now; self.rx_n = 0; self.rx_b = 0;
        if n == 0 { return; }
        let pps = n * 1_000_000_000 / ns;
        if pps > self.rx_best { self.rx_best = pps; }
        crate::println!("rxbench: {} frames ({} B, {} B/frame) in {} us -> {} packets/s, {} Mbit/s (the best interval {})",
            n, b, b / n, ns / 1000, pps, b * 8 * 1_000_000_000 / ns / 1_000_000, self.rx_best);
    }
    /// Return the buffer slot into the receive ring (the call is deferred until flush()).
    pub fn rx_done(&mut self, slot: usize) {
        let addr = crate::mem::vaddr(unsafe { self.rxb.add(slot) });
        self.dev.rx_post(slot, addr);
    }
    /// Collect the completed transmissions (free the buffers' slots).
    pub fn reap_tx(&mut self) { self.dev.tx_reap(); }
    /// Collect the transmit completions outside a pass (session 32).
    pub fn tx_watch(&mut self) { self.dev.tx_watch(); }
    /// A free transmit slot (None — all the frames are in flight).
    pub fn tx_alloc(&mut self) -> Option<usize> {
        let ntx = self.ntx;
        self.dev.tx_alloc(ntx)
    }
    /// The payload's offset in the transmit buffer (the card's header; 0 — a card without one).
    pub fn tx_hdr(&mut self, slot: usize) -> usize {
        let addr = crate::mem::vaddr(unsafe { self.txb.add(slot) });
        self.dev.tx_hdr(slot, addr)
    }
    /// Post a frame (slot, n bytes from the buffer's start) into the transmit queue WITHOUT a call.
    pub fn tx_send(&mut self, slot: usize, n: usize) {
        let addr = crate::mem::vaddr(unsafe { self.txb.add(slot) });
        self.tx_n += 1; self.tx_b += n as u64;
        self.dev.tx_post(slot, addr, n);
    }
    /// Ring the device about everything accumulated during the pass (§7.2): the TX tail,
    /// the replenishment of the RX ring, the CQ heads. An MMIO write is the most expensive operation of the
    /// path (on Nitro and under a virtio kick), so it is one per batch rather than per frame.
    /// It returns true if a transmit call really went to the device (the metric of the batch's
    /// amortisation: with virtio's EVENT_IDX/poll-host it is often suppressed).
    pub fn flush(&mut self) -> bool { self.dev.flush() }
    /// How many MMIO calls the driver made (None — a card without a counter).
    pub fn db_writes(&self) -> Option<u64> { self.dev.doorbells() }
    /// The frames and bytes received since the card was brought up. For an operator it answers the question
    /// that is otherwise settled with a sniffer: is the card silent or is the address wrong.
    pub fn rx_counters(&self) -> (u64, u64) { (self.rx_n, self.rx_b) }
    /// The frames and bytes transmitted since the card was brought up.
    pub fn tx_counters(&self) -> (u64, u64) { (self.tx_n, self.tx_b) }
    /// §7.5: allow an interrupt from the card (before going to sleep).
    /// §1.2: with virtio "to allow" means clearing NO_INTERRUPT in the avail ring
    /// (`set_dev_notify(true)`): under a hypervisor without MONITOR/MWAIT there is
    /// nothing else to wake a sleeper with. The insurance in both cases is the timer kwait_bounded.
    pub fn arm_intr(&mut self) { self.dev.arm_intr(); }
    pub fn ack_intr(&mut self) { self.dev.ack_intr(); }
    pub fn irq_vector(&mut self) -> Option<u8> { self.dev.irq_vector() }
    /// §7.5/§1.2: what to sleep on when idle. With ENA it is the head of the receive CQ, with virtio it is
    /// the word (flags,idx) of the receive used ring: the device writes there when completing
    /// a buffer, and that DMA write wakes MWAIT without an interrupt and without a vmexit.
    pub fn idle_watch(&self) -> Option<(*const u32, u32)> { self.dev.idle_watch() }
    pub fn rx_ready(&self) -> Option<bool> { self.dev.rx_ready() }
    /// The card's MAC (for ARP/the cell's headers).
    pub fn mac(&self) -> [u8; 6] { self.dev.mac() }
    /// The driver's name under the rings (for the log and the gate).
    pub fn drv(&self) -> &'static str { self.dev.name() }
}

pub struct Telemetry {
    pub echoed: u64,
    pub c_sum: u64,
    pub c_min: u64,
    pub c_max: u64,
}

struct Shared {
    q: NicQ,                // §7.4: the rings on top of any card (virtio/ENA)
    last_rx: u64,
    tx_stall: u64,          // how many frames were dropped because of unfinished transmissions
    tele: Telemetry,
}

/// The rings on top of a card over the kernel's COMMON frame buffers — without smoltcp and without a stack.
/// They are needed by the device model's gate (`talaria.test=nicdev`) and by anyone who needs the card
/// "as is"; `SmolDev` below does exactly the same for smoltcp.
pub fn nicq(dev: &'static mut dyn NetDev) -> NicQ {
    let (rb, tb) = unsafe { (&mut *addr_of_mut!(RXB), &mut *addr_of_mut!(TXB)) };
    NicQ::new(dev, rb, tb)
}

pub struct SmolDev {
    sh: DevCell<Shared>,
    /// The card's interrupt status/acknowledge registers (`NetDev::intr_regs`), taken once:
    /// the line's handler clears the status through them without asking for the card.
    intr: Option<(usize, usize)>,
    /// The receive interrupt's unmask register (`NetDev::arm_reg`), taken once: `arm_intr`
    /// writes it without asking for the card.
    arm: Option<(usize, u32)>,
}

impl SmolDev {
    pub fn new(dev: &'static mut dyn NetDev) -> Self { Self::new_n(dev, 0) }
    /// The same for THE CARD NUMBER `inst`: every card must have its own pool of frame
    /// buffers, otherwise two cards write DMA into one and the same memory (and that is not
    /// a "rare race" but guaranteed corruption on the very first packet).
    pub fn new_n(dev: &'static mut dyn NetDev, inst: usize) -> Self {
        let (rb, tb): (&'static mut [[u8; BUF]], &'static mut [[u8; BUF]]) = unsafe {
            if inst == 1 { (&mut *addr_of_mut!(RXB), &mut *addr_of_mut!(TXB)) }
            else { let k = if inst == 0 { 0 } else { (inst - 1).min(MAXEXTRA - 1) }; (&mut *addr_of_mut!(RXB2[k]), &mut *addr_of_mut!(TXB2[k])) }
        };
        let intr = dev.intr_regs();
        let arm = dev.arm_reg();
        SmolDev {
            intr,
            arm,
            sh: DevCell::new(Shared {
                q: NicQ::new(dev, rb, tb),
                last_rx: 0,
                tx_stall: 0,
                tele: Telemetry {
                    echoed: 0,
                    c_sum: 0,
                    c_min: u64::MAX,
                    c_max: 0,
                },
            }),
        }
    }

    /// Ring the device about what was accumulated during a pass of the stack (§7.2). Call it right
    /// after `iface.poll(...)` — then a ping-pong comes out as one TX+RX call
    /// instead of a call per frame and per buffer return.
    /// THE CALL DOES NOT GIVE UP (session 32). Previously there was a `try_take` here, and if the card
    /// was held at that moment by a neighbour (a network waiter from another core checks the ring
    /// `rx_ready` every ~0.8 us — by a microsecond lease), the call SILENTLY was not
    /// made: `tx_db_dirty` stayed raised until somebody's next
    /// pass, and at 7566 req/s that is the next request in ~100 us. From the outside it was
    /// a p99 plateau of mysql pk of 163 us with a p50 of 60 (Linux 83): the answer lay in the ring
    /// ready, the card was not told about it. We wait for the holder — it releases within fractions
    /// of a microsecond; a give-up is counted and printed (normally zero).
    pub fn flush(&mut self) {
        use core::sync::atomic::Ordering::Relaxed;
        let g = match self.sh.try_take() {
            Some(g) => g,
            None => {
                FLUSH_WAITED.fetch_add(1, Relaxed);
                let w0 = crate::tsc::rdtsc();
                let r = self.sh.take_bounded();
                crate::sched::DEV_WAIT.note_cycles(crate::tsc::rdtsc().wrapping_sub(w0),
                    crate::percpu::cpu_id() as u64 | (crate::sched::current_id().min(255) as u64) << 8 | 4 << 16);
                match r {
                    Some(g) => g,
                    None => { FLUSH_GAVEUP.fetch_add(1, Relaxed); note_borrow(false); return; }
                }
            }
        };
        let mut s = g;
        s.q.flush(); note_borrow(true); note_holder(4);
    }
    /// §7.5: allow an interrupt from the card (before going to sleep, and at the end of the
    /// line's handler). A card whose unmask is one register (the ENA) is armed directly:
    /// the `try_take` this was SKIPPED the unmask whenever a neighbour held the card — the
    /// handler's re-arm lost to any pass on another core, and the queue's interrupt stayed
    /// masked until the next pass that happened to receive (the ordering audit's note).
    /// Otherwise arming touches the rings and needs the card.
    pub fn arm_intr(&self) {
        if let Some((reg, v)) = self.arm {
            // SAFETY: the card's own register window, mapped as device memory for the life of
            // the kernel; the unmask write carries no state of the queues.
            unsafe { core::ptr::write_volatile(crate::mem::devptr::<u32>(reg as u64), v); }
            return;
        }
        if let Some(mut s) = self.sh.try_take() { s.q.arm_intr(); }
    }
    /// THE CLEARING DOES NOT DEPEND ON WHO HOLDS THE CARD. It used to be `try_take` and
    /// nothing else: when the handler interrupted a pass that held the card, the status was
    /// NOT cleared. On an edge-routed line that costs nothing; on the GIC the window's SPI is
    /// a level, it stayed raised, and the handler was entered again at once — 600 thousand
    /// times a second, the interrupted pass not getting one instruction (arm64 `diskserver`
    /// with `talaria.netirq=1`: one boot in three never answered). The two registers are
    /// independent of the queues, so they are written directly.
    pub fn ack_intr(&self) {
        if let Some((status, ack)) = self.intr {
            // SAFETY: the registers of the card's own window, mapped as device memory for the
            // whole life of the kernel; a read of the status and a write of the same bits back.
            unsafe {
                let v = core::ptr::read_volatile(crate::mem::devptr::<u32>(status as u64));
                if v != 0 { core::ptr::write_volatile(crate::mem::devptr::<u32>(ack as u64), v); }
            }
            return;
        }
        if let Some(mut s) = self.sh.try_take() { s.q.ack_intr(); }
    }
    pub fn irq_vector(&self) -> Option<u8> { self.sh.try_take().and_then(|mut s| s.q.irq_vector()) }
    /// §7.5: the address and the value of the receive ring's word for sleeping on a monitor.
    pub fn idle_watch(&self) -> Option<(*const u32, u32)> { self.sh.try_take().and_then(|s| s.q.idle_watch()) }
    /// Is there a received frame right now (see `NetDev::rx_ready`). It is asked
    /// WITHOUT the stack's lock, so a busy device answers None — and the caller
    /// falls back to the former path (a comparison of the ring's word).
    pub fn rx_ready(&self) -> Option<bool> { self.sh.try_take().and_then(|s| s.q.rx_ready()) }
    /// Collect the transmit completions from a check of the ring (session 32, see `NetDev::tx_watch`).
    pub fn tx_watch(&self) { if let Some(mut s) = self.sh.try_take() { s.q.tx_watch(); } }
    /// The number of calls to the device (the §7.1 diagnostics). try_borrow: the summary is printed by
    /// netprof straight from the datapath, where the RefCell is already taken for writing (otherwise a panic).
    pub fn db_writes(&self) -> Option<u64> { self.sh.try_take().and_then(|s| s.q.db_writes()) }
    /// The frames/bytes received by this card (the interface's diagnostics in `ip`).
    pub fn rx_counters(&self) -> (u64, u64) { self.sh.try_take().map(|s| s.q.rx_counters()).unwrap_or((0, 0)) }
    /// The frames/bytes transmitted and how many frames were DROPPED because of unfinished
    /// transmissions. The drops stand next to the transmissions on purpose: "tx 8 million" without them
    /// looks like health at exactly the moment when the ring has stopped
    /// being freed.
    pub fn tx_counters(&self) -> (u64, u64, u64) {
        self.sh.try_take().map(|s| { let (n, b) = s.q.tx_counters(); (n, b, s.tx_stall) }).unwrap_or((0, 0, 0))
    }
    /// The driver's name and the counters UNDER ONE BORROW (see `netsys::if_snapshot`).
    pub fn snapshot(&self) -> Option<(&'static str, u64, u64, u64, u64, u64)> {
        let s = self.sh.try_take()?;
        let (rn, rb) = s.q.rx_counters();
        let (tn, tb) = s.q.tx_counters();
        Some((s.q.drv(), rn, rb, tn, tb, s.tx_stall))
    }
    /// The driver's name under the interface (`netstat`: "the card came up BY WHAT").
    pub fn driver(&self) -> &'static str { self.sh.try_take().map(|s| s.q.drv()).unwrap_or("?") }

    pub fn take_stats(&self) -> Telemetry {
        let Some(mut sh) = self.sh.try_take()
            else { return Telemetry { echoed: 0, c_sum: 0, c_min: 0, c_max: 0 } };
        core::mem::replace(
            &mut sh.tele,
            Telemetry {
                echoed: 0,
                c_sum: 0,
                c_min: u64::MAX,
                c_max: 0,
            },
        )
    }
}

pub struct SmolRx<'a> {
    sh: &'a DevCell<Shared>,
    slot: usize,
    hdr: usize,
    len: usize,
}

impl<'a> RxToken for SmolRx<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        // We do NOT hold the borrow across f: inside this same callback smoltcp
        // takes a TxToken (the answer to a packet), and that one does a borrow_mut -> a RefCell
        // panic and a silent stack. We take the buffer's raw address and release it.
        note_holder(2);
        let p = match self.sh.take_bounded() { Some(sh) => sh.q.rx_ptr(self.slot), None => return f(&[]) };
        let r = f(unsafe { core::slice::from_raw_parts(p.add(self.hdr), self.len) });
        if let Some(mut sh) = self.sh.take_bounded() { sh.q.rx_done(self.slot); }
        r
    }
}

pub struct SmolTx<'a> {
    sh: &'a DevCell<Shared>,
}

impl<'a> TxToken for SmolTx<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        note_holder(3);
        let Some(mut sh) = self.sh.take_bounded() else {
            let mut tmp = [0u8; BUF];
            return f(&mut tmp[..len]);   // the card is unavailable — we lose the frame, the machine lives
        };
        // find a free tx slot, reaping completions as needed
        // We wait for a free transmit slot for A LIMITED time: if the device
        // has stopped giving out completions, an eternal loop here would hang the whole core
        // (it was caught on EC2: after a burst the stack went silent entirely, including ARP).
        // It is better to drop a frame — the stack will survive, and the reason will be visible in a counter.
        let mut slot = None;
        for i in 0..100_000u32 {
            // Waiting for a release WITHOUT having rung about the frames already posted is a sure
            // deadlock in batch mode (§7.2): the device simply will not take them.
            if i == 0 || i % 64 == 0 { let _ = sh.q.flush(); }
            sh.q.reap_tx();
            if let Some(x) = sh.q.tx_alloc() { slot = Some(x); break; }
            if i == 99_999 { sh.tx_stall += 1; }
            core::hint::spin_loop();
        }
        let slot = match slot {
            Some(s) => s,
            None => {
                if sh.tx_stall == 1 || sh.tx_stall % 1000 == 0 {
                    crate::println!("l1: the TX ring is not being freed (there are no completions), drops={}", sh.tx_stall);
                }
                let mut tmp = [0u8; BUF];
                return f(&mut tmp[..len]);
            }
        };
        let hdr = sh.q.tx_hdr(slot);
        let r = f(&mut sh.q.tx_buf(slot)[hdr..hdr + len]);
        sh.q.tx_send(slot, hdr + len);
        crate::netprof::mark_tx(); // §7.1
        if sh.last_rx != 0 {
            let c = tsc::rdtsc() - sh.last_rx;
            sh.last_rx = 0;
            sh.tele.echoed += 1;
            sh.tele.c_sum += c;
            if c < sh.tele.c_min {
                sh.tele.c_min = c;
            }
            if c > sh.tele.c_max {
                sh.tele.c_max = c;
            }
        }
        r
    }
}


/// ── THE OWNERSHIP OF THE CARD ───────────────────────────────────────────────
///
/// It was a `RefCell`, and that turned out to be the wrong instrument. The mutual exclusion
/// around the card is provided by the network stack's lock (`netsys`), while the `RefCell` stood
/// as insurance for the case of a path past the lock: the datapath honestly skips a refused borrow.
/// But a `RefCell` has a property that is deadly here: its counter
/// is released ONLY by dropping the guard, that is, only if the owner reaches
/// the end. A task that disappeared inside a pass of the stack takes the ownership of the card away
/// FOREVER — and from the outside that looks like "the server died": the process is alive, the listener is
/// in place, there are as many tasks as before, while the frames do not go. That is exactly how
/// PostgreSQL behaved under load; it was caught by the counters (`succeeded` froze, `busy`
/// grew by millions) and by the owner's record (the last to take it was a pass of the stack).
///
/// So the ownership here is arranged in the same way as the stack's lock: THE OWNER lies in
/// the word, and if the owner is dead, the next claimant takes the card for itself.
/// We never touch a live owner — preempting it is forbidden by the lock itself.
pub struct DevCell<T> {
    v: core::cell::UnsafeCell<T>,
    /// 0 — free; otherwise the number of the owning task + 1.
    owner: core::sync::atomic::AtomicU64,
}
unsafe impl<T> Sync for DevCell<T> {}

pub struct DevGuard<'a, T> { c: &'a DevCell<T> }

impl<T> DevCell<T> {
    pub const fn new(v: T) -> Self {
        Self { v: core::cell::UnsafeCell::new(v), owner: core::sync::atomic::AtomicU64::new(0) }
    }
    /// Take the card. `None` — it is busy with a LIVE owner (we skip the pass).
    pub fn try_take(&self) -> Option<DevGuard<'_, T>> {
        use core::sync::atomic::Ordering::{AcqRel, Acquire};
        let me = crate::sched::current_id() as u64 + 1;
        if self.owner.compare_exchange(0, me, AcqRel, Acquire).is_ok() {
            return Some(DevGuard { c: self });
        }
        let o = self.owner.load(Acquire);
        // The same owner means a NESTED entry (a pass of the stack inside a pass):
        // we refuse, as the RefCell refused, otherwise two guards will release the word
        // twice.
        if o == 0 || o == me { return None; }
        if crate::sched::task_dead((o - 1) as usize)
            && self.owner.compare_exchange(o, me, AcqRel, Acquire).is_ok()
        {
            STOLEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return Some(DevGuard { c: self });
        }
        None
    }
}

impl<'a, T> core::ops::Deref for DevGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T { unsafe { &*self.c.v.get() } }
}
impl<'a, T> core::ops::DerefMut for DevGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T { unsafe { &mut *self.c.v.get() } }
}
impl<'a, T> Drop for DevGuard<'a, T> {
    fn drop(&mut self) { self.c.owner.store(0, core::sync::atomic::Ordering::Release); }
}

impl<T> DevCell<T> {
    /// Take the card, letting a live owner finish speaking. The receive and transmit tokens
    /// (they are called by smoltcp inside a pass of the stack) must get the card: they already have
    /// a parsed frame in their hands. The former `RefCell` PANICKED in this place,
    /// that is, a conflict cost the machine; here it is a bounded wait, and when it is
    /// exhausted a refusal that the caller handles by losing ONE frame.
    pub fn take_bounded(&self) -> Option<DevGuard<'_, T>> {
        for _ in 0..100_000u32 {
            if let Some(g) = self.try_take() { return Some(g); }
            core::hint::spin_loop();
        }
        if !TOKEN_TOLD.swap(true, core::sync::atomic::Ordering::Relaxed) {
            crate::println!("l1: ATTENTION: a frame's token did not wait for the card — the frame is lost (there used to be a RefCell panic here)");
        }
        None
    }
}
static TOKEN_TOLD: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// How many times the card had to be TAKEN AWAY from a vanished owner. Normally zero;
/// a non-zero one means that a task disappeared inside the network stack, and that is worth
/// analysing separately — but the machine keeps working meanwhile.
pub static STOLEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// THE STATE OF THE CARD'S BORROW — the datapath's counters.
///
/// What for. The mutual exclusion around the card is a `RefCell`, and the datapath honestly
/// skips a refused borrow (a race for the stack must cost a cycle, not the machine).
/// But the skip is MUTE, and if the borrow somehow does not get released, the picture
/// from the outside will be this: the server is alive, the listener is in place, there are as many tasks as before — while
/// the frames do not go. Exactly that was observed with PostgreSQL: `kstat net` printed
/// `net0 (?)` with zeroes (that is precisely a failed `try_borrow`) in TWO snapshots in a row a
/// minute apart, and it was impossible to understand from the report that the card was unavailable —
/// the zeroes read as "there is no traffic".
///
/// Now that has a name and a number: how many passes of the datapath went through and how many
/// ran into a busy borrow, and when the last successful one was.
pub static DEV_BORROW_OK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Session 32: how many times a call to the card / the reception of a frame under a pass WAITED for
/// the card's microsecond holder and how many times they did not wait it out (gave up).
pub static FLUSH_WAITED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static FLUSH_GAVEUP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static RX_TAKE_WAITED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static RX_TAKE_GAVEUP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DEV_BORROW_BUSY: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The TSC of the last SUCCESSFUL borrow by the datapath (0 — not a single one yet).
pub static DEV_BORROW_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// We have already complained about the sticking — we do not repeat it a second time.
static STUCK_TOLD: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// WHO took the borrow last: the task's number, the core's number and a mark of the place.
/// Without it "it stuck" is a statement rather than a diagnosis: there are three places that take the card,
/// and they are on different paths (a pass of the stack, the reception of a frame, the transmission of a frame).
pub static DEV_LAST_TASK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(u64::MAX);
pub static DEV_LAST_CPU: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(u64::MAX);
pub static DEV_LAST_SITE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The marks of the places: 1 — a pass of the stack (`receive`), 2 — the reception of a frame (`RxToken`),
/// 3 — the transmission of a frame (`TxToken`), 4 — a call to the device (`flush`).
#[inline]
pub fn note_holder(site: u64) {
    use core::sync::atomic::Ordering::Relaxed;
    DEV_LAST_TASK.store(crate::sched::current_id() as u64, Relaxed);
    DEV_LAST_CPU.store(crate::percpu::cpu_id() as u64, Relaxed);
    DEV_LAST_SITE.store(site, Relaxed);
}

/// How many frames were received SINCE THE BOOT (cumulatively, without a reset). Unlike
/// `rx_counters` (those are reset in order to count THE RATE), this counter only
/// grows: by it a pass of the stack understands whether it brought a frame.
pub static RX_SEEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
#[inline] pub fn rx_seen() -> u64 { RX_SEEN.load(core::sync::atomic::Ordering::Relaxed) }

/// Mark the outcome of the card's borrow in the datapath and complain once if
/// it is not released for longer than a second.
#[inline]
fn note_borrow(ok: bool) {
    use core::sync::atomic::Ordering::Relaxed;
    let now = tsc::rdtsc();
    if ok {
        DEV_BORROW_OK.fetch_add(1, Relaxed);
        DEV_BORROW_TSC.store(now, Relaxed);
        return;
    }
    DEV_BORROW_BUSY.fetch_add(1, Relaxed);
    let last = DEV_BORROW_TSC.load(Relaxed);
    if last == 0 { return; }
    let hz = crate::tsc::calibrate_cached();
    if hz != 0 && now.wrapping_sub(last) > hz && !STUCK_TOLD.swap(true, Relaxed) {
        crate::println!("l1: ATTENTION: the card has been unavailable to the datapath for more than a second — the borrow is not released \
(the last to take it: task {}, core {}, place {} [1=a pass of the stack, 2=the reception of a frame, 3=the transmission of a frame, 4=a call]). \
The frames do not go: from the outside that looks like \"the server died\", although the workload is alive.",
            DEV_LAST_TASK.load(Relaxed), DEV_LAST_CPU.load(Relaxed), DEV_LAST_SITE.load(Relaxed));
    }
}

impl Device for SmolDev {
    type RxToken<'a>
        = SmolRx<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = SmolTx<'a>
    where
        Self: 'a;

    fn receive(&mut self, _t: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let (slot, hdr, len) = {
            // try rather than borrow_mut: a borrow conflict here is a race
            // FOR THE STACK, and the right reaction to it is to skip the pass (the frame
            // will be taken by the next one) rather than to bring THE KERNEL down with a RefCell panic. The mutual
            // exclusion is provided by the netsys lock; this is insurance for the case of
            // a path that will yet be found past the lock: observation and diagnostics
            // have no right to cost the machine.
            // ...but first WAIT OUT the microsecond holder (a check of the ring
            // by a neighbour, the collection of completions): otherwise a pass under the stack's lock
            // went PAST a frame, and the frame waited for the next pass (session 32).
            let mut sh = match self.sh.try_take() {
                Some(g) => g,
                None => {
                    RX_TAKE_WAITED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    let w0 = crate::tsc::rdtsc();
                    let r = self.sh.take_bounded();
                    crate::sched::DEV_WAIT.note_cycles(crate::tsc::rdtsc().wrapping_sub(w0),
                        crate::percpu::cpu_id() as u64 | (crate::sched::current_id().min(255) as u64) << 8 | 1 << 16);
                    match r {
                        Some(g) => g,
                        None => { RX_TAKE_GAVEUP.fetch_add(1, core::sync::atomic::Ordering::Relaxed); note_borrow(false); return None; }
                    }
                }
            };
            note_borrow(true); note_holder(1);
            sh.q.reap_tx();
            let (slot, hdr, len) = sh.q.poll_rx()?;
            sh.last_rx = tsc::rdtsc();
            // Cumulatively, unlike `rx_n` (that one is reset for the rate):
            // by it a pass of the stack understands whether it BROUGHT a frame, and only then
            // looks for the sockets that became ready (see netsys::deliver_ready).
            RX_SEEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::netprof::mark_rx(); // §7.1
            (slot, hdr, len)
        };
        Some((
            SmolRx {
                sh: &self.sh,
                slot,
                hdr,
                len,
            },
            SmolTx { sh: &self.sh },
        ))
    }

    fn transmit(&mut self, _t: Instant) -> Option<Self::TxToken<'_>> {
        Some(SmolTx { sh: &self.sh })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        // ATTENTION (§7.2): in smoltcp this is NOT "packets per poll" but the ceiling of the
        // announced TCP window: window <= max_burst_size * MSS. With the value 1
        // every incoming TCP connection worked in the mode "one segment — one
        // RTT" (~1.4 KB per round trip). We set it by the number of receive buffers: the window will be limited by
        // the socket's buffer rather than by the card, and the peer still will not be able to overflow the ring.
        caps.max_burst_size = Some(NRX);
        caps.medium = Medium::Ethernet;
        // We do compute the UDP checksum: a zero one is formally lawful, but it switches off
        // the fast verification paths on the receiving side, while it costs tens of nanoseconds on a
        // 64-byte frame.
        caps.checksum.udp = Checksum::Both;
        caps
    }
}

fn now(tsc_hz: u64) -> Instant {
    let t = tsc::rdtsc();
    let us = (t / tsc_hz) * 1_000_000 + (t % tsc_hz) * 1_000_000 / tsc_hz;
    Instant::from_micros(us as i64)
}

static mut SOCK_STORE: [SocketStorage; 2] = [SocketStorage::EMPTY; 2];
static mut RX_META: [udp::PacketMetadata; 16] = [udp::PacketMetadata::EMPTY; 16];
static mut TX_META: [udp::PacketMetadata; 16] = [udp::PacketMetadata::EMPTY; 16];
static mut RX_PAYLOAD: [u8; 32768] = [0; 32768];
static mut TX_PAYLOAD: [u8; 32768] = [0; 32768];
static mut TCP_RX: [u8; 16384] = [0; 16384];
static mut TCP_TX: [u8; 16384] = [0; 16384];

/// A kernel-mode HTTP server on TCP :80 (smoltcp on top of virtio). A check of the TCP stack.
pub fn run_httpd(dev: &'static mut dyn NetDev, mac: [u8; 6], tsc_hz: u64) -> ! {
    // SmolDev is in a static: inside there is a NicQ with virtio rings, it does not fit on an AP's stack
    static mut SDEV: Option<SmolDev> = None;
    let sdev = unsafe {
        core::ptr::write(core::ptr::addr_of_mut!(SDEV), Some(SmolDev::new(dev)));
        (*core::ptr::addr_of_mut!(SDEV)).as_mut().unwrap()
    };
    let cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    let mut iface = Interface::new(cfg, sdev, now(tsc_hz));
    iface.update_ip_addrs(|a| { a.push(IpCidr::new(IpAddress::v4(10,0,0,2), 24)).unwrap(); });
    let mut sockets = unsafe { SocketSet::new(&mut *addr_of_mut!(SOCK_STORE) as &mut [_]) };
    let tcp_sock = unsafe { tcp::Socket::new(
        tcp::SocketBuffer::new(&mut *addr_of_mut!(TCP_RX) as &mut [u8]),
        tcp::SocketBuffer::new(&mut *addr_of_mut!(TCP_TX) as &mut [u8])) };
    let h = sockets.add(tcp_sock);
    sockets.get_mut::<tcp::Socket>(h).listen(80).unwrap();
    println!("talaria: an HTTP server on TCP :80 (kernel-mode, smoltcp)");

    const BODY: &str = "Hello from Talaria OS kernel httpd!\n";
    // A full keep-alive answer (Connection: keep-alive), the body is 35 bytes.
    fn build_resp(out: &mut [u8]) -> usize {
        let hdr = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: keep-alive\r\nContent-Length: ";
        let mut n = 0;
        for &b in hdr { out[n]=b; n+=1; }
        let cl = BODY.len();
        for d in [(cl/10) as u8, (cl%10) as u8] { out[n]=b'0'+d; n+=1; }
        for &b in b"\r\n\r\n" { out[n]=b; n+=1; }
        for &b in BODY.as_bytes() { out[n]=b; n+=1; }
        n
    }
    let mut acc = [0u8; 4096];
    let mut acc_len = 0usize;
    let mut reqs: u64 = 0;
    let mut conns: u64 = 0;
    loop {
        let ts = now(tsc_hz);
        iface.poll(ts, sdev, &mut sockets);
        sdev.flush(); // §7.2
        let s = sockets.get_mut::<tcp::Socket>(h);
        if s.may_recv() {
            // read what is available into acc
            let free = acc.len() - acc_len;
            if free > 0 {
                let got = s.recv(|buf| { let n = buf.len().min(free); (n, (n, buf.as_ptr())) });
                if let Ok((n, ptr)) = got {
                    unsafe { core::ptr::copy_nonoverlapping(ptr, acc.as_mut_ptr().add(acc_len), n); }
                    acc_len += n;
                }
            }
            // process all the complete requests (the boundary \r\n\r\n) — keep-alive
            loop {
                let mut end = None;
                if acc_len >= 4 {
                    for i in 0..=acc_len - 4 {
                        if &acc[i..i+4] == b"\r\n\r\n" { end = Some(i + 4); break; }
                    }
                }
                match end {
                    Some(e) if s.can_send() => {
                        let mut resp = [0u8; 512];
                        let n = if crate::ctlplane::is_ctl(&acc[..e]) {
                            crate::ctlplane::build_response(&acc[..e], &mut resp) // A3.1: an authenticated /ctl
                        } else { build_resp(&mut resp) };
                        s.send_slice(&resp[..n]).ok();
                        acc.copy_within(e..acc_len, 0);
                        acc_len -= e;
                        reqs += 1;
                        if reqs <= 3 { println!("talaria: the HTTP keep-alive request #{} has been served", reqs); }
                    }
                    _ => break,
                }
            }
        }
        if !s.is_open() {
            acc_len = 0;
            conns += 1;
            if conns <= 3 { println!("talaria: the connection is closed, re-listen (keep-alive, requests in total={})", reqs); }
            s.listen(80).ok();
        }
    }
}


pub fn run(dev: &'static mut dyn NetDev, mac: [u8; 6], tsc_hz: u64) -> ! {
    static mut SDEV2: Option<SmolDev> = None;
    let sdev = unsafe {
        core::ptr::write(core::ptr::addr_of_mut!(SDEV2), Some(SmolDev::new(dev)));
        (*core::ptr::addr_of_mut!(SDEV2)).as_mut().unwrap()
    };

    let cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    let mut iface = Interface::new(cfg, sdev, now(tsc_hz));
    iface.update_ip_addrs(|a| {
        a.push(IpCidr::new(IpAddress::v4(10, 0, 0, 2), 24)).unwrap();
    });

    let mut sockets = unsafe { SocketSet::new(&mut *addr_of_mut!(SOCK_STORE) as &mut [_]) };
    let socket = unsafe {
        udp::Socket::new(
            udp::PacketBuffer::new(
                &mut *addr_of_mut!(RX_META) as &mut [_],
                &mut *addr_of_mut!(RX_PAYLOAD) as &mut [u8],
            ),
            udp::PacketBuffer::new(
                &mut *addr_of_mut!(TX_META) as &mut [_],
                &mut *addr_of_mut!(TX_PAYLOAD) as &mut [u8],
            ),
        )
    };
    let handle = sockets.add(socket);
    sockets
        .get_mut::<udp::Socket>(handle)
        .bind(7777)
        .expect("bind");

    println!("talaria: L1 smoltcp up, udp echo :7777");

    let interval = 5 * tsc_hz;
    let gap_thresh = tsc_hz / 100_000; // 10 us
    let mut gap_max: u64 = 0;
    let mut gap_over: u64 = 0;
    let mut prev = tsc::rdtsc();
    let mut last_report = prev;
    let mut echo_buf = [0u8; 2048];

    loop {
        let t = tsc::rdtsc();
        let gap = t - prev;
        if gap > gap_max {
            gap_max = gap;
        }
        if gap > gap_thresh {
            gap_over += 1;
        }
        prev = t;

        let ts = now(tsc_hz);
        iface.poll(ts, sdev, &mut sockets);
        sdev.flush(); // §7.2

        let s = sockets.get_mut::<udp::Socket>(handle);
        loop {
            let (n, meta) = match s.recv() {
                Ok((data, meta)) => {
                    let n = data.len();
                    echo_buf[..n].copy_from_slice(data);
                    (n, meta)
                }
                Err(_) => break,
            };
            let _ = s.send_slice(&echo_buf[..n], meta);
        }
        // flush replies in the same iteration
        iface.poll(ts, sdev, &mut sockets);
        sdev.flush(); // §7.2

        if t - last_report >= interval {
            let to_ns = |c: u64| c * 1_000_000_000 / tsc_hz;
            let st = sdev.take_stats();
            if st.echoed > 0 {
                println!(
                    "stats[l1]: pkts={} rx->tx ns min/avg/max = {}/{}/{} loop_gap_max_ns={} gaps>10us={}",
                    st.echoed,
                    to_ns(st.c_min),
                    to_ns(st.c_sum / st.echoed),
                    to_ns(st.c_max),
                    to_ns(gap_max),
                    gap_over
                );
            } else {
                println!(
                    "stats[l1]: idle loop_gap_max_ns={} gaps>10us={}",
                    to_ns(gap_max),
                    gap_over
                );
            }
            gap_max = 0;
            gap_over = 0;
            last_report = tsc::rdtsc();
            prev = last_report;
        }

        core::hint::spin_loop();
    }
}
