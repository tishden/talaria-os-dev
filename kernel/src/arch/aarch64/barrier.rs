//! The barriers a driver puts between memory a DEVICE reads and the register write that
//! tells it to read — `arch::barrier` (docs/ARM64-PLAN.md §4.11, §4.13).
//!
//! `fence(SeqCst)` is `dmb ish` here: it orders this core's accesses for the INNER
//! shareable domain — the other cores. A PCIe device (a DMA master behind the host bridge)
//! is in the OUTER shareable domain, and only a `dmb osh*` orders the stores to the ring
//! against the doorbell's MMIO write as the device will observe them.

/// Stores to DMA memory are complete, as a device sees them, before what follows (the
/// doorbell): `dmb oshst`.
#[inline(always)]
pub fn dma_wmb() {
    // SAFETY: a barrier instruction; no memory, no stack, no flags.
    unsafe { core::arch::asm!("dmb oshst", options(nomem, nostack, preserves_flags)); }
}

/// What the device wrote BEFORE the completion just read is what the loads after this see
/// (the payload of a read behind its completion entry): `dmb oshld`. Without it the core
/// may have loaded the payload's bytes before it loaded the entry that says they are there.
#[inline(always)]
pub fn dma_rmb() {
    // SAFETY: a barrier instruction; no memory, no stack, no flags.
    unsafe { core::arch::asm!("dmb oshld", options(nomem, nostack, preserves_flags)); }
}
