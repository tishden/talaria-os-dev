//! The kernel's address kinds — the vocabulary of the direct map (docs/ARM64-PLAN.md §10.16).
//! Re-exported as `mem::*`; dependency-free so that `kernel-unit` compiles this very file.

// ── WHAT AN ADDRESS IS: the kernel's address kinds, named (docs/ARM64-PLAN.md §10.16) ──
// Today the kernel reaches RAM through the identity (virtual == physical), and an integer
// became a pointer by a bare `as` cast wherever it was needed. The direct map moves RAM's
// kernel view to slot 256, and then an integer that is a FRAME and an integer that is
// already a VIRTUAL address stop being the same thing. Every integer-to-pointer and
// pointer-to-integer cast in the kernel goes through one of these helpers, which say which
// kind it is; `implicit_provenance_casts` (both directions) is denied in main.rs, so a bare
// cast does not compile. `talaria.test=dmapid` checks the round trips.
//
// Stage 2: `kva` of RAM the direct map covers is the map's address (`DMAP_BASE + pa`), every
// other physical address keeps its identity (a firmware table outside RAM, anything before the
// map is built, `talaria.dmap=0`) and is counted (`DMAP_MISS`) — the meter of what stage 4,
// which drops RAM's identity from process spaces, still has to give a kind of its own.
//
//   kva/kptr     a physical RAM address (a frame from this allocator, a page-table entry's
//                address, a DMA buffer's address) -> the kernel's pointer to it
//   kpa/kpa_of   the kernel's pointer to RAM -> the physical address (for a device, an entry)
//   vptr/vaddr   an integer that is ALREADY a kernel virtual address (a symbol of the image,
//                a stack top, a pointer kept as u64) <-> the pointer; no translation ever
//   uaccess::uptr  a ring-3 address in the current address space
//   devptr       a device register (MMIO), already at its virtual address (`arch::mmio_va`)
use core::sync::atomic::{AtomicU64, Ordering};

/// Where the direct map starts: top-level slot 256 on both machines — x86's first kernel-half
/// slot, arm64's first TTBR0 slot above `USER_TOP` (`paging::user_range_ok` refuses it to ring 3).
#[cfg(target_arch = "x86_64")]
pub const DMAP_BASE: u64 = 0xFFFF_8000_0000_0000;
#[cfg(not(target_arch = "x86_64"))]
pub const DMAP_BASE: u64 = 0x0000_8000_0000_0000;
/// One top-level slot: the physical addresses 0..512 GiB.
pub const DMAP_SPAN: u64 = 1 << 39;
/// Which 2 MiB pieces of physical memory the map covers (`paging::dmap_init` sets them, never
/// clears them): a bit per piece, 32 KiB for the slot. Empty -> `kva` is the identity.
static DMAP_2M: [AtomicU64; (DMAP_SPAN >> 21) as usize / 64] = [const { AtomicU64::new(0) }; (DMAP_SPAN >> 21) as usize / 64];
/// How many `kva` calls found their address outside the map (the identity was used).
pub static DMAP_MISS: AtomicU64 = AtomicU64::new(0);
/// The last such address (the stage-4 meter names one).
pub static DMAP_MISS_LAST: AtomicU64 = AtomicU64::new(0);
/// Mark [pa, pa + len) as covered; both 2 MiB-aligned. Only `paging::dmap_init`, after the
/// leaves are written and visible.
pub fn dmap_cover(pa: u64, len: u64) {
    let mut i = pa >> 21;
    while i < (pa + len) >> 21 && i < DMAP_SPAN >> 21 {
        DMAP_2M[(i / 64) as usize].fetch_or(1 << (i % 64), Ordering::Release);
        i += 1;
    }
}
/// Is physical address `pa` reachable through the direct map.
#[inline(always)]
pub fn dmap_covers(pa: u64) -> bool {
    let i = pa >> 21;
    i < DMAP_SPAN >> 21 && DMAP_2M[(i / 64) as usize].load(Ordering::Relaxed) & (1 << (i % 64)) != 0
}
/// The kernel's virtual address of physical RAM address `pa`.
#[inline(always)]
pub fn kva(pa: u64) -> u64 {
    if dmap_covers(pa) { return pa + DMAP_BASE; }
    kva_miss(pa)
}
#[cold]
#[inline(never)]
fn kva_miss(pa: u64) -> u64 {
    if DMAP_ON.load(Ordering::Relaxed) {
        DMAP_MISS.fetch_add(1, Ordering::Relaxed);
        DMAP_MISS_LAST.store(pa, Ordering::Relaxed);
    }
    pa
}
/// The map exists (set by `paging::dmap_init`): a miss before it is not a miss.
pub static DMAP_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// The kernel's pointer to physical RAM address `pa`.
#[inline(always)] pub fn kptr<T>(pa: u64) -> *mut T { core::ptr::with_exposed_provenance_mut(kva(pa) as usize) }
/// The physical address of kernel virtual address `va` of RAM (the inverse of `kva`): the map's
/// range is translated, anything else is the identity (the image, a kernel stack, a miss).
#[inline(always)] pub fn kpa(va: u64) -> u64 { if va.wrapping_sub(DMAP_BASE) < DMAP_SPAN { va - DMAP_BASE } else { va } }
/// The physical address of what a kernel pointer to RAM points at.
#[inline(always)] pub fn kpa_of<T: ?Sized>(p: *const T) -> u64 { kpa(vaddr(p)) }
/// A pointer from an integer that already is a kernel virtual address.
#[inline(always)] pub fn vptr<T>(va: u64) -> *mut T { core::ptr::with_exposed_provenance_mut(va as usize) }
/// A pointer's virtual address as an integer (kept, compared, given back by `vptr`).
#[inline(always)] pub fn vaddr<T: ?Sized>(p: *const T) -> u64 { p.cast::<u8>().expose_provenance() as u64 }
/// A device register's pointer; `va` is its virtual address already (`arch::mmio_va`).
#[inline(always)] pub fn devptr<T>(va: u64) -> *mut T { core::ptr::with_exposed_provenance_mut(va as usize) }
