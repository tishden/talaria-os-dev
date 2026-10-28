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

/// Copy `words` u64s into write-combining device memory (arm64's twin writes whole 64-byte
/// blocks). x86's WC buffer combines the plain stores by itself: the loop as it always was.
#[inline(always)]
pub unsafe fn wc_copy64(dst: *mut u64, src: *const u64, words: usize) {
    for i in 0..words {
        // SAFETY: the caller gives `words` readable u64s at src and writable device u64s at dst.
        unsafe { core::ptr::write_volatile(dst.add(i), core::ptr::read_unaligned(src.add(i))); }
    }
}

/// x86 keeps the fence after an LLQ record (`mfence`) as it was measured: dropping it is a
/// change of the x86 datapath and a separate, measured commit (arm64's twin is `true`).
pub const WC_ORDERED_BY_DOORBELL: bool = false;
