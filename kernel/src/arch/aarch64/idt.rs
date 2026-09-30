//! The interrupt numbers and the handler table under the x86 names — `arch::idt` on arm64
//! (docs/ARM64-PLAN.md §10.1). There is no descriptor table: `VBAR_EL1` points at the
//! vectors of `trap.rs`, and the IRQ vector acknowledges INTIDs from the GIC and dispatches
//! on this table. A "vector" here is an INTID: the SGIs 0..15 are the inter-core calls,
//! the PPI 27 is the tick, the SPIs (32..) are the devices.

use core::sync::atomic::{AtomicU64, Ordering};

pub static IPI_COUNT: AtomicU64 = AtomicU64::new(0);
pub static TICKS: AtomicU64 = AtomicU64::new(0);
/// The inter-core calls are SGIs: the plain IPI (a counter), the TLB shootdown (unused on
/// arm64 — `TLBI ... IS` broadcasts — but the number exists for the callers), the wake-up.
pub const IPI_VECTOR: usize = 1;
pub const TLB_VECTOR: usize = 2;
pub const WAKE_VECTOR: usize = 3;
/// "Enter the scheduler now": what the shared code asks for when it sends an IPI on the
/// TIMER's vector (a dedicated core has no tick; a thread marked dead in EL0 is taken off
/// its core by exactly this call). A PPI cannot be sent to another core, so the request
/// travels as this SGI and the IRQ vector treats it as a tick that re-arms nothing.
pub const RESCHED_SGI: u32 = 4;
/// The tick: the virtual timer's PPI (the tree says 27 on every machine seen).
pub const TIMER_VECTOR: usize = 27;
/// The network card's line and the first MSI: SPIs, chosen at 32 + a distance from what
/// `virt` wires to its virtio-mmio windows (SPIs 16..47 -> INTIDs 48..79).
pub const NIC_VECTOR: usize = 80;
pub const MSI_VECTOR_BASE: usize = 96;
/// How many INTIDs the table covers (SGIs, PPIs and the first SPIs).
const NVEC: usize = 256;

/// The handlers by INTID: the address of an `extern "C" fn()`; 0 — count and end it.
static HANDLERS: [AtomicU64; NVEC] = [const { AtomicU64::new(0) }; NVEC];
/// The scheduler's tick handler, kept across a late core's `init` (as x86's `SCHED_TIMER`).
pub static SCHED_TIMER: AtomicU64 = AtomicU64::new(0);

/// The vectors and this core's GIC side. Idempotent; every core calls it.
pub fn init() {
    super::trap::init();
    unsafe { super::gic::init_cpu(); }
}
pub fn set_vector(vec: usize, handler: u64) {
    if vec == TIMER_VECTOR { SCHED_TIMER.store(handler, Ordering::SeqCst); }
    if vec < NVEC { HANDLERS[vec].store(handler, Ordering::SeqCst); }
}
/// The installed handler of an INTID, if any (the IRQ vector asks).
pub fn handler_of(intid: u32) -> Option<extern "C" fn()> {
    let h = if (intid as usize) < NVEC { HANDLERS[intid as usize].load(Ordering::Relaxed) } else { 0 };
    if h == 0 { None } else { Some(unsafe { core::mem::transmute::<usize, extern "C" fn()>(h as usize) }) }
}
extern "C" fn nic_stub() { crate::netsys::nic_irq(); }
pub fn nic_handler() -> u64 { nic_stub as *const () as u64 }

/// The exception frame the shared code reads: the trap frame.
pub type ExcFrame = super::trap::Frame;
