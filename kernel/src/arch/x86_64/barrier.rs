//! The barriers a driver puts between memory a DEVICE reads and the register write that
//! tells it to read — `arch::barrier`. x86 is TSO and a doorbell is an uncached MMIO write,
//! which the store buffer keeps in order behind the ring's stores; the full fence the
//! drivers have always had here (`mfence`) is kept as it is — a weaker one would be a
//! change of the measured x86 datapath, and that is a separate, measured commit
//! (docs/ARM64-PLAN.md §4.13). arm64's twin is `dmb oshst`.

/// Stores to DMA memory are complete, as a device sees them, before what follows (the doorbell).
#[inline(always)]
pub fn dma_wmb() { core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst); }

/// What the device wrote before the completion just read is what the loads after this
/// see. x86 does not reorder a load before an earlier load: nothing for the machine, and
/// the compiler is told not to move the loads either.
#[inline(always)]
pub fn dma_rmb() { core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst); }
