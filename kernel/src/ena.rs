//! The driver of the AWS ENA (Elastic Network Adapter) — PLAN-V1.1 §4.4, POLLING-FIRST.
//!
//! This is the EC2 (Nitro) network card: without it Talaria on an instance is mute. The
//! minimum for the datapath is implemented: a readless read of the registers, a reset of the controller, the admin
//! queue (AQ/ACQ/AENQ), a GET_FEATURE (the MAC/the MTU/the queues' limits), the creation of one
//! pair of IO queues per direction (a TX SQ+CQ, an RX SQ+CQ) in the host's memory and the descriptor
//! rings. There are NO interrupts: the CQ is polled by the phase bit, as in the rest of
//! Talaria's datapath (the reactor polls anyway). The LLQ (the descriptors in the device's
//! memory) is the next step, here placement_policy=HOST.
//!
//! The spec: the open headers of amzn-drivers/ena_com (ena_regs_defs.h,
//! ena_admin_defs.h, ena_eth_io_defs.h) — the structures are duplicated here as
//! #[repr(C)] in the same order of the fields.
//!
//! THE NITRO REQUIREMENTS found out on a live c6i.large (see POC-RESULTS v1.1):
//! the firmware rejects the creation of ANY IO queues (CREATE_CQ/CREATE_SQ ->
//! UNKNOWN_ERROR, without a clarification) until the driver has introduced itself:
//!   1) a SET_FEATURE(HOST_ATTR_CONFIG) goes as THE FIRST command after the admin init;
//!      before it the device returns a reduced set of features too (LLQ/RSS -> UNSUPPORTED);
//!   2) the host_info must have A DRIVER VERSION of the level of a modern ena
//!      (2.x): with 1.1.0 the queues were not created, with 2.13.0 they were created at once
//!      (and supported_features grew: 0x142059f6 -> 0x14205df6);
//!   3) the function's MSI-X is on (the vectors may be masked — the datapath
//!      polls anyway), the rings' depth is 512/1024 and the alignment is by the size.
//! The diagnostics: a kprobe on ena_com_execute_admin_command of a live Linux driver
//! (the same instance type) and a byte-by-byte comparison of the commands.
//!
//! The interface outside is the same methods as virtio-net's in l1.rs (receive_begin/
//! poll_receive/receive_complete/transmit_begin/poll_transmit/transmit_complete),
//! so that netsys/smoltcp do not know which card is under them (l1::Nic).
use core::sync::atomic::{fence, Ordering};

// ── the registers (BAR0), ena_regs_defs.h ──
const REG_VERSION: u64 = 0x00;
const REG_CONTROLLER_VERSION: u64 = 0x04;
const REG_CAPS: u64 = 0x08;
const REG_AQ_BASE_LO: u64 = 0x10;
const REG_AQ_BASE_HI: u64 = 0x14;
const REG_AQ_CAPS: u64 = 0x18;
const REG_ACQ_BASE_LO: u64 = 0x20;
const REG_ACQ_BASE_HI: u64 = 0x24;
const REG_ACQ_CAPS: u64 = 0x28;
const REG_AQ_DB: u64 = 0x2c;
const REG_AENQ_CAPS: u64 = 0x34;
const REG_AENQ_BASE_LO: u64 = 0x38;
const REG_AENQ_BASE_HI: u64 = 0x3c;
const REG_AENQ_HEAD_DB: u64 = 0x40;
const REG_INTR_MASK: u64 = 0x4c;
const REG_DEV_CTL: u64 = 0x54;
const REG_DEV_STS: u64 = 0x58;
const REG_MMIO_REG_READ: u64 = 0x5c;
const REG_MMIO_RESP_LO: u64 = 0x60;
const REG_MMIO_RESP_HI: u64 = 0x64;

const DEV_STS_READY: u32 = 1;
const DEV_STS_RESET_IN_PROGRESS: u32 = 1 << 3;
const DEV_CTL_DEV_RESET: u32 = 1;
const CAPS_RESET_TIMEOUT_SHIFT: u32 = 1;
const CAPS_RESET_TIMEOUT_MASK: u32 = 0x3e;

// ── the admin opcodes and the feature ids (ena_admin_defs.h) ──
const OP_CREATE_SQ: u8 = 1;
const OP_CREATE_CQ: u8 = 3;
const OP_GET_FEATURE: u8 = 8;
const OP_SET_FEATURE: u8 = 9;
const FEAT_DEVICE_ATTRIBUTES: u8 = 1;
const FEAT_MAX_QUEUES_NUM: u8 = 2;
const FEAT_LLQ: u8 = 4;
const FEAT_MAX_QUEUES_EXT: u8 = 7;
const FEAT_MTU: u8 = 14;
const FEAT_AENQ_CONFIG: u8 = 26;
const FEAT_HOST_ATTR_CONFIG: u8 = 28;
// §4.4: the PHC is the device's clock (AWS Time Sync PTP through ENA). The presence of the feature is
// bit 29 in the device descriptor's supported_features.
const FEAT_PHC_CONFIG: u8 = 29;
// §4.5 (A1): RSS. A kprobe diff with a live Linux driver (a down/up of ens5 on the same
// instance type) showed that AFTER creating the queues it configures RSS —
// the redirection table (id 12) and the hash function (id 10) — while we never did that.
// The only divergence of the sequence apart from the number of queues.
const FEAT_RSS_HASH_FUNCTION: u8 = 10;
const FEAT_RSS_IND_TABLE: u8 = 12;
const RSS_IND_ENTRIES: usize = 128;      // as with Linux: size = log2(128) = 7
const RSS_FUNC_TOEPLITZ: u32 = 1 << 1;   // selected_func = 1 << ENA_ADMIN_TOEPLITZ
const PHC_TYPE_READLESS: u32 = 0;

// the depths/sizes
const AQ_DEPTH: u16 = 32;      // ENA_ADMIN_QUEUE_DEPTH
const AENQ_DEPTH: u16 = 16;    // ENA_ASYNC_QUEUE_DEPTH
const AQ_ENTRY: usize = 64;
const ACQ_ENTRY: usize = 64;
const AENQ_ENTRY: usize = 64;
// The depth of the device's rings. IMPORTANT: Nitro rejects (status=6, without a clarification)
// the creation of queues with a "non-standard" depth — a live Linux ena driver on the same
// instance sends 512/1024, and only such values are accepted; 128/16 and even
// 1024 on a 4K-aligned buffer were rejected. A ring is allocated with headroom and
// aligned by its own full size (dma_alloc_coherent in Linux gives a
// natural alignment — the device counts on it).
pub const Q_DEPTH: u16 = 512;
// §4.5 (A1), step 3: the depth of the RECEIVE rings. A byte-by-byte comparison with a live
// Linux driver showed that its CREATE_CQ/CREATE_SQ commands differ from
// ours EXACTLY by the depth: it has an RX of 1024, we had 512 (the TX is 512 for both).
// The hypothesis: the device prefetches the receive descriptors in batches, and on a small
// ring the fetching is not amortised.
pub const RX_DEPTH: u16 = 512;   // 1024 WAS TRIED: the RTT 89 -> 98 us, the hypothesis is refuted
pub const TX_RING: u16 = 128;
/// THE LATENCY OF A TRANSMISSION'S COMPLETION (session 32): from the posting of a frame into the ring to
/// the writing of a completion by the device, us — the buckets <5 <10 <20 <50 <100 <150
/// <200 200+. It is taken with an accuracy of the moment when somebody read it
/// (`tx_watch` from a check of the ring by a waiter — about a microsecond with a spin).
/// A request's tail that is in none of the server's intervals must be either
/// before the pass (the frame is unnoticed in the ring) or after the call — here it is the second.
pub static TXC_HIST: [core::sync::atomic::AtomicU64; 8] = [const { core::sync::atomic::AtomicU64::new(0) }; 8];
pub static TXC_MAX_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);  // that many frame buffers we keep in flight (slot/req_id)
pub const RX_RING: u16 = 256;
const TX_DESC_SZ: usize = 16;  // ena_eth_io_tx_desc
const RX_DESC_SZ: usize = 16;  // ena_eth_io_rx_desc
const TX_CDESC_SZ: usize = 8;  // ena_eth_io_tx_cdesc
const RX_CDESC_SZ: usize = 16; // ena_eth_io_rx_cdesc_base
pub const BUF: usize = 2048;   // a frame's buffer (an MTU of 1500 + headroom)

// The ENA's PCI ids (ena_pci_id_tbl.h): the PF 0xec20, the VF 0xec21 (+ the old 0x0ec2/0x1ec2)
const ENA_VENDOR: u16 = 0x1d0f;
const ENA_DEVICES: [u16; 4] = [0xec20, 0xec21, 0x0ec2, 0x1ec2];

// The frame buffers are shared with the virtio path (l1::rxb/txb): the address is passed from the outside.
// They are in the kernel's .bss (or the high area): the address is the KERNEL'S pointer, and
// the device is given `mem::kpa` of it (the descriptors, the LLQ's data part).

/// Print the full exchange of admin commands (the driver's diagnostics on a live Nitro).
pub static DBG: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// The LLQ (§7.3): the transmit descriptors and a frame's header are written INTO THE DEVICE'S
/// MEMORY (BAR2) rather than into the host's memory. The device does not need to fetch a
/// descriptor over PCIe — that is the regular path of the Linux driver on Nitro.
/// **It is ON by default** (session 6): together with accel_mode this configuration
/// gives an RTT of 34 us against Linux's 43 on the same stand — it is the production one.
/// `talaria.ena=nollq` — return the descriptors into the host's memory (an A/B).
pub static WANT_LLQ: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
/// §4.4: talaria.ena=phc — switch the device's clock (PHC) on if it is declared.
pub static WANT_PHC: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// §4.5: RSS as with the Linux driver. The bits: 1 — the redirection table,
/// 2 — the hash function (talaria.ena=rss — both, rsstbl / rsshash — separately).
pub static WANT_RSS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// §4.5: talaria.ena=hibuf — the FRAME buffers from the memory above 4 GB. The rings have already
/// been moved there (session 2: otherwise the device did not accept a CREATE_CQ), while the buffers
/// stayed in the kernel's .bss, i.e. in the first 32 MB. We check whether the DMA pays
/// for the low addresses.
pub static WANT_HIBUF: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// §4.6 (A1): talaria.ena=unmask — write the moderation/unmasking register at
/// THE END of every pass of the datapath, as Linux does at the end of a NAPI poll. A checkable
/// hypothesis of the gap: we keep all the MSI-X vectors masked and only
/// poll, while the device, not seeing an unmasking, may hold a completion back
/// until its own timer, considering the driver an interrupt-driven one.
pub static WANT_UNMASK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// §4.5 (A1): talaria.ena=mtu9001 — ask the device for an MTU of 9001, as Linux does
/// (taken from the device init of a live driver: a SET_FEATURE(MTU)=0x2329). We set
/// 1500 because the receive path uses one descriptor. THE DIAGNOSTICS: frames larger than the buffer
/// are not handled with this flag — it is good only for measurements with small
/// packets.
pub static WANT_MTU9001: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// §A1: talaria.ena=nowc — do NOT move the LLQ window into write-combining (leave
/// UC, as it was before session 6). Only for an A/B: the reference driver always uses WC.
pub static WANT_NOWC: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// A11: `talaria.ena=noreadless` — read the device's registers STRAIGHT from the BAR rather than
/// through the readless mechanism (a request into a register + an answer by DMA into our memory).
/// Readless was invented for the paravirtual platforms where an MMIO read is expensive; on
/// bare metal (a c6i.metal) the device does not answer such a request at all, while a direct
/// read of the BAR is a lawful path (the Linux driver does that too when readless
/// is off). The flag is set BY ITSELF too, at the first timeout.
pub static WANT_NOREADLESS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Readless has already been recognised as non-working on this machine (after the first timeout) —
/// otherwise every read would cost 20 ms of waiting for an answer that will not come.
static READLESS_DEAD: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// §A1: accel_mode LLQ exactly as at the reference: DISABLE_META_CACHING |
/// LIMIT_TX_BURST. The first one obliges us to put a meta descriptor into EVERY packet,
/// the second to ring not less often than once every max_tx_burst_size bytes of descriptors.
/// Without these flags the device works in the NON-accelerated LLQ mode ("driver needs
/// to support those requirements in order to use accelerated llq» —
/// ena_admin_defs.h), and that cost us THE WHOLE gap against Linux: 92 -> 34 us
/// of RTT on a c6i.large. **It is ON by default**; `talaria.ena=noaccel` removes it.
pub static WANT_ACCEL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
/// §A1: talaria.ena=meta — put a meta descriptor into every packet WITHOUT switching
/// accel_mode on. It separates two effects: the device caches the meta per queue, while we
/// did not send it once over the queue's whole life — perhaps for a packet without
/// a meta the firmware's slow path is switched on.
pub static WANT_META: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// §A1: talaria.ena=intr — remove the mask of the DEVICE's interrupts (REG_INTR_MASK=0)
/// and once, at the initialisation, allow both CQs to raise an interrupt with
/// zero moderation intervals. It differs from `unmask` in that in the datapath
/// NOTHING is written (that variant paid with an extra write into the BAR per every pass
/// and became worse). A checkable hypothesis: the device holds a completion back
/// while it considers that there is nowhere to deliver an interrupt.
pub static WANT_INTR: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// §A1: talaria.ena=nocqdb — do not ring the CQ's head. A modern ena_com does not make that
/// call at all (ena_com_update_dev_comp_head has been removed from the driver), while
/// we spend two writes into the BAR per pass on it.
pub static WANT_NOCQDB: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

// LLQ (ena_admin_defs.h)
const LLQ_HEADER_INLINE: u32 = 0x1;
const LLQ_ENTRY_128B: u32 = 0x1;
const LLQ_ENTRY_256B: u32 = 0x4;
const LLQ_DNBH_2: u32 = 0x2;
const LLQ_STRIDE_MULTIPLE: u32 = 0x2;
const LLQ_STRIDE_SINGLE: u32 = 0x1;
const PLACEMENT_HOST: u32 = 1;
const PLACEMENT_DEV: u32 = 3;
// accel_mode (enum ena_admin_accel_mode_feat): a bit field rather than values
const ACCEL_DISABLE_META_CACHING: u32 = 1 << 0;
const ACCEL_LIMIT_TX_BURST: u32 = 1 << 1;
// The meta descriptor's fields (ena_eth_io_tx_meta_desc.len_ctrl) — ena_com_create_meta
const META_EXT_VALID: u32 = 1 << 14;
const META_ETH_META_TYPE: u32 = 1 << 20;
const META_STORE: u32 = 1 << 21;
const META_DESC: u32 = 1 << 23;

pub struct Ena {
    regs: u64,
    // admin
    aq: u64, acq: u64, aenq: u64, mmio_resp: u64,
    aq_tail: u16, acq_head: u16, acq_phase: bool, aq_cmd_id: u16, aq_phase_flag: bool,
    aenq_head: u16, aenq_phase: bool,
    mmio_seq: u16,
    // io: TX
    tx_sq: u64, tx_cq: u64, tx_db: u64, tx_cq_db: u64,
    tx_tail: u16, tx_phase: bool, tx_cq_head: u16, tx_cq_phase: bool,
    tx_free: [bool; TX_RING as usize],
    tx_free_n: u16, tx_next: u16,        // the counter of the free slots + a cursor (without a linear scan)
    tx_post_tsc: [u64; TX_RING as usize], // the cycle of a frame's posting (session 32: the latency of a transmission's completion)
    // io: RX
    rx_sq: u64, rx_cq: u64, rx_db: u64, rx_cq_db: u64,
    // The register "the moderation intervals + an unmask" of every CQ (ena_eth_io_intr_reg).
    // While nothing is written into it, the device holds the completions by the default
    // of its own moderation — on a ping-pong that is tens of microseconds of waiting for nothing.
    rx_intr_reg: u64, tx_intr_reg: u64,
    /// The MSI-X vector of the IO queues (the index in the device's table; the IDT's vector is
    /// `idt::MSI_VECTOR_BASE + io_vec`). See `irq_vector`.
    io_vec: u32,
    rx_tail: u16, rx_phase: bool, rx_cq_head: u16, rx_cq_phase: bool,
    /// A SNAPSHOT OF THE RECEIVE CQ's HEAD FOR A READ WITHOUT THE LOCK: the head is in the low 16 bits,
    /// the phase in bit 16. It is needed by the sign "the card is silent": it is asked BEFORE the stack's
    /// lock, while the ordinary fields are being moved by a foreign pass at that moment.
    rx_watch: core::sync::atomic::AtomicU32,
    // §7.2 the deferred calls: the doorbell registers are written ONCE per batch rather than per frame
    tx_db_dirty: bool, tx_cq_dirty: bool, rx_db_dirty: bool, rx_cq_dirty: bool,
    pub db_writes: u64,                  // how many MMIO calls have been made (the §7.1 diagnostics)
    // §7.3 LLQ: the ring of descriptors in the device's memory
    llq_base: u64,                       // the ring's address in BAR2 (0 — the LLQ is off)
    llq_entry: usize,                    // the record's size (128 B)
    llq_hdr_max: usize,                  // how many bytes of a frame we put inline
    // §A1 accel_mode: the device does not cache the meta -> we send a meta descriptor in
    // every packet; and we ring not less often than once every llq_burst records.
    llq_meta: bool,
    llq_burst: u16, llq_burst_left: u16,
    // §4.4 PHC: the device's answer page + the call's offset in BAR0
    phc_out: u64,
    phc_db: u32,
    phc_req: u16,
    pub phc_ok: bool,
    pub mac: [u8; 6],
    pub mtu: u32,
    pub rx_pkts: u64,
    pub tx_pkts: u64,
    stat_tsc: u64,          // the mark of the last printing of the statistics
}

#[inline] fn w32(a: u64, v: u32) { unsafe { core::ptr::write_volatile(crate::mem::devptr::<u32>(a), v) } }
#[inline] fn r32_raw(a: u64) -> u32 { unsafe { core::ptr::read_volatile(crate::mem::devptr::<u32>(a)) } }

/// The meta_ctrl fields of a TX descriptor: the indices of the L3/L4 protocols. It tells the device
/// what kind of packet it is transmitting (the Linux driver always fills them in);
/// the checksums are still computed by the stack, we do not ask for an offload.
///   l3_proto_idx (3:0): 8 = IPv4, 11 = IPv6;  DF (4)
///   l4_proto_idx (12:8): 12 = TCP, 13 = UDP
fn meta_of(addr: u64, len: usize) -> u32 {
    if len < 34 { return 0; }
    let b = |i: usize| -> u8 { unsafe { core::ptr::read_volatile(crate::mem::vptr::<u8>(addr + i as u64)) } };
    if b(12) != 0x08 || b(13) != 0x00 { return 0; }        // it is not IPv4
    let ihl = (b(14) & 0xF) as usize * 4;
    if ihl < 20 || 14 + ihl > len { return 8; }            // IPv4 without parsing L4
    let l4 = match b(23) { 6 => 12u32, 17 => 13, _ => 0 }; // TCP / UDP
    let df = if b(20) & 0x40 != 0 { 1u32 << 4 } else { 0 };
    8 | df | (l4 << 8)
}

fn udelay(us: u64) {
    let hz = crate::tsc::hz();
    let t0 = crate::tsc::rdtsc();
    while crate::tsc::rdtsc() - t0 < hz * us / 1_000_000 { core::hint::spin_loop(); }
}

impl Ena {
    /// A readless read of a register: the request goes through MMIO_REG_READ, the answer is put by the device
    /// by DMA into mmio_resp (the req_id coincided -> reg_val is valid). A direct read of
    /// the BAR with ENA is not guaranteed (Nitro), so it goes only this way — like ena_com.
    fn reg_read(&mut self, off: u64) -> u32 {
        // A direct read of the BAR: either it was asked for explicitly, or readless has already stayed silent.
        if WANT_NOREADLESS.load(Ordering::Relaxed) || READLESS_DEAD.load(Ordering::Relaxed) {
            return r32_raw(self.regs + off);
        }
        self.mmio_seq = self.mmio_seq.wrapping_add(1);
        let seq = self.mmio_seq;
        unsafe { core::ptr::write_volatile(crate::mem::kptr::<u32>(self.mmio_resp), (seq as u32).wrapping_add(0xDEAD) << 16); }
        crate::arch::barrier::dma_wmb();
        w32(self.regs + REG_MMIO_REG_READ, ((off as u32) << 16) | seq as u32);
        for _ in 0..20_000 {
            let hdr = unsafe { core::ptr::read_volatile(crate::mem::kptr::<u32>(self.mmio_resp)) };
            if (hdr & 0xFFFF) as u16 == seq {
                if ((hdr >> 16) as u16) as u64 == off {
                    return unsafe { core::ptr::read_volatile(crate::mem::kptr::<u32>(self.mmio_resp + 4)) };
                }
                break;
            }
            udelay(1);
        }
        // There is no answer by DMA. Once we declare readless non-working and further on
        // we read the registers straight from the BAR — and this same call returns the direct
        // read rather than a u32::MAX: otherwise the very first call would bring the probe down.
        if !READLESS_DEAD.swap(true, Ordering::Relaxed) {
            let direct = r32_raw(self.regs + off);
            crate::println!("ena: the readless read is silent (a timeout at {:#x}) — I switch to a direct read of the BAR; {:#x} = {:#x}",
                off, off, direct);
            return direct;
        }
        r32_raw(self.regs + off)
    }

    /// One admin command: 64 bytes into the AQ, a call, a wait for a completion in the ACQ by
    /// the phase bit. It returns a copy of the ACQ record (16 u32) or None at a timeout.
    fn admin(&mut self, cmd: &mut [u32; 16]) -> Option<[u32; 16]> {
        self.aq_cmd_id = (self.aq_cmd_id + 1) & 0xFFF;
        let cid = self.aq_cmd_id;
        // the aq_common_desc in LE u32: command_id[15:0] | opcode[23:16] | flags[31:24] (bit 0 of the flags is the phase)
        cmd[0] = (cmd[0] & 0xFFFF_0000) | (cid as u32) | ((self.aq_phase_flag as u32) << 24);
        // THE TAIL IS MONOTONIC, THE INDEX IS ITS LOW BITS (ena_com: `sq.tail++`, the phase flips
        // when `tail & mask == 0`, and the DOORBELL takes the counter itself). It used to wrap to
        // zero at AQ_DEPTH and write 0 into the doorbell: no boot ever issued 32 admin commands
        // until GET_STATS was read every 10 s — the 33rd command put the device into
        // DEV_STS FATAL_ERROR (0x21) and the datapath died (2026-10-22, §12 item 1a's probe).
        let slot = self.aq + ((self.aq_tail & (AQ_DEPTH - 1)) as u64) * AQ_ENTRY as u64;
        unsafe { core::ptr::copy_nonoverlapping(cmd.as_ptr() as *const u8, crate::mem::kptr::<u8>(slot), AQ_ENTRY); }
        crate::arch::barrier::dma_wmb();
        self.aq_tail = self.aq_tail.wrapping_add(1);
        if self.aq_tail & (AQ_DEPTH - 1) == 0 { self.aq_phase_flag = !self.aq_phase_flag; }
        w32(self.regs + REG_AQ_DB, self.aq_tail as u32);
        // wait for the completion (polling, up to ~2 s)
        let hz = crate::tsc::hz();
        let t0 = crate::tsc::rdtsc();
        loop {
            let ce = self.acq + ((self.acq_head & (AQ_DEPTH - 1)) as u64) * ACQ_ENTRY as u64;
            let w0 = unsafe { core::ptr::read_volatile(crate::mem::kptr::<u32>(ce)) };
            let cphase = (w0 >> 24) & 1 != 0;
            if cphase == self.acq_phase {
                crate::arch::barrier::dma_rmb(); // the answer's words after the phase bit
                let mut out = [0u32; 16];
                unsafe { core::ptr::copy_nonoverlapping(crate::mem::kptr::<u8>(ce), out.as_mut_ptr() as *mut u8, ACQ_ENTRY); }
                self.acq_head = self.acq_head.wrapping_add(1);
                if self.acq_head & (AQ_DEPTH - 1) == 0 { self.acq_phase = !self.acq_phase; }
                let status = ((w0 >> 16) & 0xFF) as u8;
                if DBG.load(core::sync::atomic::Ordering::Relaxed) {
                    crate::println!("ena[dbg]: cmd[0..6]={:#x} {:#x} {:#x} {:#x} {:#x} {:#x} -> resp w0={:#x} w1={:#x} w2={:#x} w3={:#x}",
                        cmd[0], cmd[1], cmd[2], cmd[3], cmd[4], cmd[5], w0, out[1], out[2], out[3]);
                }
                if status != 0 {
                    let ext = out[1] & 0xFFFF; // acq_common: extended_status[15:0]
                    crate::println!("ena: admin opcode={} -> status={} ext={} (w1={:#x} w2={:#x})",
                        (cmd[0] >> 16) & 0xFF, status, ext, out[1], out[2]);
                    return None;
                }
                return Some(out);
            }
            if crate::tsc::rdtsc() - t0 > 2 * hz { crate::println!("ena: an admin timeout"); return None; }
            core::hint::spin_loop();
        }
    }
    fn get_feature(&mut self, id: u8) -> Option<[u32; 16]> { self.get_feature_ver(id, 0) }
    /// A GET_FEATURE with a feature's version (MAX_QUEUES_EXT requires version=1, as in ena_com).
    fn get_feature_ver(&mut self, id: u8, ver: u8) -> Option<[u32; 16]> {
        let mut c = [0u32; 16];
        c[0] = (OP_GET_FEATURE as u32) << 16;
        // the feat_common (@ the offset 16: flags, feature_id, feature_version, rsv)
        c[4] = ((id as u32) << 8) | ((ver as u32) << 16);
        self.admin(&mut c)
    }

    /// Create a CQ: the direction sets the record's size (a TX 8 B, an RX 16 B).
    /// interrupt_mode_enabled=1 — as in ena_com: the device rejects a CQ without
    /// it (status=6). The MSI-X vectors meanwhile are UNMASKED (the handler in the IDT
    /// only sends an EOI — it has been checked: the counter of the delivered ones grows at an AENQ), but
    /// the interrupts are silenced at the device's level (REG_INTR_MASK=1 and the CQ's moderation
    /// registers are not unmasked) — the datapath polls.
    /// talaria.ena=intr removes that too (§A1).
    #[allow(dead_code)]
    fn create_cq(&mut self, phys: u64, depth: u16, entry_bytes: u8) -> Option<(u16, u64, u32)> {
        self.create_cq_try(phys, depth, entry_bytes / 4, true, 0)
    }
    /// The variant with explicit parameters — for the diagnostic matrix (see probe).
    /// It returns (cq_idx, the offset of the head doorbell, the offset of the intr/unmask register).
    fn create_cq_try(&mut self, phys: u64, depth: u16, words: u8, intr: bool, vector: u32) -> Option<(u16, u64, u32)> {
        let mut c = [0u32; 16];
        c[0] = (OP_CREATE_CQ as u32) << 16;
        // w1: cq_caps_1 (b0: bit5 interrupt_mode), cq_caps_2 (b1: entry words), cq_depth (b2..3)
        c[1] = ((intr as u32) << 5) | ((words as u32) << 8) | ((depth as u32) << 16);
        c[2] = vector; // msix_vector
        c[3] = phys as u32;
        c[4] = (phys >> 32) as u32;
        let r = self.admin(&mut c)?;
        // resp: acq_common (2 u32), cq_idx|cq_actual_depth (u32), numa_off, head_db_off, unmask_off
        let idx = (r[2] & 0xFFFF) as u16;
        let head_db = r[4] as u64;
        Some((idx, head_db, r[5]))
    }

    /// Create an SQ (direction: 1=TX, 2=RX), the host's memory, a contiguous ring,
    /// a completion per every descriptor. It returns (sq_idx, doorbell_offset).
    fn create_sq(&mut self, phys: u64, depth: u16, dir: u8, cq_idx: u16) -> Option<(u16, u64)> {
        self.create_sq_p(phys, depth, dir, cq_idx, PLACEMENT_HOST).map(|(i, db, _)| (i, db))
    }
    /// Create an SQ with an explicit policy of the descriptors' placement: HOST (the ring is in
    /// our memory) or DEV (an LLQ — the ring is in the device's memory). It returns
    /// (sq_idx, doorbell_offset, llq_descriptors_offset).
    fn create_sq_p(&mut self, phys: u64, depth: u16, dir: u8, cq_idx: u16, placement: u32) -> Option<(u16, u64, u32)> {
        let mut c = [0u32; 16];
        c[0] = (OP_CREATE_SQ as u32) << 16;
        // w1: sq_identity (b0: dir<<5), rsv (b1), sq_caps_2 (b2: placement|completion),
        //     sq_caps_3 (b3: physically_contiguous)
        let sq_identity = (dir as u32) << 5;
        let sq_caps_2 = placement;           // completion_policy=DESC(0): a cqe per every descriptor
        let sq_caps_3 = 0x1u32;              // physically contiguous
        c[1] = sq_identity | (sq_caps_2 << 16) | (sq_caps_3 << 24);
        c[2] = (cq_idx as u32) | ((depth as u32) << 16);
        if placement == PLACEMENT_HOST { c[3] = phys as u32; c[4] = (phys >> 32) as u32; }
        // sq_head_writeback (we do not use it with completion_policy=0)
        let r = self.admin(&mut c)?;
        let idx = (r[2] & 0xFFFF) as u16;
        let db = r[3] as u64;
        Some((idx, db, r[4]))   // r[4] = llq_descriptors_offset (in BAR2)
    }

    // ── the datapath (the interface is like virtio-net's in l1.rs) ──

    /// Place the RX buffer slot into the receive ring. It returns a "token" = slot.
    pub fn receive_begin(&mut self, slot: usize, addr: u64) -> u16 {
        let idx = (self.rx_tail & (RX_DEPTH - 1)) as u64;
        let d = self.rx_sq + idx * RX_DESC_SZ as u64;
        let pa = crate::mem::Pa::of_kva(addr).get(); // the buffer is the kernel's pointer; the device takes its physical address
        unsafe {
            // length(u16) | rsv(u8) | ctrl(u8: phase|first|last|comp_req) | req_id(u16) | rsv(u16)
            // Bit 5 in the header's ctrl marks "MBO", but ena_com does NOT set it —
            // and with it the device stopped giving frames after the first seven.
            core::ptr::write_volatile(crate::mem::kptr::<u32>(d),
                (BUF as u32 & 0xFFFF) | (((self.rx_phase as u32) | (1 << 2) | (1 << 3) | (1 << 4)) << 24));
            core::ptr::write_volatile(crate::mem::kptr::<u32>(d + 4), slot as u32 & 0xFFFF);
            core::ptr::write_volatile(crate::mem::kptr::<u32>(d + 8), pa as u32);
            core::ptr::write_volatile(crate::mem::kptr::<u32>(d + 12), ((pa >> 32) as u32) & 0xFFFF);
        }
        // The rings' counters are MONOTONIC (like io_sq->tail in ena_com): the counter itself goes into
        // the doorbell, the descriptor's index is the low bits, the phase flips
        // at a turn of the ring.
        self.rx_tail = self.rx_tail.wrapping_add(1);
        if self.rx_tail & (RX_DEPTH - 1) == 0 { self.rx_phase = !self.rx_phase; }
        self.rx_db_dirty = true;
        slot as u16
    }
    /// How many receive buffers are waiting for a call and how many completions are not acknowledged.
    /// The threshold is as in ena_com: the CQ's head is acknowledged not more often than once per a quarter of the ring,
    /// the receive replenishment not more often than once every RX_BATCH buffers. Every write into
    /// the BAR on Nitro is expensive (an interception by the hypervisor — tens of microseconds), so
    /// per a ping-pong turnaround there must be EXACTLY ONE call left: the transmit tail.
    /// §7.2: one call per batch. To bring ALL the datapath's deferred MMIO writes together
    /// (the replenishment of the RX ring, the heads of both CQs, the TX tail) — it is called at the end of
    /// a pass of the stack rather than per frame. On Nitro a write into a doorbell is the most
    /// expensive operation of the path (the only synchronous exit to the device).
    /// It returns true if a call by the TRANSMIT path was really needed (the counter of
    /// the batch's amortisation — like virtio's kick).
    pub fn flush(&mut self) -> bool {
        if !(self.tx_db_dirty || self.rx_db_dirty || self.rx_cq_dirty || self.tx_cq_dirty) { return false; }
        let kicked = self.tx_db_dirty;
        let had_rx = self.rx_cq_dirty;   // §4.6: was the receive path handled in this pass
        crate::arch::barrier::dma_wmb();
        if self.tx_db_dirty { w32(self.regs + self.tx_db, self.tx_tail as u32); self.tx_db_dirty = false; self.db_writes += 1;
            self.llq_burst_left = self.llq_burst; }
        if self.rx_db_dirty { w32(self.regs + self.rx_db, self.rx_tail as u32); self.rx_db_dirty = false; self.db_writes += 1; }
        // §A1: the calls into the CQ's HEAD. A modern ena_com does not make them at all
        // (ena_com_update_dev_comp_head has been removed), but with us without them the receive path once
        // stalled — we leave them by default, talaria.ena=nocqdb removes them.
        let nocq = WANT_NOCQDB.load(Ordering::Relaxed);
        if self.rx_cq_dirty { self.rx_cq_dirty = false;
            if !nocq && self.rx_cq_db != 0 { w32(self.regs + self.rx_cq_db, self.rx_cq_head as u32); self.db_writes += 1; } }
        if self.tx_cq_dirty { self.tx_cq_dirty = false;
            if !nocq && self.tx_cq_db != 0 { w32(self.regs + self.tx_cq_db, self.tx_cq_head as u32); self.db_writes += 1; } }
        // §4.6 (A1): the moderation intervals 0 + an unmask at the end of a pass — exactly what
        // Linux does in ena_unmask_interrupt at the completion of a NAPI.
        if had_rx && self.rx_intr_reg != 0 && WANT_UNMASK.load(Ordering::Relaxed) {
            w32(self.regs + self.rx_intr_reg, 1 << 30); self.db_writes += 1;
        }
        kicked
    }

    /// Is there a received frame: it returns (slot, len) by the RX CQ's phase bit.
    pub fn poll_receive(&mut self) -> Option<(usize, usize)> {
        let idx = (self.rx_cq_head & (RX_DEPTH - 1)) as u64;
        let ce = self.rx_cq + idx * RX_CDESC_SZ as u64;
        let status = unsafe { core::ptr::read_volatile(crate::mem::kptr::<u32>(ce)) };
        if ((status >> 24) & 1 != 0) != self.rx_cq_phase { return None; }
        // The rest of the completion (the length, the buffer's id) and the frame itself are read
        // only AFTER the phase bit was seen: ena_com's dma_rmb() after the phase check. On x86
        // loads keep their order by themselves; on arm64 they do not, and a load issued before
        // the phase's could bring the previous lap's id or length (ARM64-PLAN §12 item 1).
        crate::arch::barrier::dma_rmb();
        let len = unsafe { core::ptr::read_volatile(crate::mem::kptr::<u16>(ce + 4)) } as usize;
        let req_id = unsafe { core::ptr::read_volatile(crate::mem::kptr::<u16>(ce + 6)) } as usize;
        self.rx_cq_head = self.rx_cq_head.wrapping_add(1);
        if self.rx_cq_head & (RX_DEPTH - 1) == 0 { self.rx_cq_phase = !self.rx_cq_phase; }
        self.rx_watch.store(self.rx_cq_head as u32 | ((self.rx_cq_phase as u32) << 16),
            core::sync::atomic::Ordering::Release);
        // Acknowledge the handled completions to the device: without this call ENA
        // considers the CQ full and stops giving packets (it was caught on EC2 —
        // the receive path stalled exactly after a few frames while the ring was free).
        // §7.2: the call is deferred until flush() (the insurance is once per a quarter of the ring,
        // so that a long burst does not run into a CQ that is "full" from the device's point of view).
        self.rx_cq_dirty = true;
        if self.rx_cq_head & (RX_DEPTH / 4 - 1) == 0 { let _ = self.flush(); }
        self.rx_pkts += 1;
        Some((req_id % RX_RING as usize, len.min(BUF)))
    }

    /// A call into the transmit tail. It also recharges the LLQ burst's budget
    /// (ena_com_write_tx_sq_doorbell does exactly that).
    fn tx_doorbell(&mut self) {
        crate::arch::barrier::dma_wmb();
        w32(self.regs + self.tx_db, self.tx_tail as u32);
        self.tx_db_dirty = false; self.db_writes += 1;
        self.llq_burst_left = self.llq_burst;
    }


    /// An LLQ transmission (§7.3): the descriptor AND the frame's start are written straight into the device's
    /// memory (BAR2) — the device does not need to fetch a descriptor over PCIe.
    /// The record is assembled on the stack and leaves as a stream of 8-byte stores.
    fn transmit_llq(&mut self, slot: usize, addr: u64, len: usize) {
        let hdr = len.min(self.llq_hdr_max);
        let rem = len - hdr;
        let mut e = [0u8; 256];
        let req_id = slot as u32;
        let buf_addr = if rem > 0 { crate::mem::Pa::of_kva(addr).add(hdr as u64).get() } else { 0 }; // the device's address of the frame's rest
        // Slot 0 is the meta descriptor if the device does not cache the meta (accel_mode).
        // Then FIRST moves onto it, while the data descriptor goes as slot 1 —
        // exactly as in ena_com_prepare_tx (`if (!have_meta) desc->len_ctrl |= FIRST`).
        let d = if self.llq_meta {
            let m0 = META_DESC | META_EXT_VALID | META_ETH_META_TYPE | META_STORE
                | ((self.tx_phase as u32) << 24) | (1 << 26);
            e[0..4].copy_from_slice(&m0.to_le_bytes());
            TX_DESC_SZ
        } else { 0 };
        let first = if self.llq_meta { 0 } else { 1u32 << 26 };
        let w0 = (rem as u32 & 0xFFFF) | (((req_id >> 10) & 0x3F) << 16)
            | ((self.tx_phase as u32) << 24) | first | (1 << 27) | (1 << 28);
        let w1 = (req_id & 0x3FF) << 22 | meta_of(addr, len);
        let w2 = buf_addr as u32;
        let w3 = ((buf_addr >> 32) as u32 & 0xFFFF) | ((hdr as u32) << 24); // header_length is in 31:24
        e[d..d + 4].copy_from_slice(&w0.to_le_bytes());
        e[d + 4..d + 8].copy_from_slice(&w1.to_le_bytes());
        e[d + 8..d + 12].copy_from_slice(&w2.to_le_bytes());
        e[d + 12..d + 16].copy_from_slice(&w3.to_le_bytes());
        // the header goes AFTER the slots for the descriptors (descs_num_before_header=2 -> 32 B)
        unsafe { core::ptr::copy_nonoverlapping(crate::mem::vptr::<u8>(addr), e.as_mut_ptr().add(32), hdr); }
        let dst = self.llq_base + ((self.tx_tail as u64) & (Q_DEPTH as u64 - 1)) * self.llq_entry as u64;
        unsafe {
            let src = e.as_ptr() as *const u64;
            for i in 0..(self.llq_entry / 8) {
                core::ptr::write_volatile(crate::mem::devptr::<u64>(dst + (i * 8) as u64), core::ptr::read_unaligned(src.add(i)));
            }
        }
        fence(Ordering::SeqCst);
    }

    /// Post a frame into the transmit ring WITHOUT a call (the call goes in flush(), §7.2).
    pub fn transmit_no_notify(&mut self, slot: usize, addr: u64, len: usize) -> bool {
        self.reap_tx();
        if !self.tx_free[slot] { return false; }
        if self.llq_base != 0 {
            // LIMIT_TX_BURST: between two calls the device accepts not more than
            // max_tx_burst_size bytes of records — to exceed that means to lose a packet
            // (ena_com returns an ENOSPC). We ring in advance and zero the count.
            if self.llq_burst != 0 && self.llq_burst_left == 0 { self.tx_doorbell(); }
            self.tx_free[slot] = false;
            self.tx_free_n -= 1;
            self.tx_post_tsc[slot] = crate::tsc::rdtsc();
            self.transmit_llq(slot, addr, len);
            if self.llq_burst_left != 0 { self.llq_burst_left -= 1; }
            self.tx_tail = self.tx_tail.wrapping_add(1);
            if self.tx_tail & (Q_DEPTH - 1) == 0 { self.tx_phase = !self.tx_phase; }
            self.tx_db_dirty = true;
            self.tx_pkts += 1;
            return true;
        }
        self.tx_free[slot] = false;
        self.tx_free_n -= 1;
        self.tx_post_tsc[slot] = crate::tsc::rdtsc();
        let d = self.tx_sq + ((self.tx_tail & (Q_DEPTH - 1)) as u64) * TX_DESC_SZ as u64;
        let req_id = slot as u32;
        let pa = crate::mem::Pa::of_kva(addr).get(); // the buffer is the kernel's pointer; the device takes its physical address
        unsafe {
            // len_ctrl: length | req_id_hi<<16 | phase<<24 | first<<26 | last<<27 | comp_req<<28
            core::ptr::write_volatile(crate::mem::kptr::<u32>(d),
                (len as u32 & 0xFFFF) | (((req_id >> 10) & 0x3F) << 16)
                | ((self.tx_phase as u32) << 24) | (1 << 26) | (1 << 27) | (1 << 28));
            // meta_ctrl: req_id_lo<<22 + the type of the protocols (see meta_of)
            core::ptr::write_volatile(crate::mem::kptr::<u32>(d + 4), (req_id & 0x3FF) << 22 | meta_of(addr, len));
            core::ptr::write_volatile(crate::mem::kptr::<u32>(d + 8), pa as u32);
            core::ptr::write_volatile(crate::mem::kptr::<u32>(d + 12), (pa >> 32) as u32 & 0xFFFF);
        }
        self.tx_tail = self.tx_tail.wrapping_add(1);
        if self.tx_tail & (Q_DEPTH - 1) == 0 { self.tx_phase = !self.tx_phase; }
        self.tx_db_dirty = true;
        self.tx_pkts += 1;
        true
    }

    /// Collect the completed transmissions (free the slots). The call into the TX CQ's head goes
    /// once per the whole collected batch (§7.2).
    pub fn reap_tx(&mut self) {
        loop {
            let ce = self.tx_cq + ((self.tx_cq_head & (Q_DEPTH - 1)) as u64) * TX_CDESC_SZ as u64;
            let w0 = unsafe { core::ptr::read_volatile(crate::mem::kptr::<u32>(ce)) }; // req_id | status | flags
            let phase = (w0 >> 24) & 1 != 0;
            if phase != self.tx_cq_phase { return; }
            let req_id = (w0 & 0xFFFF) as usize;
            if req_id < TX_RING as usize && !self.tx_free[req_id] {
                self.tx_free[req_id] = true; self.tx_free_n += 1;
                let hz = crate::tsc::hz();
                if hz != 0 && self.tx_post_tsc[req_id] != 0 {
                    let us = crate::tsc::rdtsc().wrapping_sub(self.tx_post_tsc[req_id]) / (hz / 1_000_000).max(1);
                    let b = match us { 0..=4 => 0, 5..=9 => 1, 10..=19 => 2, 20..=49 => 3, 50..=99 => 4, 100..=149 => 5, 150..=199 => 6, _ => 7 };
                    TXC_HIST[b].fetch_add(1, Ordering::Relaxed);
                    if us > TXC_MAX_US.load(Ordering::Relaxed) { TXC_MAX_US.store(us, Ordering::Relaxed); }
                }
            }
            self.tx_cq_head = self.tx_cq_head.wrapping_add(1);
            if self.tx_cq_head & (Q_DEPTH - 1) == 0 { self.tx_cq_phase = !self.tx_cq_phase; }
            self.tx_cq_dirty = true;
        }
    }
    /// §7.5: allow the device to raise a receive interrupt (the moderation
    /// intervals 0 + an unmask). In the polling mode it is not needed, but before a sleep it is
    /// obligatory: a sleeping core will be woken only by an interrupt.
    pub fn arm_intr(&self) {
        if self.rx_intr_reg != 0 { w32(self.regs + self.rx_intr_reg, 1 << 30); }
    }
    /// The card's line on ENA (`talaria.netirq=1`, session 26). With virtio a line is a GSI
    /// through the I/O APIC, with ENA it is an MSI-X message: the vector `MSI_VECTOR_BASE + io_vec`
    /// is already programmed in the device's table and directed to a core by
    /// the layout `irq=` (`affinity::route_device_irqs`), but on it stands a stub with
    /// one EOI, while on the device's side the interrupts are silenced by `REG_INTR_MASK=1`.
    /// Here the mask is removed; the handler for the vector is set by `netsys::netirq_setup`.
    /// The receive interrupt itself is raised by `arm_intr` (an unmask of the receive CQ): after
    /// a delivery the device holds it until the next unmask, and the line's handler
    /// raises it anew at the end of a pass — exactly like `ena_unmask_interrupt`
    /// at the end of a NAPI. The transmit CQ is never raised: it is parsed by a pass.
    pub fn irq_vector(&mut self) -> Option<u8> {
        if self.rx_intr_reg == 0 { return None; }
        w32(self.regs + REG_INTR_MASK, 0);
        Some((crate::idt::MSI_VECTOR_BASE as u32 + self.io_vec) as u8)
    }
    /// §7.5: the word of the receive CQ that we wait for, and its current value — in order to
    /// SLEEP on it with a monitor (MONITOR/MWAIT) instead of a busy poll: it will be woken by the device's
    /// own DMA write (or by any interrupt, our MSI-X is unmasked).
    pub fn rx_cq_watch(&self) -> (*const u32, u32) {
        let idx = (self.rx_cq_head & (RX_DEPTH - 1)) as u64;
        let a = crate::mem::kptr::<u32>(self.rx_cq + idx * RX_CDESC_SZ as u64).cast_const();
        (a, unsafe { core::ptr::read_volatile(a) })
    }
    /// A free TX slot (or None — they are all in flight). O(1) by a cursor: a linear
    /// scan of the ring per EVERY frame was a noticeable part of the transmission's price.
    pub fn tx_slot(&mut self) -> Option<usize> {
        self.reap_tx();
        if self.tx_free_n == 0 { return None; }
        for _ in 0..TX_RING {
            let i = self.tx_next as usize;
            self.tx_next = (self.tx_next + 1) % TX_RING;
            if self.tx_free[i] { return Some(i); }
        }
        None
    }

    /// The datapath's periodic statistics (once every 30 s) — one can see where it stalls.
    /// ONLY with `talaria.stat=`: printing into the serial on EC2 costs milliseconds
    /// with the interrupts forbidden (see `obs::periodic_print_on`).
    pub fn stats_tick(&mut self) {
        if !crate::obs::periodic_print_on() { return; }
        let hz = crate::tsc::hz();
        let now = crate::tsc::rdtsc();
        if self.stat_tsc == 0 { self.stat_tsc = now; return; }
        if now - self.stat_tsc < 30 * hz { return; }   // less often: printing into the serial on EC2 is expensive
        self.stat_tsc = now;
        let sts = self.reg_read(REG_DEV_STS);
        // the word of the CQ record we are waiting for: one can see whether the device writes anything there
        // (and with what phase) — otherwise the receive path "stalls" without a visible reason
        let idx = (self.rx_cq_head & (RX_DEPTH - 1)) as u64;
        let peek = unsafe { core::ptr::read_volatile(crate::mem::kptr::<u32>(self.rx_cq + idx * RX_CDESC_SZ as u64)) };
        // Through a buffer: two lines straight from here are the datapath with IF=0, and on EC2
        // they gave with redis a "max of 4.8-4.9 ms" at every point with a p99.99 of
        // 70 us (see obs::deferred_print).
        let (rx, tx, db) = (self.rx_pkts, self.tx_pkts, self.db_writes);
        let (rt, rh, rp, tt, tf) = (self.rx_tail, self.rx_cq_head, self.rx_cq_phase as u8, self.tx_tail, self.tx_free_n);
        crate::obs::deferred_print(|| {
            crate::println!("ena: the MSI-X interrupts delivered: {}", crate::idt::IPI_COUNT.load(core::sync::atomic::Ordering::Relaxed));
            crate::println!("ena: rx={} tx={} db={} (per frame {}.{:02}) rx_tail={} rx_cq_head={}/ph{} tx_tail={} free_tx={} sts={:#x} cq[{}]={:#x}",
                rx, tx, db, db / (rx + tx).max(1), db * 100 / (rx + tx).max(1) % 100,
                rt, rh, rp, tt, tf, sts, idx, peek);
        });
    }

    /// Drain the AENQ (the device's asynchronous events): we do not handle them, but we must
    /// move the head — otherwise the device considers the queue overflowed and resets.
    pub fn drain_aenq(&mut self) {
        loop {
            // The head is monotonic and the doorbell takes the counter (ena_com's aenq.head++),
            // the index is its low bits — the same rule as the admin queue's tail (see `admin`).
            let e = self.aenq + ((self.aenq_head & (AENQ_DEPTH - 1)) as u64) * AENQ_ENTRY as u64;
            let flags = unsafe { core::ptr::read_volatile(crate::mem::kptr::<u32>(e + 4)) };
            if ((flags >> 0) & 1 != 0) != self.aenq_phase { break; }
            crate::arch::barrier::dma_rmb();
            self.aenq_head = self.aenq_head.wrapping_add(1);
            if self.aenq_head & (AENQ_DEPTH - 1) == 0 { self.aenq_phase = !self.aenq_phase; }
            w32(self.regs + REG_AENQ_HEAD_DB, self.aenq_head as u32);
        }
    }
}

/// Find and initialise ENA. None — there is no device or the initialisation did not succeed.
impl Ena {
    /// The parts of RSS separately — that is how it turned out that the receive path is broken exactly by THE TABLE.
    /// The record's field is called `cq_idx`, but ena_com puts the SQ's index into it
    /// (`rss->rss_ind_tbl[i].cq_idx = io_sq_queues[idx].idx`) — with the CQ's index
    /// the device took the frames into nowhere: udp_ping gave "all lost" with the command accepted.
    pub fn rss_config_parts(&mut self, rx_idx: u16, tbl: bool, hash: bool) -> bool {
        let page = match crate::mem::alloc_any(1) { Some(a) => a, None => return false };
        // 1) the redirection table: 128 records {u16 cq_idx; u16 rsv} = 512 B
        unsafe {
            let t = crate::mem::kptr::<u16>(page);
            for i in 0..RSS_IND_ENTRIES { core::ptr::write_volatile(t.add(i * 2), rx_idx); core::ptr::write_volatile(t.add(i * 2 + 1), 0); }
        }
        if tbl {
        let mut c = [0u32; 16];
        c[0] = ((OP_SET_FEATURE as u32) << 16) | (0x4 << 24);
        c[1] = (RSS_IND_ENTRIES * 4) as u32;
        c[2] = page as u32;
        c[3] = ((page >> 32) as u32) & 0xFFFF;
        c[4] = (FEAT_RSS_IND_TABLE as u32) << 8;
        c[5] = 0;                                   // min_size | max_size<<16
        c[6] = 7;                                   // size = log2(128)
        c[7] = 0xFFFF_FFFF;                         // inlined_index = -1
        if self.admin(&mut c).is_none() { crate::println!("ena: RSS: the redirection table was rejected"); return false; }
        }
        // 2) the Toeplitz hash function: {key_parts=10, rsv, key[10]} = 48 B
        if hash {
        unsafe {
            let k = crate::mem::kptr::<u32>(page);
            core::ptr::write_volatile(k, 10);
            core::ptr::write_volatile(k.add(1), 0);
            // a fixed key: the reproducibility matters more than the "randomness"
            for i in 0..10 { core::ptr::write_volatile(k.add(2 + i), 0x6d5a_56da_u32.wrapping_mul(i as u32 + 1) ^ 0x255b_0ec2); }
        }
        let mut c = [0u32; 16];
        c[0] = ((OP_SET_FEATURE as u32) << 16) | (0x4 << 24);
        c[1] = 48;
        c[2] = page as u32;
        c[3] = ((page >> 32) as u32) & 0xFFFF;
        c[4] = (FEAT_RSS_HASH_FUNCTION as u32) << 8;
        c[5] = 0;                                   // supported_func (it is ignored at a SET)
        c[6] = RSS_FUNC_TOEPLITZ;                   // selected_func
        c[7] = 0xFFFF_FFFF;                         // init_val — as with Linux
        if self.admin(&mut c).is_none() { crate::println!("ena: RSS: the hash function was rejected"); return false; }
        }
        crate::println!("ena: RSS is configured: the table={} ({} records -> the queue {}) the hash={} (Toeplitz)", tbl, RSS_IND_ENTRIES, rx_idx, hash);
        true
    }
    /// §4.4: configure the PHC (the device's clock). A readless protocol, as in ena_com:
    ///   1) a GET_FEATURE(PHC_CONFIG) -> the type and the call's offset in BAR0;
    ///   2) a SET_FEATURE(PHC_CONFIG) with the address of OUR page for the answer;
    ///   3) the read: we write a req_id into the call, wait until the device puts the same
    ///      req_id into the page, read the 64-bit stamp.
    /// What for: an NTP correction by construction divides the RTT in half, there is nothing to separate
    /// the RX and the TX with (§4.9 is incomplete). A clock shared with a neighbour in the rack fixes that.
    pub fn phc_init(&mut self) -> bool {
        let g = match self.get_feature(FEAT_PHC_CONFIG) { Some(g) => g, None => {
            crate::println!("ena: the PHC — the GET_FEATURE was rejected (the device's clock is unavailable)"); return false; } };
        // phc_desc: [2] type|rsv, [3] doorbell_offset, [4] expire_timeout_usec,
        //           [5] block_timeout_usec, [6..7] output_address, [8] output_length
        let typ = g[2] & 0xFF;
        let db = g[3];
        crate::println!("ena: the PHC: the type={} the call@{:#x} expire={}us block={}us", typ, db, g[4], g[5]);
        if typ != PHC_TYPE_READLESS { crate::println!("ena: the PHC: only readless is supported"); return false; }
        if self.phc_out == 0 {
            match crate::mem::alloc_any(1) { Some(a) => self.phc_out = a, None => return false }
        }
        unsafe { core::ptr::write_bytes(crate::mem::kptr::<u8>(self.phc_out), 0, 4096) };
        let mut c = [0u32; 16];
        c[0] = (OP_SET_FEATURE as u32) << 16;
        c[4] = (FEAT_PHC_CONFIG as u32) << 8;
        c[5] = PHC_TYPE_READLESS;              // type
        c[6] = 0;                              // doorbell_offset (the device uses its own)
        c[7] = 0;                              // expire_timeout_usec (0 = by default)
        c[8] = 0;                              // block_timeout_usec
        c[9] = self.phc_out as u32;            // output_address.lo
        c[10] = ((self.phc_out >> 32) as u32) & 0xFFFF; // output_address.hi
        c[11] = 64;                            // output_length (struct ena_admin_phc_resp)
        if self.admin(&mut c).is_none() { crate::println!("ena: the PHC: the SET_FEATURE was rejected"); return false; }
        self.phc_db = db;
        self.phc_ok = true;
        crate::println!("ena: the PHC is on (the answer page {:#x})", self.phc_out);
        true
    }
    /// Read the device's clock (ns). None — it is not configured/a timeout.
    pub fn phc_read(&mut self) -> Option<u64> {
        if !self.phc_ok { return None; }
        self.phc_req = self.phc_req.wrapping_add(1);
        let req = self.phc_req;
        unsafe {
            let p = crate::mem::kptr::<u16>(self.phc_out);
            core::ptr::write_volatile(p.add(4), 0);          // erase the previous req_id? (the field @0)
            core::ptr::write_volatile(crate::mem::kptr::<u64>(self.phc_out + 8), u64::MAX);
            w32(self.regs + self.phc_db as u64, req as u32); // the call
            let hz = crate::tsc::hz();
            let t0 = crate::tsc::rdtsc();
            loop {
                let got = core::ptr::read_volatile(crate::mem::kptr::<u16>(self.phc_out));
                if got == req {
                    let ts = core::ptr::read_volatile(crate::mem::kptr::<u64>(self.phc_out + 8));
                    if ts != u64::MAX { return Some(ts); }
                }
                if crate::tsc::rdtsc() - t0 > hz / 1000 { return None; } // 1 ms
                core::hint::spin_loop();
            }
        }
    }
}

/// Bring up THE FIRST ENA card found (a compatibility with the old path).
pub fn probe() -> Option<Ena> {
    let mut dev = None;
    for id in ENA_DEVICES { if let Some(d) = crate::pci::find(ENA_VENDOR, id) { dev = Some(d); break; } }
    probe_at(&dev?)
}

/// Bring up A PARTICULAR PCI function. A separate entry is needed where there is more than one
/// card (an instance with several ENIs): "the first one found" is no longer an answer then.
pub fn probe_at(d: &crate::pci::PciDev) -> Option<Ena> {
    let d = *d;
    let (bar0, sz0) = crate::pci::bar(d.bus, d.dev, d.func, 0);
    // A11: we print WHAT exactly was found and where we are going to write, BEFORE the first access to
    // the device. On a c6i.metal the probe died on a read of the registers, and by the log it was
    // impossible to understand even whether it is the right function and what its BAR is.
    crate::println!("ena: the device {:02x}:{:02x}.{} BAR0={:#x} ({} KB)",
        d.bus, d.dev, d.func, crate::arch::mmio_pa(bar0), sz0 / 1024);
    if bar0 == 0 { crate::println!("ena: BAR0 is unavailable"); return None; }

    // the pages for the queues/buffers (identity: phys == virt)
    let aq = crate::mem::alloc_any(1)?;                 // 32*64 = 2 KB
    let acq = crate::mem::alloc_any(1)?;
    let aenq = crate::mem::alloc_any(1)?;               // 16*64 = 1 KB
    let mmio_resp = crate::mem::alloc_any(1)?;
    // the rings: 512 records x 16 B = 8 KB, we allocate 16 KB and align by 16 KB
    // The rings come from the RAM ABOVE 4 GB: a live ena driver on this hardware places them
    // there (dma_alloc_coherent), and that is the only thing by which its CREATE_CQ accepted by the device
    // differed from our rejected one. The fallback is the ordinary memory.
    let ring = || -> Option<u64> {
        if let Some(a) = crate::mem::alloc_high(4, 16 * 1024) { return Some(a); }
        let base = crate::mem::alloc_contig(8)?;
        let a = (base + 0x3FFF) & !0x3FFFu64;
        unsafe { core::ptr::write_bytes(crate::mem::kptr::<u8>(a), 0, 16 * 1024) };
        Some(a)
    };
    let tx_sq = ring()?;
    let tx_cq = ring()?;
    let rx_sq = ring()?;
    let rx_cq = ring()?;
    for p in [aq, acq, aenq, mmio_resp] {
        unsafe { core::ptr::write_bytes(crate::mem::kptr::<u8>(p), 0, 4096) };
    }

    let mut e = Ena {
        regs: bar0, aq, acq, aenq, mmio_resp,
        aq_tail: 0, acq_head: 0, acq_phase: true, aq_cmd_id: 0, aq_phase_flag: true,
        aenq_head: 0, aenq_phase: true, mmio_seq: 0,
        tx_sq, tx_cq, tx_db: 0, tx_cq_db: 0, tx_tail: 0, tx_phase: true,
        tx_cq_head: 0, tx_cq_phase: true, tx_free: [true; TX_RING as usize],
        tx_free_n: TX_RING, tx_next: 0, tx_post_tsc: [0; TX_RING as usize],
        rx_sq, rx_cq, rx_db: 0, rx_cq_db: 0, rx_tail: 0, rx_phase: true,
        rx_cq_head: 0, rx_cq_phase: true, rx_watch: core::sync::atomic::AtomicU32::new(1 << 16),
        rx_intr_reg: 0, tx_intr_reg: 0, io_vec: 0,
        tx_db_dirty: false, tx_cq_dirty: false, rx_db_dirty: false, rx_cq_dirty: false, db_writes: 0,
        llq_base: 0, llq_entry: 0, llq_hdr_max: 0,
        llq_meta: false, llq_burst: 0, llq_burst_left: 0,
        phc_out: 0, phc_db: 0, phc_req: 0, phc_ok: false,
        mac: [0; 6], mtu: 1500, rx_pkts: 0, tx_pkts: 0, stat_tsc: 0,
    };

    // the address of the readless read's answer — before the first reg_read
    w32(e.regs + REG_MMIO_RESP_LO, mmio_resp as u32);
    w32(e.regs + REG_MMIO_RESP_HI, (mmio_resp >> 32) as u32);
    w32(e.regs + REG_INTR_MASK, 1); // the interrupts are masked: only polling

    let ver = e.reg_read(REG_VERSION);
    let cver = e.reg_read(REG_CONTROLLER_VERSION);
    let caps = e.reg_read(REG_CAPS);
    if ver == u32::MAX || caps == u32::MAX {
        // A11: we print WHAT was read. A 0xFFFFFFFF on all the registers is not
        // "the device is silent" but "the BAR is not decoded" (the wrong address, the memory space is not
        // enabled, a bridge's window does not cover the BAR): those are different diagnoses,
        // and they can be told apart only by seeing the values.
        crate::println!("ena: the registers do not read: version={:#x} controller={:#x} caps={:#x} (readless is {})",
            ver, cver, caps, if READLESS_DEAD.load(Ordering::Relaxed) { "dead, we read straight from the BAR" } else { "alive" });
        return None;
    }
    crate::println!("ena: {:02x}:{:02x}.{} BAR0={:#x} version={}.{} controller={}.{}.{} caps={:#x}",
        d.bus, d.dev, d.func, crate::arch::mmio_pa(bar0), (ver >> 8) & 0xFF, ver & 0xFF,
        (cver >> 16) & 0xFF, (cver >> 8) & 0xFF, cver & 0xFF, caps);

    // the controller's reset (like ena_com_dev_reset)
    let sts = e.reg_read(REG_DEV_STS);
    if sts & DEV_STS_READY == 0 { crate::println!("ena: the device is not READY (sts={:#x})", sts); return None; }
    let timeout_100ms = ((caps & CAPS_RESET_TIMEOUT_MASK) >> CAPS_RESET_TIMEOUT_SHIFT).max(1) as u64;
    w32(e.regs + REG_DEV_CTL, DEV_CTL_DEV_RESET);
    w32(e.regs + REG_MMIO_RESP_LO, mmio_resp as u32); // the address is rewritten after the reset
    w32(e.regs + REG_MMIO_RESP_HI, (mmio_resp >> 32) as u32);
    let mut ok = false;
    for _ in 0..(timeout_100ms * 100) {
        if e.reg_read(REG_DEV_STS) & DEV_STS_RESET_IN_PROGRESS != 0 { ok = true; break; }
        udelay(1000);
    }
    if !ok { crate::println!("ena: the reset did not start"); return None; }
    w32(e.regs + REG_DEV_CTL, 0);
    ok = false;
    for _ in 0..(timeout_100ms * 100) {
        if e.reg_read(REG_DEV_STS) & DEV_STS_RESET_IN_PROGRESS == 0 { ok = true; break; }
        udelay(1000);
    }
    if !ok { crate::println!("ena: the reset did not finish"); return None; }

    // the admin queues: the bases and the depths/the records' sizes (exactly like ena_com)
    w32(e.regs + REG_AQ_BASE_LO, aq as u32);
    w32(e.regs + REG_AQ_BASE_HI, (aq >> 32) as u32);
    w32(e.regs + REG_ACQ_BASE_LO, acq as u32);
    w32(e.regs + REG_ACQ_BASE_HI, (acq >> 32) as u32);
    w32(e.regs + REG_AQ_CAPS, (AQ_DEPTH as u32) | ((AQ_ENTRY as u32) << 16));
    w32(e.regs + REG_ACQ_CAPS, (AQ_DEPTH as u32) | ((ACQ_ENTRY as u32) << 16));
    w32(e.regs + REG_AENQ_BASE_LO, aenq as u32);
    w32(e.regs + REG_AENQ_BASE_HI, (aenq >> 32) as u32);
    w32(e.regs + REG_AENQ_CAPS, (AENQ_DEPTH as u32) | ((AENQ_ENTRY as u32) << 16));
    // (the call into the AENQ_HEAD_DB comes later, after the subscription to the groups: a live driver
    //  does an ena_com_admin_aenq_enable immediately before creating the queues)

    // ── The host attributes go AS THE FIRST command (the order matters) ──────
    // The trace of a live ena driver on this same instance taken with a kprobe: right
    // after the admin init it sends a SET_FEATURE(HOST_ATTR_CONFIG), and only then
    // queries the capabilities. Before that command Nitro's firmware returns a reduced
    // set of features (LLQ/RSS/the offloads -> UNSUPPORTED) and rejects the creation of any
    // IO queues (CREATE_CQ/CREATE_SQ -> UNKNOWN_ERROR).
    // The layout of ena_admin_host_info (the offsets are by the header):
    //   0 os_type | 4 os_dist_str[128] | 132 os_dist | 136 kernel_ver_str[32]
    //   168 kernel_ver | 172 driver_version | 176 supported_network_features[2]
    //   184 ena_spec_version | 186 bdf | 188 num_cpus | 192 driver_supported_features
    let hi = crate::mem::alloc_any(1)?;
    unsafe {
        core::ptr::write_bytes(crate::mem::kptr::<u8>(hi), 0, 4096);
        core::ptr::write_volatile(crate::mem::kptr::<u32>(hi), 1u32);                  // os_type = LINUX (a Linux-ABI runtime)
        core::ptr::copy_nonoverlapping(b"Talaria OS\0".as_ptr(), crate::mem::kptr::<u8>(hi + 4), 11);
        core::ptr::copy_nonoverlapping(b"talaria-1.1\0".as_ptr(), crate::mem::kptr::<u8>(hi + 136), 12);
        // driver_version: major | minor<<8 | sub_minor<<16 | module_type<<24.
        // We set a version of the level of a modern ena driver: Nitro's firmware may
        // refuse the creation of queues to a driver with a "too old" version.
        core::ptr::write_volatile(crate::mem::kptr::<u32>(hi + 172), 2 | (13 << 8));  // 2.13.0
        core::ptr::write_volatile(crate::mem::kptr::<u16>(hi + 184), 2u16 << 8);     // ena_spec_version 2.0 (MAJOR<<8|MINOR)
        core::ptr::write_volatile(crate::mem::kptr::<u16>(hi + 186),
            ((d.bus as u16) << 8) | ((d.dev as u16) << 3) | d.func as u16);
        core::ptr::write_volatile(crate::mem::kptr::<u16>(hi + 188), 1u16);          // num_cpus
        core::ptr::write_volatile(crate::mem::kptr::<u32>(hi + 192), 0u32);          // we declare no extra features
    }
    let mut c = [0u32; 16];
    c[0] = (OP_SET_FEATURE as u32) << 16;
    c[4] = (FEAT_HOST_ATTR_CONFIG as u32) << 8;
    c[5] = hi as u32; c[6] = ((hi >> 32) as u32) & 0xFFFF; // os_info_ba (u32 low + u16 high)
    crate::println!("ena: host attributes {}",
        if e.admin(&mut c).is_some() { "have been accepted" } else { "HAVE BEEN REJECTED" });

    // ── the device's capabilities ───────────────────────────────────────────
    let attr = e.get_feature(FEAT_DEVICE_ATTRIBUTES)?;
    // dev_attr: impl_id, device_version, supported_features, capabilities,
    //           phys_addr_width, virt_addr_width, mac[6]+rsv[2], max_mtu
    let (mac_w0, mac_w1) = (attr[8], attr[9]);
    e.mac = [mac_w0 as u8, (mac_w0 >> 8) as u8, (mac_w0 >> 16) as u8, (mac_w0 >> 24) as u8,
             mac_w1 as u8, (mac_w1 >> 8) as u8];
    let max_mtu = attr[10];
    e.mtu = if WANT_MTU9001.load(Ordering::Relaxed) { max_mtu.min(9001) }
            else if max_mtu >= 1500 { 1500 } else { max_mtu };
    crate::println!("ena: mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} max_mtu={} supported={:#x} caps={:#x}",
        e.mac[0], e.mac[1], e.mac[2], e.mac[3], e.mac[4], e.mac[5], max_mtu, attr[4], attr[5]);
    // §4.4: the device's clock — only if it is declared in supported_features (bit 29)
    let phc_sup = attr[4] & (1 << FEAT_PHC_CONFIG) != 0;
    crate::println!("ena: the PHC (the device's clock): {}", if phc_sup { "it is declared by the device" } else { "it is NOT declared on this instance type" });
    if phc_sup && WANT_PHC.load(core::sync::atomic::Ordering::SeqCst) { e.phc_init(); }
    // The footprint of the receive rings against L3 ("leaky DMA", POC-RESULTS v0.15). Right now
    // the rule "a ring within L3" is kept BY A CONSTANT (RX_RING=256 buffers),
    // and it will break silently on the day the multi-queue receive path is opened (A6):
    // 32 queues of the same rings are already tens of megabytes. So the number is computed and
    // printed by itself rather than staying a comment next to the constant.
    crate::cat::report_footprint("the ENA's receive path", 1, RX_RING as u64, BUF as u64, RX_DEPTH as u64 * RX_DESC_SZ as u64);
    if let Some(q) = e.get_feature_ver(FEAT_MAX_QUEUES_EXT, 1) {
        crate::println!("ena: the queues: tx_sq={} tx_cq={} rx_sq={} rx_cq={} tx_depth={} rx_depth={}",
            q[3], q[4], q[5], q[6], q[7], q[9]);
    } else if let Some(q) = e.get_feature(FEAT_MAX_QUEUES_NUM) {
        crate::println!("ena: the queues (legacy): max_sq={} depth={} max_cq={} depth={}", q[2], q[3], q[4], q[5]);
    }
    // The LLQ (version 1): our TX is in the host's memory, but we repeat the introduction — without it
    // the device considers the driver "incomplete"
    if let Some(l) = e.get_feature_ver(FEAT_LLQ, 1) {
        // llq_desc: [2] max_llq_num [3] max_llq_depth
        //           [4] hdr_loc sup|en [5] entry_size sup|en [6] desc_num_before_hdr sup|en
        //           [7] stride sup|en [8] rsv [9..10] accel_mode
        let (hdr_s, ent_s) = (l[4] & 0xFFFF, l[5] & 0xFFFF);
        let (dnbh_s, str_s) = (l[6] & 0xFFFF, l[7] & 0xFFFF);
        // w8 = feature_version | entry_size_recommended<<8 | max_wide_llq_depth<<16
        // (we print it in order to make sure of accel_mode's offset: if w8 is parsed
        // meaningfully, then accel_mode really is in w9)
        crate::println!("ena: the LLQ: num={} depth={} hdr_sup={:#x} entry_sup={:#x} dnbh_sup={:#x} stride_sup={:#x} w8={:#x} (ver={} recommended={}) \
            accel_sup={:#x} max_burst={} B",
            l[2], l[3], hdr_s, ent_s, dnbh_s, str_s, l[8], l[8] & 0xFF, (l[8] >> 8) & 0xFF,
            l[9] & 0xFFFF, (l[9] >> 16) & 0xFFFF);
        // §7.3: we send a SET_FEATURE(LLQ) ONLY by talaria.ena=llq. The configuration
        // moves the device into a mode where it expects the descriptors in its own memory —
        // without the corresponding datapath the TX completions stop coming.
        if WANT_LLQ.load(core::sync::atomic::Ordering::Relaxed) {
            // A 256-byte record is more profitable: A SMALL frame fits into it entirely
            // (a header of up to 224 B), and the device does not need a single read from
            // the host's memory — neither for a descriptor nor for the packet's body.
            let (entry, esz) = if ent_s & LLQ_ENTRY_256B != 0 { (LLQ_ENTRY_256B, 256usize) }
                               else if ent_s & LLQ_ENTRY_128B != 0 { (LLQ_ENTRY_128B, 128) } else { (0, 0) };
            let stride = if str_s & LLQ_STRIDE_MULTIPLE != 0 { LLQ_STRIDE_MULTIPLE }
                         else if str_s & LLQ_STRIDE_SINGLE != 0 { LLQ_STRIDE_SINGLE } else { 0 };
            if hdr_s & LLQ_HEADER_INLINE != 0 && entry != 0 && dnbh_s & LLQ_DNBH_2 != 0 && stride != 0 {
                let mut c = [0u32; 16];
                c[0] = (OP_SET_FEATURE as u32) << 16;
                // feature_version: a GET is sent with version=1, while a SET goes with 0. That is what
                // the reference does (ena_com_set_llq does not fill the field at all), and that is also
                // how a live driver's command looks in the taken trace of the device init.
                c[4] = (FEAT_LLQ as u32) << 8;
                // the payload llq_desc goes from the *_enabled fields (the upper halves of the words)
                c[7] = LLQ_HEADER_INLINE << 16;
                c[8] = entry << 16;
                c[9] = LLQ_DNBH_2 << 16;
                c[10] = stride << 16;
                // c[11] = feature_version|entry_size_recommended|max_wide_llq_depth,
                // c[12] = accel_mode.set.enabled_flags (see ena_admin_feature_llq_desc).
                let accel_sup = l[9] & 0xFFFF;            // accel_mode.get.supported_flags
                let burst_sz = (l[9] >> 16) & 0xFFFF;     // accel_mode.get.max_tx_burst_size (bytes)
                if WANT_ACCEL.load(Ordering::Relaxed) {
                    c[12] = accel_sup & (ACCEL_DISABLE_META_CACHING | ACCEL_LIMIT_TX_BURST);
                }
                if e.admin(&mut c).is_some() {
                    e.llq_entry = esz;
                    e.llq_hdr_max = esz - 2 * TX_DESC_SZ; // 2 descriptor slots before the header
                    if c[12] & ACCEL_DISABLE_META_CACHING != 0 || WANT_META.load(Ordering::Relaxed) { e.llq_meta = true; }
                    if c[12] & ACCEL_LIMIT_TX_BURST != 0 && burst_sz != 0 {
                        e.llq_burst = (burst_sz as usize / esz) as u16;
                        e.llq_burst_left = e.llq_burst;
                    }
                    crate::println!("ena: the LLQ is configured (a record of {} B, an inline header of up to {} B, stride={:#x}, accel: sup={:#x} \
                        enabled={:#x}, meta={} burst={} records)",
                        e.llq_entry, e.llq_hdr_max, stride, accel_sup, c[12], e.llq_meta, e.llq_burst);
                } else {
                    crate::println!("ena: the SET_FEATURE(LLQ) was rejected — we stay on the host descriptors");
                }
            } else {
                crate::println!("ena: the device does not support the needed combination of LLQ (hdr={:#x} entry={:#x} dnbh={:#x} stride={:#x})",
                    hdr_s, ent_s, dnbh_s, str_s);
            }
        }
    }
    let _ = e.get_feature(11);              // STATELESS_OFFLOAD_CONFIG
    let _ = e.get_feature_ver(12, 1);       // RSS_INDIRECTION_TABLE_CONFIG
    let mut c = [0u32; 16];                 // a GET_STATS (op 11) — it is in a live driver's trace
    c[0] = 11u32 << 16;
    let _ = e.admin(&mut c);

    // the AENQ: the subscription to the events (we drain them in drain_aenq, but the queue must live)
    let groups = e.get_feature(FEAT_AENQ_CONFIG).map(|r| r[2]).unwrap_or(0);
    let mut c = [0u32; 16];
    c[0] = (OP_SET_FEATURE as u32) << 16;
    c[4] = (FEAT_AENQ_CONFIG as u32) << 8;
    c[6] = groups & 0xdf;                   // as a live driver does (without KEEP_ALIVE)
    crate::println!("ena: the AENQ groups {:#x} {}", groups & 0xdf,
        if e.admin(&mut c).is_some() { "are on" } else { "are NOT on" });

    // The MTU: with ENA it is 9001 by default, our buffers are 2048 — we ask for 1500 so that a frame
    // is not cut into several descriptors (a multi-desc RX is not implemented here)
    let mut c = [0u32; 16];
    c[0] = (OP_SET_FEATURE as u32) << 16;
    c[4] = (FEAT_MTU as u32) << 8;
    c[5] = e.mtu;
    if e.admin(&mut c).is_none() { crate::println!("ena: the SET_FEATURE(MTU) did not go through — we continue"); }

    // The AENQ is "on": the head = the depth (all the records are available to the device), then
    // we drain everything it has already managed to put there (a link-up and so on)
    w32(e.regs + REG_AENQ_HEAD_DB, AENQ_DEPTH as u32);
    e.drain_aenq();

    // MSI-X: we switch it on (the Function Mask is removed, the vectors are programmed onto the BSP's LAPIC
    // and unmasked — in the IDT there stand stubs with one EOI). ENA rejects the IO queues
    // of a function with MSI-X switched off. The datapath stays polling: the interrupts are
    // silenced on the DEVICE's side (REG_INTR_MASK=1 above).
    let msix_n = crate::pci::msix_enable_masked(&d);
    let io_vec = if msix_n >= 2 { 1u32 } else { 0 };
    crate::println!("ena: MSI-X has {} vectors, io_vec={}", msix_n, io_vec);
    e.io_vec = io_vec;
    // V1.9: where exactly these interrupts arrive is now a decision rather than a default.
    // The datapath polls the rings, but the MSI-X message itself arrives somewhere all the same,
    // and that "somewhere" must not turn out to be a dedicated core of a latency-critical thread
    // (see affinity::irq_core: they will not be given there, and it will be said so).
    crate::affinity::route_device_irqs(&d, "ENA");

    // ── The IO queues: first the CQ, then an SQ onto it ─────────────────────
    let mut tx_cq_r = e.create_cq_try(tx_cq, Q_DEPTH, (TX_CDESC_SZ / 4) as u8, true, io_vec);
    if tx_cq_r.is_none() {
        let sts = e.reg_read(REG_DEV_STS);
        crate::println!("ena: the CQ was rejected, DEV_STS={:#x} (fatal={}) — I try the polling mode",
            sts, (sts >> 5) & 1);
        tx_cq_r = e.create_cq_try(tx_cq, Q_DEPTH, (TX_CDESC_SZ / 4) as u8, false, 0);
    }
    let (tx_cq_idx, tx_cq_db, tx_unmask) = tx_cq_r?;
    // §7.3: the TX SQ goes into the device's memory (an LLQ) or into the host's memory
    let want_dev = e.llq_entry != 0;
    let (_tx_sq_idx, tx_db, llq_off) = e.create_sq_p(tx_sq, Q_DEPTH, 1, tx_cq_idx,
        if want_dev { PLACEMENT_DEV } else { PLACEMENT_HOST })?;
    if want_dev {
        let (bar2, sz2) = crate::pci::bar(d.bus, d.dev, d.func, 2);
        if bar2 == 0 { crate::println!("ena: BAR2 (the device's memory) is unavailable — the LLQ is off"); e.llq_entry = 0; }
        else {
            e.llq_base = bar2 + llq_off as u64;
            // §A1: the LLQ's window is write-combining, like the devm_ioremap_wc of the reference
            // ena driver (it maps the reg BAR with an ordinary ioremap, i.e. UC). On UC
            // every one of the descriptor's 16/32 writes goes as a separate PCIe
            // transaction, and the core stands waiting; WC gathers them into 64-byte batches.
            // talaria.ena=nowc — leave UC (an A/B with the former behaviour).
            let wc = !WANT_NOWC.load(Ordering::Relaxed) && crate::paging::map_mmio_wc(bar2, sz2);
            crate::println!("ena: the LLQ is active — the ring of the descriptors is in BAR2 {:#x} (+{:#x}, the window is {} KB, {})",
                bar2, llq_off, sz2 / 1024, if wc { "write-combining" } else { "uncached" });
        }
    }
    let (rx_cq_idx, rx_cq_db, rx_unmask) = e.create_cq_try(rx_cq, RX_DEPTH, (RX_CDESC_SZ / 4) as u8, true, io_vec)?;
    let (rx_sq_idx, rx_db) = e.create_sq(rx_sq, RX_DEPTH, 2, rx_cq_idx)?;
    e.tx_db = tx_db; e.tx_cq_db = tx_cq_db; e.rx_db = rx_db; e.rx_cq_db = rx_cq_db;
    e.rx_intr_reg = rx_unmask as u64; e.tx_intr_reg = tx_unmask as u64;
    crate::println!("ena: the moderation registers: rx_intr={:#x} tx_intr={:#x}", rx_unmask, tx_unmask);
    // §A1: talaria.ena=intr — allow the interrupts to the device ONCE, here.
    // Our MSI-X vectors are unmasked anyway (the handler only sends an EOI), but
    // at the device's level the interrupts are silenced by REG_INTR_MASK=1 from the very start.
    if WANT_INTR.load(Ordering::Relaxed) {
        w32(e.regs + REG_INTR_MASK, 0);
        if e.rx_intr_reg != 0 { w32(e.regs + e.rx_intr_reg, 1 << 30); }
        if e.tx_intr_reg != 0 { w32(e.regs + e.tx_intr_reg, 1 << 30); }
        crate::println!("ena: the device's interrupts ARE ALLOWED (the intervals 0, an unmask of both CQs) — the datapath polls anyway");
    }
    // A micro-benchmark of the price of a write into the BAR: on Nitro it may be intercepted
    // by the hypervisor, and then a "call" is the datapath's most expensive operation.
    {
        let hz = crate::tsc::hz();
        let t0 = crate::tsc::rdtsc();
        for _ in 0..100 { w32(e.regs + e.tx_db, e.tx_tail as u32); }
        let d = crate::tsc::rdtsc() - t0;
        let t1 = crate::tsc::rdtsc();
        for _ in 0..100 { core::hint::black_box(crate::tsc::rdtsc()); }
        let d0 = crate::tsc::rdtsc() - t1;
        crate::println!("ena: the price of a call into the BAR: {} ns (an rdtsc for comparison is {} ns)",
            d * 1_000_000_000 / hz / 100, d0 * 1_000_000_000 / hz / 100);
    }
    // §4.5: RSS goes by the flag talaria.ena=rss (an A/B with the former behaviour)
    let rssm = WANT_RSS.load(core::sync::atomic::Ordering::SeqCst);
    if rssm != 0 { e.rss_config_parts(if rssm & 4 != 0 { rx_cq_idx } else { rx_sq_idx }, rssm & 1 != 0, rssm & 2 != 0); }
    crate::println!("ena: the IO queues are ready (the tx cq={} db={:#x}, the rx cq={} db={:#x}), the depth is {}, polling",
        tx_cq_idx, tx_db, rx_cq_idx, rx_db, Q_DEPTH);
    crate::println!("ena: the rings' depth: the TX {} / the RX {} (a live Linux driver has an RX of 1024)", Q_DEPTH, RX_DEPTH);
    Some(e) // the receive ring is filled by l1::NicQ (the frame buffers are there)
}

// ── ENA as a NetDev (the device model — netdev.rs) ──────────────────────────
// A thin wrapper: everything meaningful was already in the driver itself, while `NicQ` simply
// knew it by name. Now it does not.
impl crate::netdev::NetDev for Ena {
    fn name(&self) -> &'static str { "ena" }
    fn mac(&self) -> [u8; 6] { self.mac }
    /// One less than the ring's depth: a full ring is indistinguishable from an empty one.
    fn rx_capacity(&self, want: usize) -> usize { want.min(RX_RING as usize - 1) }
    fn rx_post(&mut self, slot: usize, addr: u64) { self.receive_begin(slot, addr); }
    fn rx_poll(&mut self) -> Option<(usize, usize, usize)> {
        match self.poll_receive() {
            Some((slot, len)) => Some((slot, 0, len)),
            // It is empty — at the same time drain the device's event queue: at its
            // overflow ENA goes into a reset, and the datapath dies silently.
            None => { self.drain_aenq(); self.stats_tick(); None }
        }
    }
    fn tx_alloc(&mut self, ntx: usize) -> Option<usize> { self.tx_slot().filter(|&i| i < ntx) }
    fn tx_post(&mut self, slot: usize, addr: u64, n: usize) { self.transmit_no_notify(slot, addr, n); }
    fn tx_reap(&mut self) { self.reap_tx(); }
    fn tx_watch(&mut self) { if self.tx_free_n != TX_RING { self.reap_tx(); } }
    // Ena::... — in order to call the card's OWN method rather than this same one from the trait.
    fn flush(&mut self) -> bool { Ena::flush(self) }
    fn arm_intr(&mut self) { Ena::arm_intr(self); }
    fn arm_reg(&self) -> Option<(usize, u32)> {
        if self.rx_intr_reg == 0 { None } else { Some(((self.regs + self.rx_intr_reg) as usize, 1 << 30)) }
    }
    fn irq_vector(&mut self) -> Option<u8> { Ena::irq_vector(self) }
    fn idle_watch(&self) -> Option<(*const u32, u32)> { Some(self.rx_cq_watch()) }
    /// IS THERE A RECEIVED FRAME — by the same phase bit by which
    /// `poll_receive` takes it, and therefore this answer cannot lie. The head and the phase
    /// are taken from an ATOMIC snapshot (`rx_watch`) rather than from the fields: we are asked
    /// without the stack's lock.
    fn rx_ready(&self) -> Option<bool> {
        let w = self.rx_watch.load(core::sync::atomic::Ordering::Acquire);
        let head = (w & 0xFFFF) as u16;
        let phase = w >> 16 != 0;
        let idx = (head & (RX_DEPTH - 1)) as u64;
        let st = unsafe { core::ptr::read_volatile(crate::mem::kptr::<u32>(self.rx_cq + idx * RX_CDESC_SZ as u64)) };
        Some(((st >> 24) & 1 != 0) == phase)
    }
    fn doorbells(&self) -> Option<u64> { Some(self.db_writes) }
}

/// The slots of the live cards: an EC2 instance sometimes has several network interfaces, and
/// every one is its own card with its own rings. An instance lives until a reboot.
const NSLOT: usize = 4;
static mut SLOT: [Option<Ena>; NSLOT] = [None, None, None, None];

fn probe_drv(dev: Option<&crate::pci::PciDev>, inst: usize) -> Option<&'static mut dyn crate::netdev::NetDev> {
    if inst >= NSLOT { return None; }
    let e = match dev { Some(d) => probe_at(d)?, None => probe()? };
    unsafe {
        let p = core::ptr::addr_of_mut!(SLOT[inst]);
        core::ptr::write(p, Some(e));
        Some((*p).as_mut().unwrap() as &mut dyn crate::netdev::NetDev)
    }
}

pub static DRIVER: crate::netdev::Driver = crate::netdev::Driver {
    name: "ena",
    // The list is the same as `probe` walked: the PF, the VF and two old ids.
    bind: &[
        crate::netdev::Bind::Pci(ENA_VENDOR, 0xec20),
        crate::netdev::Bind::Pci(ENA_VENDOR, 0xec21),
        crate::netdev::Bind::Pci(ENA_VENDOR, 0x0ec2),
        crate::netdev::Bind::Pci(ENA_VENDOR, 0x1ec2),
    ],
    probe: probe_drv,
};
