//! The GICv3 interrupt controller (docs/ARM64-PLAN.md §4.8), the parts a core needs
//! to take a timer tick and an IPI: the distributor (`GICD_*`, one frame), one
//! redistributor per core (`GICR_*`, two 64 KiB frames each, found by the core's
//! affinity), and the CPU interface, which on v3 is system registers (`ICC_*`) —
//! acknowledging and ending an interrupt is an `mrs`/`msr`, not an MMIO window,
//! which is why the PV EOI trick of the x86 tick has no counterpart here.
//!
//! INTIDs: SGIs 0..15 (the IPIs), PPIs 16..31 (per core: the timer is 27), SPIs
//! 32..1019 (wired lines: virtio-mmio on `virt`), LPIs 8192.. (MSI-X through the ITS —
//! `its.rs`). Everything is put in group 1 (the only group on a machine with one
//! security state, which is what QEMU `virt` and a Nitro guest are).
//!
//! # Safety
//!
//! `init_distributor` runs once, on the boot core, before any other core exists;
//! `init_cpu` runs on the core it configures (the redistributor frame is chosen by
//! that core's own MPIDR). The MMIO addresses come from the device tree or ACPI and
//! must be mapped Device (block 0 of the early map covers `virt`'s).

use core::sync::atomic::{AtomicU64, Ordering};

static GICD: AtomicU64 = AtomicU64::new(0);
static GICR: AtomicU64 = AtomicU64::new(0);
static GICR_END: AtomicU64 = AtomicU64::new(0);

// The distributor's registers.
const GICD_CTLR: u64 = 0x000;
const GICD_TYPER: u64 = 0x004;
const GICD_IGROUPR: u64 = 0x080;
const GICD_ISENABLER: u64 = 0x100;
const GICD_ICENABLER: u64 = 0x180;
const GICD_IPRIORITYR: u64 = 0x400;
const GICD_IROUTER: u64 = 0x6100;
const CTLR_ENABLE_G1: u32 = 1 << 1;
const CTLR_ARE: u32 = 1 << 4;
const CTLR_RWP: u32 = 1 << 31;

// A redistributor: the RD frame, then the SGI frame 64 KiB above it.
const GICR_FRAME: u64 = 0x2_0000;
const GICR_TYPER: u64 = 0x008;
const GICR_WAKER: u64 = 0x014;
const WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
const TYPER_LAST: u64 = 1 << 4;
const SGI_BASE: u64 = 0x1_0000;
const GICR_IGROUPR0: u64 = 0x080;
const GICR_ISENABLER0: u64 = 0x100;
const GICR_ICENABLER0: u64 = 0x180;
const GICR_IPRIORITYR: u64 = 0x400;

/// The priority every interrupt gets: one level, no pre-emption between them —
/// the kernel masks at the core, not at the controller.
const PRIORITY: u8 = 0x80;
/// What `ack` returns when nothing is pending.
pub const SPURIOUS: u32 = 1023;

#[inline] unsafe fn r32(a: u64) -> u32 { core::ptr::read_volatile(crate::mem::devptr::<u32>(a)) }
#[inline] unsafe fn w32(a: u64, v: u32) { core::ptr::write_volatile(crate::mem::devptr::<u32>(a), v) }
#[inline] unsafe fn w64(a: u64, v: u64) { core::ptr::write_volatile(crate::mem::devptr::<u64>(a), v) }

/// Where the frames are (from the tree or the MADT).
/// The frames are kept as the kernel's pointers (`mmu::mmio_va`: the devices' alias for GB0).
pub fn set_frames(gicd: u64, gicr: u64, gicr_size: u64) {
    let (gicd, gicr) = (super::mmu::mmio_va(gicd), super::mmu::mmio_va(gicr));
    GICD.store(gicd, Ordering::Relaxed);
    GICR.store(gicr, Ordering::Relaxed);
    GICR_END.store(gicr + gicr_size, Ordering::Relaxed);
}
pub fn present() -> bool { GICD.load(Ordering::Relaxed) != 0 }
/// The kernel's pointers to the distributor and the first redistributor (the devices' window gate).
pub fn frames() -> (u64, u64) { (GICD.load(Ordering::Relaxed), GICR.load(Ordering::Relaxed)) }
/// The distributor's TYPER, read through the kernel's pointer (0: no GIC).
pub fn typer() -> u32 { let d = GICD.load(Ordering::Relaxed); if d == 0 { 0 } else { unsafe { r32(d + GICD_TYPER) } } }

/// The distributor: affinity routing on, group 1 enabled, every SPI in group 1 at
/// one priority, disabled. Returns the number of SPIs the controller implements.
pub unsafe fn init_distributor() -> u32 {
    let d = GICD.load(Ordering::Relaxed);
    let typer = r32(d + GICD_TYPER);
    let nlines = 32 * ((typer & 0x1F) + 1);
    w32(d + GICD_CTLR, 0);
    while r32(d + GICD_CTLR) & CTLR_RWP != 0 {}
    for i in 1..(nlines / 32) as u64 {
        w32(d + GICD_ICENABLER + 4 * i, 0xFFFF_FFFF);
        w32(d + GICD_IGROUPR + 4 * i, 0xFFFF_FFFF);
    }
    for i in 8..(nlines / 4) as u64 {
        w32(d + GICD_IPRIORITYR + 4 * i, u32::from_ne_bytes([PRIORITY; 4]));
    }
    w32(d + GICD_CTLR, CTLR_ARE | CTLR_ENABLE_G1);
    while r32(d + GICD_CTLR) & CTLR_RWP != 0 {}
    nlines - 32
}

/// This core's redistributor frame: the one whose `TYPER` names our affinity.
fn my_redistributor() -> Option<u64> {
    let me = super::cpu::mpidr();
    let mut f = GICR.load(Ordering::Relaxed);
    let end = GICR_END.load(Ordering::Relaxed);
    if f == 0 { return None; }
    loop {
        if f + GICR_FRAME > end { return None; }
        let typer = unsafe { core::ptr::read_volatile(crate::mem::devptr::<u64>(f + GICR_TYPER)) };
        let aff = typer >> 32;
        let aff_mpidr = (aff & 0xFF_FFFF) | ((aff >> 24) << 32);
        if aff_mpidr == me { return Some(f); }
        if typer & TYPER_LAST != 0 { return None; }
        f += GICR_FRAME;
    }
}

/// This core's redistributor frame, for the LPI tables (`its`).
pub fn my_frame() -> Option<u64> { my_redistributor() }
/// The redistributor of the core with `mpidr`: (its frame, its `GICR_TYPER`).
pub fn redistributor_of(mpidr: u64) -> Option<(u64, u64)> {
    let mut f = GICR.load(Ordering::Relaxed);
    let end = GICR_END.load(Ordering::Relaxed);
    if f == 0 { return None; }
    loop {
        if f + GICR_FRAME > end { return None; }
        let typer = unsafe { core::ptr::read_volatile(crate::mem::devptr::<u64>(f + GICR_TYPER)) };
        let aff = typer >> 32;
        if (aff & 0xFF_FFFF) | ((aff >> 24) << 32) == mpidr { return Some((f, typer)); }
        if typer & TYPER_LAST != 0 { return None; }
        f += GICR_FRAME;
    }
}
/// Does the distributor implement LPIs (`GICD_TYPER.LPIS`) with at least 14 INTID bits?
pub fn lpis_supported() -> bool {
    let d = GICD.load(Ordering::Relaxed);
    if d == 0 { return false; }
    let typer = unsafe { r32(d + GICD_TYPER) };
    typer & (1 << 17) != 0 && ((typer >> 19) & 0x1F) + 1 >= 14
}

/// This core: wake its redistributor, put its SGIs and PPIs in group 1 at one
/// priority (all disabled), then the CPU interface — system-register access on,
/// the priority mask open, group 1 enabled. `false` if no redistributor claims us.
pub unsafe fn init_cpu() -> bool {
    let Some(f) = my_redistributor() else { return false; };
    let waker = r32(f + GICR_WAKER);
    w32(f + GICR_WAKER, waker & !WAKER_PROCESSOR_SLEEP);
    while r32(f + GICR_WAKER) & WAKER_CHILDREN_ASLEEP != 0 {}
    let sgi = f + SGI_BASE;
    w32(sgi + GICR_ICENABLER0, 0xFFFF_FFFF);
    w32(sgi + GICR_IGROUPR0, 0xFFFF_FFFF);
    for i in 0..8u64 { w32(sgi + GICR_IPRIORITYR + 4 * i, u32::from_ne_bytes([PRIORITY; 4])); }
    core::arch::asm!(
        "mrs {t}, icc_sre_el1",
        "orr {t}, {t}, #1",
        "msr icc_sre_el1, {t}",
        "isb",
        "mov {t}, #0xff",
        "msr icc_pmr_el1, {t}",
        "mov {t}, #1",
        "msr icc_igrpen1_el1, {t}",
        "isb",
        t = out(reg) _, options(nostack),
    );
    // A core that comes up after the ITS takes LPIs as the boot core does.
    if super::its::present() { super::its::enable_lpis_here(); }
    true
}

/// Enable a PPI or an SGI (INTID < 32) on THIS core.
pub unsafe fn enable_local(intid: u32) {
    if let Some(f) = my_redistributor() { w32(f + SGI_BASE + GICR_ISENABLER0, 1 << (intid & 31)); }
}

/// Enable an SPI and route it to the core with `mpidr`.
pub unsafe fn enable_spi(intid: u32, mpidr: u64) {
    let d = GICD.load(Ordering::Relaxed);
    let aff = (mpidr & 0xFF_FFFF) | ((mpidr >> 32) << 32);
    w64(d + GICD_IROUTER + 8 * intid as u64, aff);
    w32(d + GICD_ISENABLER + 4 * (intid / 32) as u64, 1 << (intid & 31));
}

const GICR_ISPENDR0: u64 = 0x200;
const GICR_ISACTIVER0: u64 = 0x300;
const GICD_ISPENDR: u64 = 0x200;
const GICD_ISACTIVER: u64 = 0x300;
/// Is `intid` pending (an SGI/PPI on this core's redistributor, an SPI at the distributor)?
pub fn is_pending(intid: u32) -> bool {
    unsafe {
        if intid < 32 { my_redistributor().map(|f| r32(f + SGI_BASE + GICR_ISPENDR0) & (1 << intid) != 0).unwrap_or(false) }
        else { r32(GICD.load(Ordering::Relaxed) + GICD_ISPENDR + 4 * (intid / 32) as u64) & (1 << (intid & 31)) != 0 }
    }
}
/// Is `intid` active (acknowledged, not yet ended)?
pub fn is_active(intid: u32) -> bool {
    unsafe {
        if intid < 32 { my_redistributor().map(|f| r32(f + SGI_BASE + GICR_ISACTIVER0) & (1 << intid) != 0).unwrap_or(false) }
        else { r32(GICD.load(Ordering::Relaxed) + GICD_ISACTIVER + 4 * (intid / 32) as u64) & (1 << (intid & 31)) != 0 }
    }
}

/// Acknowledge the highest-priority pending interrupt: its INTID, or `SPURIOUS`.
#[inline]
pub fn ack() -> u32 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, icc_iar1_el1", out(reg) v, options(nomem, nostack)); }
    v as u32
}

/// End the interrupt `ack` returned (priority drop + deactivation in one, EOImode 0).
#[inline]
pub fn eoi(intid: u32) {
    unsafe { core::arch::asm!("msr icc_eoir1_el1, {}", "isb", in(reg) intid as u64, options(nomem, nostack)); }
}

/// Send SGI `sgi` (0..15) to the core with `mpidr`.
pub fn send_sgi(mpidr: u64, sgi: u32) {
    let aff0 = mpidr & 0xF;
    let v = ((mpidr >> 32) & 0xFF) << 48        // Aff3
        | ((mpidr >> 16) & 0xFF) << 32          // Aff2
        | ((mpidr >> 8) & 0xFF) << 16           // Aff1
        | ((sgi as u64) & 0xF) << 24
        | (1u64 << aff0);                       // the target list within the Aff1 cluster
    // `dsb ishst` first: a `dmb` (what a SeqCst store or fence gives) does not order a
    // system-register write, so without it the SGI may arrive before the stores it announces
    // (a WOKEN mark, a hint) are visible to the target — a lost wakeup until the next
    // interrupt. And no `nomem`: the compiler must not move the caller's stores past it.
    // Linux's `gic_ipi_send_mask` does the same (docs/ARM64-PLAN.md §4.13).
    unsafe { core::arch::asm!("dsb ishst", "msr icc_sgi1r_el1, {}", "isb", in(reg) v, options(nostack)); }
}
