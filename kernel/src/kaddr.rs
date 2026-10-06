//! The kernel's address kinds — the vocabulary of the direct map (docs/ARM64-PLAN.md §10.16).
//! Re-exported as `mem::*`; dependency-free so that `kernel-unit` compiles this very file.

// ── WHAT AN ADDRESS IS: the kernel's address kinds, named (docs/ARM64-PLAN.md §10.16) ──
// Today the kernel reaches RAM through the identity (virtual == physical), and an integer
// became a pointer by a bare `as` cast wherever it was needed. The direct map moves RAM's
// kernel view to slot 256, and then an integer that is a FRAME and an integer that is
// already a VIRTUAL address stop being the same thing. Every integer-to-pointer and
// pointer-to-integer cast in the kernel goes through one of these helpers, which say which
// kind it is; `implicit_provenance_casts` (both directions) is denied in main.rs, so a bare
// cast does not compile. Stage 1: they are all the identity. `talaria.test=dmapid`.
//
//   kva/kptr     a physical RAM address (a frame from this allocator, a page-table entry's
//                address, a DMA buffer's address) -> the kernel's pointer to it
//   kpa/kpa_of   the kernel's pointer to RAM -> the physical address (for a device, an entry)
//   vptr/vaddr   an integer that is ALREADY a kernel virtual address (a symbol of the image,
//                a stack top, a pointer kept as u64) <-> the pointer; no translation ever
//   uaccess::uptr  a ring-3 address in the current address space
//   devptr       a device register (MMIO), already at its virtual address (`arch::mmio_va`)
/// The kernel's virtual address of physical RAM address `pa`.
#[inline(always)] pub fn kva(pa: u64) -> u64 { pa }
/// The kernel's pointer to physical RAM address `pa`.
#[inline(always)] pub fn kptr<T>(pa: u64) -> *mut T { core::ptr::with_exposed_provenance_mut(kva(pa) as usize) }
/// The physical address of kernel virtual address `va` of RAM (the inverse of `kva`).
#[inline(always)] pub fn kpa(va: u64) -> u64 { va }
/// The physical address of what a kernel pointer to RAM points at.
#[inline(always)] pub fn kpa_of<T: ?Sized>(p: *const T) -> u64 { kpa(vaddr(p)) }
/// A pointer from an integer that already is a kernel virtual address.
#[inline(always)] pub fn vptr<T>(va: u64) -> *mut T { core::ptr::with_exposed_provenance_mut(va as usize) }
/// A pointer's virtual address as an integer (kept, compared, given back by `vptr`).
#[inline(always)] pub fn vaddr<T: ?Sized>(p: *const T) -> u64 { p.cast::<u8>().expose_provenance() as u64 }
/// A device register's pointer; `va` is its virtual address already (`arch::mmio_va`).
#[inline(always)] pub fn devptr<T>(va: u64) -> *mut T { core::ptr::with_exposed_provenance_mut(va as usize) }
