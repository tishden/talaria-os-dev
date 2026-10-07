//! The GICv3 Interrupt Translation Service: MSI-X on arm64 (docs/ARM64-PLAN.md §4.8).
//!
//! A message-signalled interrupt on this machine is not an address that names a core, as
//! the LAPIC's `0xFEE0_0000 | apic << 12` is. The device writes its EVENT number to ONE
//! register (`GITS_TRANSLATER`), the ITS looks the pair (device id, event) up in tables the
//! kernel gave it and delivers an LPI — an INTID from 8192 up — to the redistributor of the
//! core the pair's COLLECTION is mapped to. So "route this vector to that core" is a command
//! to the ITS (`MAPTI`, then `MOVI`), not a rewrite of the device's table.
//!
//! What is set up, once, on the boot core (`init`):
//! * the LPI configuration table (a byte per LPI: the priority and the enable bit), shared
//!   by every redistributor, and a pending table per redistributor (`enable_lpis_here`,
//!   called by every core for itself: `GICR_PROPBASER`/`PENDBASER`, then `EnableLPIs`);
//! * the ITS's own tables (`GITS_BASER<n>`: devices and collections, flat) and its command
//!   queue (`GITS_CBASER`), then `GITS_CTLR.Enabled`;
//! * a collection per core (`MAPC`), numbered by the logical core and mapped when a core
//!   gets its first vector.
//!
//! What a driver asks for (`msi`): the first vector of a device maps the device (`MAPD` with
//! an interrupt translation table sized for its vector count), every vector gets an LPI
//! (`MAPTI`), its configuration byte is enabled (`INV`), and the answer is the pair the
//! device's MSI-X entry must hold: the address of `GITS_TRANSLATER` and the event number.
//!
//! The device id is the PCIe requester id `bus << 8 | dev << 3 | func` — true without an
//! SMMU in the path (QEMU `virt`; on Graviton the IORT table says the same or an offset —
//! phase 6 reads it).
//!
//! # Safety
//!
//! `init` runs once on the boot core after the GIC is up and the frame allocator works;
//! the command queue is written under `LOCK` (the drivers come up one at a time at boot,
//! `affinity` retargets at run time). The frames come from the device tree.

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use super::gic;

const GITS_CTLR: u64 = 0x000;
const GITS_TYPER: u64 = 0x008;
const GITS_CBASER: u64 = 0x080;
const GITS_CWRITER: u64 = 0x088;
const GITS_CREADR: u64 = 0x090;
const GITS_BASER: u64 = 0x100;
const GITS_TRANSLATER: u64 = 0x1_0040;

const GICR_CTLR: u64 = 0x000;
const GICR_PROPBASER: u64 = 0x070;
const GICR_PENDBASER: u64 = 0x078;
const GICR_ENABLE_LPIS: u32 = 1;

/// The first LPI and how many INTID bits the tables cover: 14 bits = INTIDs up to 16383,
/// that is 8192 LPIs — the smallest the architecture allows a property table to describe.
pub const LPI_BASE: u32 = 8192;
const ID_BITS: u64 = 14;
const PROP_BYTES: u64 = (1 << ID_BITS) - LPI_BASE as u64;
const PEND_BYTES: u64 = (1 << ID_BITS) / 8;
/// How many LPIs the kernel hands out (a vector each): the handler table below.
pub const MAX_LPI: usize = 512;
/// Inner-shareable, write-back cacheable: what the tables' attributes say (the kernel's RAM).
/// The fields sit in different places: the redistributor's PROPBASER/PENDBASER carry the
/// inner cacheability in bits 9:7, the ITS's BASER/CBASER in bits 61:59 (bits 9:8 of a BASER
/// are the PAGE SIZE — the first version put the cache bits there and asked for 16 KiB pages).
const RD_ATTRS: u64 = (1 << 10) | (7 << 7);
const ITS_ATTRS: u64 = (1 << 10) | (7 << 59);
/// An LPI's configuration byte: the kernel's one priority, group 1, enabled or not.
const CFG_OFF: u8 = 0x80;
const CFG_ON: u8 = 0x80 | 1;

static BASE: AtomicU64 = AtomicU64::new(0);
static READY: AtomicBool = AtomicBool::new(false);
static PROP: AtomicU64 = AtomicU64::new(0);
static CMDQ: AtomicU64 = AtomicU64::new(0);
const CMDQ_BYTES: u64 = 0x1_0000;
static ITT_ESZ: AtomicU64 = AtomicU64::new(8);
static DEV_BITS: AtomicU64 = AtomicU64::new(16);
static PTA: AtomicBool = AtomicBool::new(false);
static LOCK: AtomicBool = AtomicBool::new(false);
static NEXT_LPI: AtomicU32 = AtomicU32::new(0);
/// The handler key ("vector") of every LPI handed out; `u16::MAX` — count it and end it.
static LPI_VEC: [AtomicU16; MAX_LPI] = [const { AtomicU16::new(u16::MAX) }; MAX_LPI];
/// (device id << 32 | event) + 1 of every LPI handed out; 0 — the slot is free.
static LPI_SRC: [AtomicU64; MAX_LPI] = [const { AtomicU64::new(0) }; MAX_LPI];
/// LPIs taken, by core, and the commands the ITS did not finish in time (normally 0).
pub static LPIS: [AtomicU64; super::smp::MAXCPU] = [const { AtomicU64::new(0) }; super::smp::MAXCPU];
pub static STALLS: AtomicU64 = AtomicU64::new(0);

/// The devices already mapped: (device id + 1, the event bits of its table).
const MAX_ITS_DEV: usize = 32;
static DEVS: [AtomicU64; MAX_ITS_DEV] = [const { AtomicU64::new(0) }; MAX_ITS_DEV];

#[inline] unsafe fn r64(a: u64) -> u64 { core::ptr::read_volatile(crate::mem::devptr::<u64>(a)) }
#[inline] unsafe fn w64(a: u64, v: u64) { core::ptr::write_volatile(crate::mem::devptr::<u64>(a), v) }
#[inline] unsafe fn r32(a: u64) -> u32 { core::ptr::read_volatile(crate::mem::devptr::<u32>(a)) }
#[inline] unsafe fn w32(a: u64, v: u32) { core::ptr::write_volatile(crate::mem::devptr::<u32>(a), v) }

pub fn present() -> bool { READY.load(Ordering::Acquire) }
/// Where a device must write its event number.
/// `BASE` is the kernel's pointer (the devices' alias); the device is told the physical one.
/// The kernel's pointer to the ITS's registers (0: not up) — the devices' window gate.
pub fn base() -> u64 { BASE.load(Ordering::Relaxed) }
pub fn translater() -> u64 { super::mmu::mmio_pa(BASE.load(Ordering::Relaxed)) + GITS_TRANSLATER }

fn lock() { while LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() { core::hint::spin_loop(); } }
fn unlock() { LOCK.store(false, Ordering::Release); }

/// Zeroed, contiguous, aligned frames (the ITS and the redistributors take 64 KiB alignment).
fn table(bytes: u64, align: u64) -> Option<u64> {
    let pages = bytes.div_ceil(4096);
    let p = crate::mem::alloc_any_aligned(pages, align)?;
    // SAFETY: the frames were just allocated and are identity-mapped RAM.
    unsafe { core::ptr::write_bytes(crate::mem::kptr::<u8>(p), 0, (pages * 4096) as usize); }
    Some(p)
}

/// One command (four little-endian words) onto the queue; the caller holds `LOCK`.
unsafe fn push(c: [u64; 4]) {
    let base = BASE.load(Ordering::Relaxed);
    let q = CMDQ.load(Ordering::Relaxed);
    let w = r64(base + GITS_CWRITER) & 0xF_FFE0;
    for (i, v) in c.iter().enumerate() { core::ptr::write_volatile(crate::mem::kptr::<u64>(q + w + 8 * i as u64), *v); }
    // The ITS reads the queue as memory: the words must be there before the pointer moves.
    // It is not one of the cores — a `dmb ish` (`fence`) does not order for it; `dsb st`
    // completes the stores for every observer before the MMIO write (Linux: `its_flush_cmd`).
    core::arch::asm!("dsb st", options(nostack, preserves_flags));
    w64(base + GITS_CWRITER, (w + 32) % CMDQ_BYTES);
}
/// Wait until the ITS has consumed everything written (it is synchronous under QEMU and
/// takes microseconds on a machine). A stall is counted and named, never waited for forever.
unsafe fn drain() -> bool {
    let base = BASE.load(Ordering::Relaxed);
    let t0 = super::cpu::counter();
    let budget = super::timer::ticks_for_ns(1_000_000_000);
    loop {
        let r = r64(base + GITS_CREADR);
        if r & 0xF_FFE0 == r64(base + GITS_CWRITER) & 0xF_FFE0 { return true; }
        if r & 1 != 0 || super::cpu::counter().wrapping_sub(t0) > budget {
            STALLS.fetch_add(1, Ordering::Relaxed);
            crate::println!("its: the command queue stalled (creadr={:#x} cwriter={:#x})", r, r64(base + GITS_CWRITER));
            return false;
        }
        core::hint::spin_loop();
    }
}

/// What `MAPC`/`SYNC` name a redistributor by: its address, or its processor number.
fn rdbase(frame: u64, typer: u64) -> u64 {
    // the frame is the kernel's pointer; the ITS is told the physical address
    if PTA.load(Ordering::Relaxed) { super::mmu::mmio_pa(frame) & !0xFFFF } else { ((typer >> 8) & 0xFFFF) << 16 }
}
fn rdbase_of_cpu(cpu: usize) -> Option<u64> {
    let mpidr = super::smp::mpidr_of(cpu)?;
    gic::redistributor_of(mpidr).map(|(f, t)| rdbase(f, t))
}
/// The collections already mapped, by logical core (a collection IS a core: `MAPC` names its
/// redistributor). Mapped on a core's first vector — the logical numbering does not exist
/// yet when the ITS comes up.
static COLLECTIONS: AtomicU64 = AtomicU64::new(0);
unsafe fn collection(cpu: usize) -> Option<u64> {
    if cpu >= 64 { return None; }
    let rd = rdbase_of_cpu(cpu)?;
    if COLLECTIONS.load(Ordering::Relaxed) & (1 << cpu) == 0 {
        push([0x09, 0, (1 << 63) | rd | cpu as u64, 0]);       // MAPC
        push([0x05, 0, rd, 0]);                                 // SYNC
        COLLECTIONS.fetch_or(1 << cpu, Ordering::Relaxed);
    }
    Some(rd)
}

/// This core's redistributor takes LPIs: the shared configuration table, a pending table
/// of its own, then the enable bit (which is write-once on real hardware — the order matters).
/// # Safety
/// On the core it configures, after `init` (the configuration table exists).
pub unsafe fn enable_lpis_here() -> bool {
    let prop = PROP.load(Ordering::Relaxed);
    if prop == 0 { return false; }
    let Some(f) = gic::my_frame() else { return false; };
    if r32(f + GICR_CTLR) & GICR_ENABLE_LPIS != 0 { return true; }
    let Some(pend) = table(PEND_BYTES.max(0x1_0000), 0x1_0000) else { return false; };
    w64(f + GICR_PROPBASER, prop | RD_ATTRS | (ID_BITS - 1));
    w64(f + GICR_PENDBASER, pend | RD_ATTRS | (1 << 62));   // PTZ: the table is zero
    w32(f + GICR_CTLR, r32(f + GICR_CTLR) | GICR_ENABLE_LPIS);
    true
}

/// Bring the ITS at `base` up. `false` (with the reason printed): no LPIs on this GIC, no
/// memory, or the ITS refused a table.
/// # Safety
/// Once, on the boot core, after the GIC's distributor and this core's interface are up.
pub unsafe fn init(base: u64) -> bool {
    if base == 0 || READY.load(Ordering::Relaxed) { return READY.load(Ordering::Relaxed); }
    if !gic::lpis_supported() { crate::println!("its: the distributor has no LPIs — MSI-X is unavailable"); return false; }
    let pa = base;
    let base = super::mmu::mmio_va(pa);
    BASE.store(base, Ordering::Relaxed);
    let typer = r64(base + GITS_TYPER);
    ITT_ESZ.store(((typer >> 4) & 0xF) + 1, Ordering::Relaxed);
    DEV_BITS.store((((typer >> 13) & 0x1F) + 1).min(16), Ordering::Relaxed);
    PTA.store(typer & (1 << 19) != 0, Ordering::Relaxed);
    w32(base + GITS_CTLR, 0);
    // The LPI configuration table: every LPI at the kernel's priority, disabled.
    let Some(prop) = table(PROP_BYTES, 0x1_0000) else { crate::println!("its: no memory for the LPI tables"); return false; };
    core::ptr::write_bytes(crate::mem::kptr::<u8>(prop), CFG_OFF, PROP_BYTES as usize);
    PROP.store(prop, Ordering::Relaxed);
    // The ITS's tables: flat, 64 KiB pages (what every ITS has), sized for the ids we use.
    for n in 0..8u64 {
        let reg = base + GITS_BASER + 8 * n;
        let cur = r64(reg);
        let typ = (cur >> 56) & 7;
        let esz = ((cur >> 48) & 0x1F) + 1;
        let entries = match typ {
            1 => 1u64 << DEV_BITS.load(Ordering::Relaxed),    // devices, by device id
            4 => 256,                                          // collections, by core
            _ => continue,
        };
        let bytes = (entries * esz).div_ceil(0x1_0000) * 0x1_0000;
        let Some(t) = table(bytes, 0x1_0000) else { crate::println!("its: no memory for the table {}", n); return false; };
        let want = (1 << 63) | (cur & (7 << 56)) | (cur & (0x1F << 48)) | t | ITS_ATTRS | (2 << 8) | (bytes / 0x1_0000 - 1);
        w64(reg, want);
        let got = r64(reg);
        if got >> 63 == 0 || (got >> 8) & 3 != 2 {
            crate::println!("its: GITS_BASER{} refused the table (wrote {:#x}, reads {:#x})", n, want, got);
            return false;
        }
    }
    let Some(q) = table(CMDQ_BYTES, 0x1_0000) else { crate::println!("its: no memory for the command queue"); return false; };
    CMDQ.store(q, Ordering::Relaxed);
    w64(base + GITS_CBASER, (1 << 63) | q | ITS_ATTRS | (CMDQ_BYTES / 4096 - 1));
    w64(base + GITS_CWRITER, 0);
    w32(base + GITS_CTLR, 1);
    if !enable_lpis_here() { crate::println!("its: this core's redistributor did not take the LPI tables"); return false; }
    let ok = drain();
    READY.store(ok, Ordering::Release);
    crate::println!("its: {:#x} is up: device ids of {} bits, a translation entry of {} B, LPIs {}..{} ({})",
        pa, DEV_BITS.load(Ordering::Relaxed), ITT_ESZ.load(Ordering::Relaxed), LPI_BASE, LPI_BASE as usize + MAX_LPI - 1,
        if ok { "the commands were taken" } else { "THE COMMAND QUEUE STALLED" });
    ok
}

/// The event bits of a mapped device, mapping it (`MAPD`) on its first vector.
unsafe fn device_bits(devid: u32, nvec: u32) -> Option<u64> {
    for d in DEVS.iter() {
        let v = d.load(Ordering::Relaxed);
        if v != 0 && (v >> 8) as u32 == devid + 1 { return Some(v & 0xFF); }
    }
    let bits = (32 - (nvec.max(2) - 1).leading_zeros()) as u64;         // ceil(log2), at least 1
    let bytes = (1u64 << bits) * ITT_ESZ.load(Ordering::Relaxed);
    let itt = table(bytes, 4096)?;
    let slot = DEVS.iter().find(|d| d.load(Ordering::Relaxed) == 0)?;
    push([0x08 | (devid as u64) << 32, bits - 1, (1 << 63) | itt, 0]);
    slot.store(((devid as u64 + 1) << 8) | bits, Ordering::Relaxed);
    Some(bits)
}

/// The message for vector `event` of the device `devid` (of `nvec` vectors), delivered to
/// the logical core `cpu` as the handler key `vector`: the pair (the address the device
/// writes to, the data it writes). An event already mapped is MOVED to `cpu` and keeps its
/// LPI; `vector = None` keeps its handler key too.
pub fn msi(devid: u32, event: u32, nvec: u32, cpu: usize, vector: Option<u16>) -> Option<(u64, u32)> {
    if !present() || devid >> DEV_BITS.load(Ordering::Relaxed) != 0 { return None; }
    lock();
    // SAFETY: the queue and the tables under `LOCK`; `init` has run (`present`).
    let r = unsafe {
        (|| {
            let rd = collection(cpu)?;
            let bits = device_bits(devid, nvec)?;
            if event as u64 >= 1 << bits { return None; }
            let src = ((devid as u64) << 32 | event as u64) + 1;
            let n = NEXT_LPI.load(Ordering::Relaxed) as usize;
            let known = (0..n).find(|&i| LPI_SRC[i].load(Ordering::Relaxed) == src);
            let idx = match known {
                Some(i) => {
                    push([0x01 | (devid as u64) << 32, event as u64, cpu as u64, 0]);              // MOVI
                    i
                }
                None => {
                    if n >= MAX_LPI { return None; }
                    NEXT_LPI.store(n as u32 + 1, Ordering::Relaxed);
                    LPI_SRC[n].store(src, Ordering::Relaxed);
                    // The vector before the mapping: the first LPI may arrive the moment
                    // the ITS has the MAPTI, and `on_lpi` must find where to go.
                    if let Some(v) = vector { LPI_VEC[n].store(v, Ordering::Release); }
                    push([0x0A | (devid as u64) << 32, event as u64 | (LPI_BASE as u64 + n as u64) << 32, cpu as u64, 0]); // MAPTI
                    core::ptr::write_volatile(crate::mem::kptr::<u8>(PROP.load(Ordering::Relaxed) + n as u64), CFG_ON);
                    push([0x0C | (devid as u64) << 32, event as u64, 0, 0]);                         // INV
                    n
                }
            };
            if let Some(v) = vector { LPI_VEC[idx].store(v, Ordering::Release); }
            push([0x05, 0, rd, 0]);                                                                  // SYNC
            if !drain() { return None; }
            Some((translater(), event))
        })()
    };
    unlock();
    r
}

/// An LPI was acknowledged on `cpu`: count it and call the handler its vector names.
#[inline]
pub fn on_lpi(intid: u32, cpu: usize) {
    if cpu < LPIS.len() { LPIS[cpu].fetch_add(1, Ordering::Relaxed); }
    let i = (intid - LPI_BASE) as usize;
    if i >= MAX_LPI { return; }
    let v = LPI_VEC[i].load(Ordering::Acquire);
    if v == u16::MAX { return; }
    if let Some(h) = super::idt::handler_of(v as u32) { h(); }
}

// ── talaria.test=its ─────────────────────────────────────────────────────────────────────
/// The test's handler: which core took the vector.
static HITS: [AtomicU64; super::smp::MAXCPU] = [const { AtomicU64::new(0) }; super::smp::MAXCPU];
extern "C" fn test_handler() {
    let c = super::cpu::cpu_index();
    if c < HITS.len() { HITS[c].fetch_add(1, Ordering::Relaxed); }
}
fn spin_ms(ms: u64) {
    let t0 = super::cpu::counter();
    let n = super::timer::ticks_for_ns(ms * 1_000_000);
    while super::cpu::counter().wrapping_sub(t0) < n { core::hint::spin_loop(); }
}

/// `talaria.test=its` (with `talaria.pci=1` and an NVMe controller on the bus): a device's
/// MSI-X vector is mapped through the ITS, fired by the device, and must arrive as an LPI
/// on the core it was routed to — then on another core after a re-route. The NVMe admin
/// queue is the trigger: its completions always interrupt on vector 0.
pub fn selftest() -> ! {
    let mut fail = 0usize;
    let mut check = |cond: bool, what: &str| {
        if cond { crate::println!("its: OK   {}", what); } else { crate::println!("its: FAILURE {}", what); fail += 1; }
    };
    crate::println!("talaria: the self-test of MSI-X through the ITS (an NVMe admin completion as an LPI)");
    check(present(), "the ITS is up: its tables, the command queue, LPIs enabled on this core's redistributor");
    let Some(d) = crate::pci::find_class(0x010802) else {
        crate::println!("its: no NVMe controller on the bus (-device nvme and talaria.pci=1 are needed) — the check is impossible");
        crate::println!("its: THERE ARE FAILURES");
        crate::suite::park()
    };
    check(d.msix_cap != 0, "the controller has an MSI-X capability");
    let vec0 = crate::idt::MSI_VECTOR_BASE;
    super::idt::set_vector(vec0, crate::mem::vaddr(test_handler as *const ()));
    let nvec = crate::pci::msix_enable_masked(&d);
    let addr = crate::pci::msix_msg_addr(&d, 0);
    crate::println!("its: {:02x}:{:02x}.{} — {} vector(s) mapped; entry 0 reads the address {:#x} (GITS_TRANSLATER is {:#x})",
        d.bus, d.dev, d.func, nvec, addr, translater());
    check(nvec > 0 && addr == translater() as u32, "the MSI-X entry holds the address of the translation register, read back from the device");
    // The admin commands of the bring-up complete with interrupts on vector 0.
    let before = (HITS[0].load(Ordering::Relaxed), LPIS[0].load(Ordering::Relaxed));
    unsafe { super::cpu::irq_enable(); }
    let n = crate::nvme::probe_nth(0);
    spin_ms(50);
    let (h0, l0) = (HITS[0].load(Ordering::Relaxed) - before.0, LPIS[0].load(Ordering::Relaxed) - before.1);
    crate::println!("its: the controller's bring-up: {} LPI(s) on core 0, the handler of vector {:#x} ran {} time(s)", l0, vec0, h0);
    check(n.is_some(), "the controller came up with MSI-X enabled");
    check(l0 > 0 && h0 == l0, "the admin completions arrived as LPIs on core 0 and reached the vector's handler");
    // The re-route: every vector to another core (if there is one under the scheduler).
    let target = if crate::smp::nr_cpus() > 1 && crate::percpu::ONLINE.load(Ordering::SeqCst) & 2 != 0 { 1 } else { 0 };
    if let (Some(mut n), 1) = (n, target) {
        let a = crate::pci::msix_route(&d, target);
        let (b0, b1) = (HITS[0].load(Ordering::Relaxed), HITS[1].load(Ordering::Relaxed));
        let mut done = 0;
        for _ in 0..8 { if n.admin_ping() { done += 1; } }
        spin_ms(50);
        let (d0, d1) = (HITS[0].load(Ordering::Relaxed) - b0, HITS[1].load(Ordering::Relaxed) - b1);
        crate::println!("its: the route to core {} (affinity {:#x}): {} admin command(s) -> {} interrupt(s) on core 1, {} on core 0", target, a, done, d1, d0);
        check(done == 8, "the admin commands complete after the re-route");
        // Not "8": an LPI is one pending bit, and two completions that land before the core
        // takes the first are ONE interrupt (the device's queue is read by polling here).
        check((1..=8).contains(&d1) && d0 == 0, "after the re-route (MOVI) the LPIs arrive on core 1 and not one on core 0");
        check(crate::pci::msix_msg_addr(&d, 0) == translater() as u32, "the entry still holds the translation register: the route is the ITS's, not the device's");
    } else {
        crate::println!("its: one core online — the re-route is not checked (talaria.smp=2 or more)");
    }
    check(STALLS.load(Ordering::Relaxed) == 0, "the ITS took every command (no stall of the queue)");
    unsafe { super::cpu::irq_disable(); }
    crate::println!("its: {}", if fail == 0 { "ALL CHECKS PASSED" } else { "THERE ARE FAILURES" });
    crate::suite::park()
}
