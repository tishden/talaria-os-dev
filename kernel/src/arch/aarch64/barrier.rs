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
    unsafe { core::arch::asm!("dmb oshst", options(nostack, preserves_flags)); }
}

/// What the device wrote BEFORE the completion just read is what the loads after this see
/// (the payload of a read behind its completion entry): `dmb oshld`. Without it the core
/// may have loaded the payload's bytes before it loaded the entry that says they are there.
#[inline(always)]
pub fn dma_rmb() {
    // SAFETY: a barrier instruction; no memory, no stack, no flags.
    unsafe { core::arch::asm!("dmb oshld", options(nostack, preserves_flags)); }
}

/// Copy `words` u64s (a multiple of 8: whole 64-byte blocks, both ends 64-byte aligned) into
/// write-combining device memory (Normal-NC) as whole 64-byte blocks: four `ldp` then four
/// `stp` back to back, so the core's gathering buffer fills completely and leaves as one
/// 64-byte write rather than many partial ones; a `dgh` at the end lets the last block go.
/// The same shape as Linux's `__iowrite64_copy` on arm64 (`__const_memcpy_toio_aligned64`,
/// written for exactly this: the ENA LLQ and mlx5's BlueFlame on Graviton). An LLQ record
/// written as 32 separate `str`s cost about 1 us a frame (ARM64-PLAN §12.9).
#[inline(always)]
pub unsafe fn wc_copy64(dst: *mut u64, src: *const u64, words: usize) {
    let mut i = 0;
    while i + 8 <= words {
        // SAFETY: the caller gives 64 readable bytes at src+i and 64 writable device bytes at
        // dst+i; the registers are named so the compiler keeps the eight stores contiguous.
        unsafe {
            core::arch::asm!(
                "ldp {a}, {b}, [{s}]", "ldp {c}, {d}, [{s}, #16]",
                "ldp {e}, {f}, [{s}, #32]", "ldp {g}, {h}, [{s}, #48]",
                "stp {a}, {b}, [{t}]", "stp {c}, {d}, [{t}, #16]",
                "stp {e}, {f}, [{t}, #32]", "stp {g}, {h}, [{t}, #48]",
                s = in(reg) src.add(i), t = in(reg) dst.add(i),
                a = out(reg) _, b = out(reg) _, c = out(reg) _, d = out(reg) _,
                e = out(reg) _, f = out(reg) _, g = out(reg) _, h = out(reg) _,
                options(nostack, preserves_flags));
        }
        i += 8;
    }
    // SAFETY: a hint instruction (DGH): no memory, no stack, no flags.
    unsafe { core::arch::asm!("hint #6", options(nostack, preserves_flags)); }
}

/// A store into write-combining device memory needs no fence of its own before the doorbell:
/// every doorbell write is preceded by `dma_wmb` (`dmb oshst`), which orders the WC stores
/// before it as the device sees them — Linux's `__iowrite64_copy` then `writel`. A `dmb ish`
/// right after the record waits for it to drain (~330 ns a frame on a Graviton).
pub const WC_ORDERED_BY_DOORBELL: bool = true;
