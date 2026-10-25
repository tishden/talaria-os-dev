//! Our own page tables: the kernel builds a PML4+PDPT with 1G identity pages from
//! its own frame allocator and switches CR3. Fewer TLB entries (4 entries
//! cover 4 GB). It replaces boot.rs's static 2M tables.
//!
//! # Safety
//!
//! This module IS the memory-safety boundary: everything it writes is a page
//! table entry, so a mistake here does not corrupt one object, it changes what
//! every address in the machine means. The `unsafe` in it rests on five
//! invariants, named P1..P5 at the sites instead of being repeated there.
//!
//! - **P1. The identity map.** In the kernel's own address space the physical
//!   memory is mapped one to one (GB0 by 2M leaves, GB1..3 by 1G ones), so a
//!   frame's physical address IS a valid pointer to its 4096 bytes. Every
//!   `phys as *mut Pte` below is that, and nothing else — the frames come from
//!   `mem::alloc_frame`/`alloc_contig`, which never hand out the same frame twice
//!   and never hand out one below `mem::alloc_base()` (the invariant `init`
//!   prints: the allocator must start above the kernel's last supervisor leaf).
//! - **P2. A table pointer is derived from a live table.** The walks (`pte_ptr`,
//!   `pde_ptr_in`, `pt_for`, `walk_*`) start at CR3 or at an explicit `pml4_phys`,
//!   mask every entry with `ADDR_MASK` and check `PTE_P` before descending. So a
//!   returned pointer always points into a frame that belongs to that table at
//!   that level — never into data, never into a frame that has been freed, because
//!   a table is freed only with the address space that owns it.
//! - **P3. A parent entry changes only by CAS.** A process's tables are shared by
//!   all its threads, and a split runs on the fault path, that is, on several cores
//!   at once. `install_table` is a compare-exchange from the observed value, and
//!   the loser of the race frees its own table and uses the winner's. The long
//!   comment above it records what a plain store cost: a SIGSEGV at a lawful
//!   address, once in a hundred rounds.
//! - **P4. An entry changed is an entry to be flushed.** A write to a table is not
//!   complete until this core's TLB is invalidated (`flush_range`/`flush_tlb`) and
//!   the other cores are shot down (`shootdown`). Skipping it is not UB in the
//!   Rust sense — it is a stale translation, which is worse to debug.
//! - **P5. The `asm!` touches THIS core's machine state.** CR0.WP, CR3, EFER.NXE,
//!   `invlpg`: the `options(...)` on each block say exactly what the compiler may
//!   assume, and a CR3 write is never reordered across the table updates it
//!   publishes because the block is not `nomem`.
//!
//! What this module deliberately does NOT protect: the modes that run the
//! workload in the kernel's own identity space (`talaria.cell=identity`, the
//! embedded demo modes). There ring 3 reaches the identity window by
//! construction; a process's address space (`new_process_space`) has no such
//! window — the identity leaves in it are supervisor, and a region is handed to
//! ring 3 only explicitly, by `protect`.

use crate::arch::mmu;
use crate::mem::{kpa_of, kptr, kva};

/// Build our own tables (1G identity, GB3 uncached for MMIO) and load CR3.
pub fn init() {
    // SAFETY: P5 — THIS core's one-time enables (x86: EFER.NXE before any NX bit reaches a
    // table, CR0.WP so that a kernel write into an RO COW page faults instead of silently
    // changing a frame shared with a child, SMEP so that ring 0 never fetches from a user
    // page), before the tables are published. SMEP is printed either way: a protection one
    // believes in without seeing it in the log is the kind that turns out to have been off.
    let smep = unsafe { mmu::init_hw() };
    crate::println!("talaria: paging: SMEP {} (ring 0 does not execute the workload's pages), SMAP is not used — the kernel copies user memory with plain \
        loads (uaccess.rs)",
                    if smep { "is ON" } else { "is NOT SUPPORTED by this CPU" });
    if !mmu::has_1g() {
        crate::println!("talaria: paging: 1G pages are not supported, I leave the boot tables");
        return;
    }
    let pml4 = crate::mem::alloc_frame().expect("frame pml4");
    let pdpt = crate::mem::alloc_frame().expect("frame pdpt");
    let pd0 = crate::mem::alloc_frame().expect("frame pd0"); // the kernel's GB -> 2M pages (per-page protection)
    // The 2M leaves occupied by THE KERNEL'S IMAGE are supervisor; leaf 0 (0..2 MB) too (the AP's
    // trampoline and the BIOS structures are there). Everything else in the kernel's gigabyte is
    // user (U/S). Previously the boundary was nailed to 32 MB; now it is computed from the image,
    // because the kernel's base has become a link-time parameter (see linker.ld, KBASE) — and
    // on arm64 that gigabyte is not the first one at all (RAM starts at 1 GiB on QEMU `virt`).
    let sup_lo = crate::mem::kernel_lo() >> 21;
    let sup_hi = (crate::mem::kernel_hi() + 0x1F_FFFF) >> 21;
    let kgb = ident_gb();
    // SAFETY: P1 + P4 + P5 — the three frames have just come from the allocator (identity-mapped,
    // nobody else holds them), they are filled COMPLETELY before the CR3 write, and the
    // CR3 write is what publishes them.
    unsafe {
        core::ptr::write_bytes(kptr::<u8>(pml4), 0, 4096);
        core::ptr::write_bytes(kptr::<u8>(pdpt), 0, 4096);
        core::ptr::write_bytes(kptr::<u8>(pd0), 0, 4096);
        pte_w(kptr::<Pte>(pml4), pdpt | PTE_P | PTE_RW | PTE_US);
        // What each of the four identity gigabytes is comes from the architecture: on x86 GB0 holds
        // the kernel and the user region, GB1-2 are RAM, GB3 is the MMIO hole; on arm64 `virt`
        // GB0 is the devices and the kernel's gigabyte is GB1.
        for gb in 0..4u64 {
            match mmu::gb_kind(gb as usize, kgb) {
                // the kernel's gigabyte: a PD with 512 2M pages; the kernel is supervisor, the rest is U/S
                mmu::GbKind::User2M => {
                    for j in 0..512u64 {
                        let abs = gb * 512 + j; // the 2M index in the machine's address space
                        let mut e = (abs << 21) | PTE_P | PTE_RW | PTE_PS;
                        if abs != 0 && !(abs >= sup_lo && abs < sup_hi) { e |= PTE_US; } // U/S outside the kernel's image
                        pte_w(kptr::<Pte>(pd0).add(j as usize), e);
                    }
                    pte_w(kptr::<Pte>(pdpt).add(gb as usize), pd0 | PTE_P | PTE_RW | PTE_US);
                }
                mmu::GbKind::Ram1G => pte_w(kptr::<Pte>(pdpt).add(gb as usize), (gb << 30) | PTE_P | PTE_RW | PTE_PS),
                // arm64's GB0 of devices is NOT mapped by its identity: the kernel reaches it through
                // the devices' alias (below), and the gigabyte is free for a workload whose image is
                // linked there (ClickHouse's arm64 ET_EXEC: 2 MB..458 MB) — every process copies this L1
                mmu::GbKind::Device if gb == 0 && cfg!(target_arch = "aarch64") => {}
                mmu::GbKind::Device => pte_w(kptr::<Pte>(pdpt).add(gb as usize), (gb << 30) | PTE_P | PTE_RW | PTE_PS | PTE_UC),
            }
        }
        // arm64: the devices' alias (`mmu::DEV_SLOT`) — GB0's Device block once more in a slot
        // above every user range, supervisor, BEFORE the first address space copies the root.
        #[cfg(target_arch = "aarch64")]
        {
            let dl1 = crate::mem::alloc_frame().expect("frame: the devices' alias");
            core::ptr::write_bytes(kptr::<u8>(dl1), 0, 4096);
            pte_w(kptr::<Pte>(dl1), PTE_P | PTE_RW | PTE_PS | PTE_UC);
            pte_w(kptr::<Pte>(pml4).add(mmu::DEV_SLOT), dl1 | PTE_P | PTE_RW);
        }
        // STAGE 7.1: the image's high alias — its 2M leaves once more at top-level slot 511
        // (x86 −2 GiB, arm64 TTBR0 L0[511]/L1[510]), supervisor, BEFORE the first address space
        // copies the root (slots 1..511) and before the root switch, so that an image linked
        // there (7.2/7.3) never runs on a root without it.
        let img_gb = crate::mem::kernel_lo() & !((1u64 << 30) - 1);
        // One PD: the image must lie in one gigabyte (2 MB..~60 MB today; KBASE moves it).
        assert!(sup_hi <= (img_gb >> 21) + 512, "the kernel's image crosses a gigabyte");
        let hpdpt = crate::mem::alloc_frame().expect("frame: the image's alias (pdpt)");
        let hpd = crate::mem::alloc_frame().expect("frame: the image's alias (pd)");
        core::ptr::write_bytes(kptr::<u8>(hpdpt), 0, 4096);
        core::ptr::write_bytes(kptr::<u8>(hpd), 0, 4096);
        // W^X (stage 7.4): the linker scripts start `.rodata` and `.data` on 2 MiB, so each leaf
        // is one section's — the code read-only and executable, the constants read-only, the data
        // writable; never both writable and executable. (A gdb breakpoint writes the code through
        // the direct map.)
        let (text_end, ro_end) = (crate::mem::kernel_text_end(), crate::mem::kernel_rodata_end());
        let alias_off = crate::mem::KIMG_VBASE - img_gb;
        for abs in sup_lo..sup_hi {
            let j = (abs - (img_gb >> 21)) as usize;
            let va = (abs << 21) + alias_off;
            let rights = if crate::mem::KIMG_OFF == 0 { PTE_RW }       // the image runs low: the alias is a copy
                else if va < text_end { 0 }
                else if va < ro_end { PTE_NX }
                else { PTE_RW | PTE_NX };
            pte_w(kptr::<Pte>(hpd).add(j), (abs << 21) | PTE_P | PTE_PS | rights);
        }
        let s4 = ((crate::mem::KIMG_VBASE >> 39) & 0x1FF) as usize;
        let s3 = ((crate::mem::KIMG_VBASE >> 30) & 0x1FF) as usize;
        pte_w(kptr::<Pte>(hpdpt).add(s3), hpd | PTE_P | PTE_RW);
        pte_w(kptr::<Pte>(pml4).add(s4), hpdpt | PTE_P | PTE_RW);
        KIMG_ALIAS_OFF.store(crate::mem::KIMG_VBASE - img_gb, core::sync::atomic::Ordering::Relaxed);
        mmu::set_root(pml4);
    }
    #[cfg(target_arch = "x86_64")]
    mmu::pat_init(); // the PAT4 slot = write-combining (the ENA's LLQ window, §A1); arm64 types memory by MAIR index
    KERNEL_PML4.store(pml4, core::sync::atomic::Ordering::Relaxed); // the reference kernel AS for fork/new_address_space
    crate::println!("talaria: our own page tables are active (GB{} on 2M: the kernel {:#x}..{:#x} is supervisor, the rest is user; the other GBs on 1G; \
        the image's high alias at {:#x})",
        kgb, crate::mem::kernel_lo(), crate::mem::kernel_hi(), kimg_alias(crate::mem::kernel_va_lo()).unwrap_or(0));
    // AN INVARIANT THAT BREAKS BY ITSELF. The protection in GB0 is set by 2 MB leaves,
    // so the leaf with the kernel's tail is supervisor ENTIRELY. The allocator must start
    // above it — otherwise the frames from that leaf will go into ring 3 and the very first write
    // there will give a #PF "protection, write". The check costs one comparison per boot
    // and is printed always: this refusal has already happened, it depended on THE IMAGE'S SIZE and
    // looked like "some code was added — printf broke".
    let base = crate::mem::alloc_base();
    let sup_end = sup_hi << 21;
    if base >= sup_end {
        crate::println!("paging: the frame allocator starts at {:#x} — above the supervisor leaf {:#x}: OK", base, sup_end);
    } else {
        crate::println!("paging: AN ALARM: the frame allocator starts at {:#x}, while the kernel's supervisor leaf ends at {:#x}",
            base, sup_end);
        crate::println!("paging: AN ALARM: {} KB of frames will be given to ring 3 on supervisor pages — the workload will fall on a #PF",
            (sup_end - base) / 1024);
    }
}

// ── THE DIRECT MAP (docs/ARM64-PLAN.md §10.16, stage 2) ─────────────────────────
// RAM once more, at `mem::DMAP_BASE + pa`, in top-level slot 256 of the reference table, so that
// every address space built after it (`copy_kernel_pml4`: slots 1..511) carries it. Only the
// firmware map's RAM (`mem::ram_region`), rounded INWARD to 2 MiB: a device or a hole must never
// get a cacheable alias (on arm64 that is an architectural aliasing fault). A gigabyte that is
// RAM from end to end is one 1G leaf, any other covered piece a 2M leaf; supervisor, never
// executable, already dirty (arm64's DBM would otherwise make the first write fault). Nothing
// here is ever split or removed, so no break-before-make on memory other cores use.
// `mem::kva` translates through the map only what `mem::dmap_cover` marked after the leaves
// are visible; `talaria.dmap=0` leaves the identity for everything (the A/B and the fallback).

/// Build the direct map; true when `kva` now goes through it.
pub fn dmap_init(enabled: bool) -> bool {
    use core::sync::atomic::Ordering;
    let k = KERNEL_PML4.load(Ordering::Relaxed);
    if !enabled || k == 0 {
        crate::println!("talaria: the direct map is OFF ({}) — the kernel reaches RAM by its identity",
            if !enabled { "talaria.dmap=0" } else { "no own tables" });
        return false;
    }
    let slot = ((crate::mem::DMAP_BASE >> 39) & 0x1FF) as usize;
    let (mut g1, mut m2, mut bytes) = (0u64, 0u64, 0u64);
    // SAFETY: P1 + P2 + P3 — the slot is empty and no address space exists yet (checked below),
    // so nothing walks these tables while they are filled; every table frame is fresh and
    // zeroed before its parent entry points at it.
    unsafe {
        let pml4 = kptr::<Pte>(k);
        if AS_LIVE.load(Ordering::Relaxed) > 0 {
            crate::println!("paging: AN ALARM: the direct map is built after {} address spaces — they do not have it", AS_LIVE.load(Ordering::Relaxed));
        }
        let fresh = || -> Option<u64> {
            let f = crate::mem::alloc_frame()?;
            core::ptr::write_bytes(kptr::<u8>(f), 0, 4096);
            Some(f)
        };
        if pte_r(pml4.add(slot)) & PTE_P == 0 {
            let Some(t) = fresh() else { crate::println!("talaria: the direct map: no frame for its table — OFF"); return false; };
            pte_w(pml4.add(slot), t | PTE_P | PTE_RW);
        }
        let pdpt = kptr::<Pte>(pte_r(pml4.add(slot)) & ADDR_MASK);
        const LEAF: u64 = PTE_P | PTE_RW | PTE_PS | PTE_NX | PTE_D;
        for i in 0..crate::mem::ram_regions() {
            let (b, l) = crate::mem::ram_region(i);
            let lo = (b + 0x1F_FFFF) & !0x1F_FFFF;
            let hi = ((b + l) & !0x1F_FFFF).min(crate::mem::DMAP_SPAN);
            let mut a = lo;
            while a < hi {
                let gi = (a >> 30) as usize;
                let e3 = pdpt.add(gi);
                if a & 0x3FFF_FFFF == 0 && a + (1 << 30) <= hi && pte_r(e3) & PTE_P == 0 {
                    pte_w(e3, a | LEAF);
                    g1 += 1; a += 1 << 30; bytes += 1 << 30;
                    continue;
                }
                if pte_r(e3) & PTE_P == 0 {
                    let Some(t) = fresh() else { break; };
                    pte_w(e3, t | PTE_P | PTE_RW);
                }
                let ev = pte_r(e3);
                if ev & PTE_PS == 0 {
                    let e2 = kptr::<Pte>(ev & ADDR_MASK).add(((a >> 21) & 0x1FF) as usize);
                    if pte_r(e2) & PTE_P == 0 { pte_w(e2, a | LEAF); m2 += 1; bytes += 1 << 21; }
                }
                a += 1 << 21;
            }
        }
        flush_tlb();
        for i in 0..crate::mem::ram_regions() {
            let (b, l) = crate::mem::ram_region(i);
            let lo = (b + 0x1F_FFFF) & !0x1F_FFFF;
            let hi = ((b + l) & !0x1F_FFFF).min(crate::mem::DMAP_SPAN);
            if hi > lo { crate::mem::dmap_cover(lo, hi - lo); }
        }
    }
    crate::mem::DMAP_ON.store(true, Ordering::SeqCst);
    crate::println!("talaria: the direct map is ON at {:#x}: {} MB of RAM ({} 1G + {} 2M leaves, slot {})",
        crate::mem::DMAP_BASE, bytes >> 20, g1, m2, slot);
    true
}

// ── THE KERNEL STACKS' WINDOW (docs/ARM64-PLAN.md §10.16, stage 3) ───────────────────
// Top-level slot 257 (`mem::KWIN_BASE`): the kernel stacks at addresses of their own, each one's
// pages mapped one by one (4K leaves, supervisor, NX) from any frames, the page under each
// never mapped. The slot's PDPT exists before the first address space, so every space shares
// every table under it: a guard is a guard everywhere at once, which retires the per-space
// guard dance of the identity stacks (item 25: a fork's child ran with its guard in the
// parent's space only), and a stack no longer needs 64 KB of CONTIGUOUS memory (item 49: the
// panic on `alloc_kstack().unwrap()` was a contiguous request). A mapped page is never
// unmapped — a freed stack keeps its frames and its slot goes back to `sched`'s pool — so no
// shootdown is ever needed for the window, and no leaf of it is ever changed while in use.

/// Build the window's top-level table; true when `sched` may give stacks from it.
pub fn kwin_init(enabled: bool) -> bool {
    use core::sync::atomic::Ordering;
    let k = KERNEL_PML4.load(Ordering::Relaxed);
    if !enabled || k == 0 {
        crate::println!("talaria: the kernel stacks' window is OFF ({}) — stacks are identity frames, guarded per space",
            if !enabled { "talaria.kwin=0" } else { "no own tables" });
        return false;
    }
    let slot = ((crate::mem::KWIN_BASE >> 39) & 0x1FF) as usize;
    // SAFETY: P1 + P2 — the slot is empty and no address space exists yet; the frame is fresh
    // and zeroed before the entry points at it.
    unsafe {
        let pml4 = kptr::<Pte>(k);
        if pte_r(pml4.add(slot)) & PTE_P == 0 {
            let Some(t) = crate::mem::alloc_frame() else { return false; };
            core::ptr::write_bytes(kptr::<u8>(t), 0, 4096);
            pte_w(pml4.add(slot), t | PTE_P | PTE_RW);
        }
    }
    KWIN_ON.store(true, Ordering::SeqCst);
    crate::println!("talaria: the kernel stacks' window is ON at {:#x} (slot {})", crate::mem::KWIN_BASE, slot);
    true
}
pub static KWIN_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

// ── THE DEVICES' WINDOW (docs/ARM64-PLAN.md §10.16, stage 5) ──────────────────────────
// Top-level slot 384 (`mem::MWIN_BASE`): the device registers at addresses of their own, one
// physical gigabyte per window gigabyte, given out the first time `mmio_va` meets an address
// of that gigabyte (`mem::mwin_*` keeps which is which). arm64's GB0 alias (`mmu::DEV_SLOT`,
// window gigabyte 0, claimed before the MMU is on) was the first piece of it. A gigabyte with
// no RAM in it is one 1G leaf; a gigabyte that also holds RAM gets a directory whose 2M leaves
// cover only the pieces with no RAM in them (a device mapping must never alias RAM: on arm64
// that is a mismatched-attributes fault, on x86 a UC alias of WB lines). Uncached (x86 PCD|PWT,
// arm64 Device-nGnRE), supervisor, never executable. The slot's table exists before the first
// address space, so every space shares every leaf in it, and a leaf, once written, is never
// removed — no shootdown, no break-before-make (a WC split, `map_mmio_wc`, is the one change,
// as it was on the identity). `talaria.mwin=0`: every device by its identity (arm64's GB0
// alias stays — the gigabyte is ring 3's there).
pub static MWIN_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static MWIN_LOCK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Build the window's top-level table; true when `mmio_va` gives window addresses.
pub fn mwin_init(enabled: bool) -> bool {
    use core::sync::atomic::Ordering;
    let k = KERNEL_PML4.load(Ordering::Relaxed);
    if !enabled || k == 0 {
        crate::println!("talaria: the devices' window is OFF ({}) — the kernel reaches device registers by their identity",
            if !enabled { "talaria.mwin=0" } else { "no own tables" });
        return false;
    }
    let slot = ((crate::mem::MWIN_BASE >> 39) & 0x1FF) as usize;
    // SAFETY: P1 + P2 — the slot is empty (x86) or holds the GB0 alias `init` built (arm64), and
    // no address space exists yet; the frame is fresh and zeroed before the entry points at it.
    unsafe {
        let pml4 = kptr::<Pte>(k);
        if AS_LIVE.load(Ordering::Relaxed) > 0 {
            crate::println!("paging: AN ALARM: the devices' window is built after {} address spaces — they do not have it", AS_LIVE.load(Ordering::Relaxed));
        }
        if pte_r(pml4.add(slot)) & PTE_P == 0 {
            let Some(t) = crate::mem::alloc_frame() else { return false; };
            core::ptr::write_bytes(kptr::<u8>(t), 0, 4096);
            pte_w(pml4.add(slot), t | PTE_P | PTE_RW);
        }
    }
    MWIN_ON.store(true, Ordering::SeqCst);
    crate::println!("talaria: the devices' window is ON at {:#x} (slot {})", crate::mem::MWIN_BASE, slot);
    true
}
/// Does RAM (the firmware map's usable ranges, exactly) touch [lo, hi).
fn ram_touches(lo: u64, hi: u64) -> bool {
    (0..crate::mem::ram_regions()).any(|i| { let (b, l) = crate::mem::ram_region(i); l != 0 && b < hi && b + l > lo })
}
/// The window's PDPT entry for window gigabyte `i` (decoded).
/// # Safety
/// P1 + P2 — a read of the shared window table; the window exists (`MWIN_ON`).
unsafe fn mwin_e3(i: u64) -> *mut Pte {
    let pml4 = kptr::<Pte>(KERNEL_PML4.load(core::sync::atomic::Ordering::Relaxed));
    let e4 = pte_r(pml4.add(((crate::mem::MWIN_BASE >> 39) & 0x1FF) as usize));
    kptr::<Pte>(e4 & ADDR_MASK).add(i as usize)
}
/// Is the window's leaf for `pa` (in window gigabyte `i`) there.
fn mwin_present(i: u64, pa: u64) -> bool {
    // SAFETY: P1 + P2 — read-only walks of the shared window table.
    unsafe {
        let e3 = pte_r(mwin_e3(i));
        if e3 & PTE_P == 0 { return false; }
        if e3 & PTE_PS != 0 { return true; }
        pte_r(kptr::<Pte>(e3 & ADDR_MASK).add(((pa >> 21) & 0x1FF) as usize)) & PTE_P != 0
    }
}
/// The kernel's pointer to the device register at physical `pa`: its window address once the
/// window exists (the gigabyte is mapped the first time it is met), the identity before that,
/// with the window OFF, or for a piece that holds RAM (counted: `mem::MWIN_MISS`).
pub fn mmio_va(pa: u64) -> u64 {
    use core::sync::atomic::Ordering;
    let on = MWIN_ON.load(Ordering::Acquire);
    if let Some(i) = crate::mem::mwin_find(pa >> 30) {
        // before the window's own table, only arm64's GB0 is given out (the early map has it)
        if !on || mwin_present(i, pa) { return crate::mem::MWIN_BASE + (i << 30) + (pa & 0x3FFF_FFFF); }
    }
    if !on { return pa; }
    match mwin_map(pa) {
        Some(va) => va,
        None => {
            crate::mem::MWIN_MISS.fetch_add(1, Ordering::Relaxed);
            crate::mem::MWIN_MISS_LAST.store(pa, Ordering::Relaxed);
            pa
        }
    }
}
/// The physical address behind a pointer `mmio_va` gave (what a device is told).
#[inline]
pub fn mmio_pa(va: u64) -> u64 { crate::mem::mwin_pa(va) }
/// Give `pa`'s gigabyte a window gigabyte (or fill in its 2M piece); None when the piece holds
/// RAM or the window is full.
fn mwin_map(pa: u64) -> Option<u64> {
    use core::sync::atomic::Ordering;
    let (pgb, glo) = (pa >> 30, (pa >> 30) << 30);
    let plo = pa & !0x1F_FFFF;
    if ram_touches(plo, plo + 0x20_0000) { return None; }
    while MWIN_LOCK.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() { core::hint::spin_loop(); }
    const DEV: u64 = PTE_P | PTE_RW | PTE_PS | PTE_UC | PTE_NX;
    let (mut newgb, mut pieces) = (false, 512);
    // SAFETY: P1 + P2 + P3 — under the window's lock: an entry of the shared window table goes
    // from empty to a leaf (or a fresh, filled directory) once; nothing maps it before it is
    // published (`tables_published`) and the gigabyte is recorded (`mwin_publish`).
    let r = unsafe {
        let (i, fresh) = match crate::mem::mwin_find(pgb) { Some(i) => (i, false), None => match crate::mem::mwin_claim() { Some(i) => (i, true), None => (u64::MAX, false) } };
        if i == u64::MAX { None } else {
            let e3 = mwin_e3(i);
            if fresh {
                if !ram_touches(glo, glo + (1 << 30)) {
                    pte_w(e3, glo | DEV);
                } else if let Some(t) = crate::mem::alloc_any_frame() {
                    core::ptr::write_bytes(kptr::<u8>(t), 0, 4096);
                    let mut n = 0;
                    for m in 0..512u64 {
                        let a = glo + (m << 21);
                        if !ram_touches(a, a + 0x20_0000) { pte_w(kptr::<Pte>(t).add(m as usize), a | DEV); n += 1; }
                    }
                    mmu::tables_published();
                    pte_w(e3, t | PTE_P | PTE_RW);
                    pieces = n;
                }
                mmu::tables_published();
                if pte_r(e3) & PTE_P != 0 { crate::mem::mwin_publish(i, pgb); newgb = true; }
            }
            if mwin_present(i, pa) { Some(crate::mem::MWIN_BASE + (i << 30) + (pa & 0x3FFF_FFFF)) } else { None }
        }
    };
    MWIN_LOCK.store(false, Ordering::Release);
    if let (Some(va), true) = (r, newgb) {
        crate::println!("paging: the devices' window: physical gigabyte {:#x} at {:#x}{}", glo, va & !0x3FFF_FFFF,
            if pieces < 512 { " (it holds RAM too: only the 2M pieces without RAM)" } else { "" });
    }
    r
}

// ── STRICT: RAM'S IDENTITY OUT OF THE PROCESS SPACES (docs/ARM64-PLAN.md §10.16, stage 4b) ─────
// `talaria.dmap=strict`: a process space (`new_process_space`) keeps the identity leaves only
// where the kernel still needs them — its own image and what lies below it in its gigabyte, and
// every 2 MiB the direct map does NOT cover (the devices, the firmware's reserved tables, a hole:
// `kva` gives those their identity, so it has to stay reachable). Every identity leaf over RAM
// the map covers is gone: ring 3 gets those addresses for its own mappings, and a kernel access
// to RAM that missed its `kva` faults in a process space instead of working by the luck of the
// identity (the crash dump names it). The kernel's own space keeps the identity (the identity
// modes run ring 3 there). It needs the map and the stacks' window: an identity kernel stack
// would be RAM by its identity.
static STRICT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// The kernel's image's end rounded up to 2 MiB: below it (in the kernel's gigabyte) the identity stays.
static STRICT_KEEP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Turn strict on — the DEFAULT since 2026-10-07 (both regressions, `validate.sh`, the arm64 KVM
/// stand's gate and databases, mdpipe on both EC2 machines ran over it); `talaria.dmap=1` keeps
/// the map without it, `talaria.dmap=0` has neither. `asked`: `talaria.dmap=strict` was given,
/// so a refusal is an error worth a loud line. True when it is on.
pub fn strict_init(want: bool, asked: bool) -> bool {
    use core::sync::atomic::Ordering;
    if !want { return false; }
    if !crate::mem::DMAP_ON.load(Ordering::Relaxed) || !KWIN_ON.load(Ordering::Relaxed) {
        if asked {
            crate::println!("talaria: talaria.dmap=strict REFUSED — it needs the direct map and the stacks' window (talaria.kwin=0?)");
        } else {
            crate::println!("talaria: the direct map is not strict — it needs the map and the stacks' window");
        }
        return false;
    }
    // the process spaces made before would keep the identity: refuse rather than half-apply
    let live = AS_LIVE.load(Ordering::Relaxed);
    if live > 0 {
        crate::println!("talaria: talaria.dmap=strict REFUSED — {} address spaces exist already", live);
        return false;
    }
    // Stage 7.4: the image runs at its high alias, so its low identity is nobody's in a process
    // space any more — only leaf 0 stays (x86's AP trampoline at 0x8000, the BIOS area). With
    // `KIMG_OFF` 0 the image still runs by its identity, and everything below its end stays.
    let keep = if crate::mem::KIMG_OFF != 0 { 0x20_0000 } else { (crate::mem::kernel_hi() + 0x1F_FFFF) & !0x1F_FFFF };
    STRICT_KEEP.store(keep, Ordering::Relaxed);
    STRICT.store(true, Ordering::SeqCst);
    crate::println!("talaria: the direct map is STRICT: process spaces carry no identity leaf over the map's RAM (below {:#x} it stays)",
        STRICT_KEEP.load(Ordering::Relaxed));
    true
}
#[inline(always)]
pub fn strict() -> bool { STRICT.load(core::sync::atomic::Ordering::Relaxed) }
/// Does strict take the identity at `va` out of a process space.
#[inline]
pub fn strict_drops(va: u64) -> bool {
    strict() && va >= STRICT_KEEP.load(core::sync::atomic::Ordering::Relaxed) && crate::mem::dmap_covers(va)
}
/// Does strict take the whole identity gigabyte at `g` out of a process space: the devices'
/// window is on, no RAM is in it (the kernel reaches its devices through the window; a
/// gigabyte of nothing is nobody's), and it holds no ivshmem (handed to ring 3 by its identity).
pub fn strict_gb_drops(g: u64) -> bool {
    let g = g & !((1u64 << 30) - 1);
    if !strict() || !MWIN_ON.load(core::sync::atomic::Ordering::Relaxed) || ram_touches(g, g + (1 << 30)) { return false; }
    let (ib, isz) = crate::pci::ivshmem();
    !(ib != 0 && ib < g + (1 << 30) && ib + isz > g)
}
/// The gate's count (`dmapstrict`): identity gigabytes with no RAM in them (the devices', the
/// holes') in space `pml4`'s first top-level slot — 0 in a process space under strict.
pub fn strict_devgb_in(pml4: u64) -> u64 {
    // SAFETY: P1 + P2 — a read-only walk of a space the caller owns.
    unsafe {
        let e4 = pte_r(kptr::<Pte>(pml4 & ADDR_MASK));
        if e4 & PTE_P == 0 { return 0; }
        let pdpt = kptr::<Pte>(e4 & ADDR_MASK);
        (0..512u64).filter(|&l3| {
            let (g, e3) = (l3 << 30, pte_r(pdpt.add(l3 as usize)));
            e3 & PTE_P != 0 && (e3 & PTE_PS == 0 || e3 & 0x000F_FFFF_C000_0000 == g) && strict_gb_drops(g)
        }).count() as u64
    }
}
/// May a process put a `MAP_FIXED` mapping at [vaddr, vaddr + pages) below 4 GiB: only under
/// strict, and only where its space has no identity leaf left — RAM the map covers (above the
/// image) and gigabytes with no RAM (devices in the window, holes). The image, the firmware's
/// tables and anything the kernel still reaches by its identity stay refused.
pub fn fixed_low_ok(vaddr: u64, pages: u64) -> bool {
    if !strict() { return false; }
    let end = vaddr.saturating_add(pages * 4096).min(4u64 << 30);
    let mut c = vaddr & !0x1F_FFFF;
    while c < end {
        if !strict_drops(c) && !strict_gb_drops(c) { return false; }
        c += 0x20_0000;
    }
    true
}
/// May a process change the rights of [vaddr, +pages) below 4 GiB: strict dropped every identity
/// leaf there AND the current space is a process's (never the kernel's, where the identity modes
/// run ring 3 beside the image).
pub fn proc_low_ok(vaddr: u64, pages: u64) -> bool {
    fixed_low_ok(vaddr, pages) && current_cr3() & ADDR_MASK != kernel_cr3() & ADDR_MASK
}
/// For a kernel fault's dump: was `addr` RAM by its identity, touched in a process space under strict.
pub fn strict_miss(addr: u64) -> bool {
    strict_drops(addr) && current_cr3() & ADDR_MASK != kernel_cr3() & ADDR_MASK
}
/// The same for a device (or a hole) by its identity: a gigabyte with no RAM, dropped by strict.
pub fn strict_dev_miss(addr: u64) -> bool {
    addr < USER_TOP && strict_gb_drops(addr) && current_cr3() & ADDR_MASK != kernel_cr3() & ADDR_MASK
}
/// The leaf `unmap_proc` leaves at an identity address: none at all where strict dropped it.
#[inline]
fn proc_ident_leaf(va: u64) -> u64 { if strict_drops(va) || strict_gb_drops(va) { 0 } else { ident_leaf(va) } }
#[inline]
fn proc_ident_leaf_2m(va: u64) -> u64 { if strict_drops(va) || strict_gb_drops(va) { 0 } else { ident_leaf_2m(va) } }
/// Take RAM's identity out of a fresh process space (`pdpt`: its private copy of the kernel's
/// PDPT, slot 0 of the root; `kgb`: its private gigabyte, whose PD is `pd0`). A 1G leaf the map
/// covers in part is split in THIS space (its PDPT is a copy, the kernel's leaf stays).
///
/// # Safety
/// P1 + P2 — the space is not in any CR3 yet; `pdpt` and `pd0` are its own tables.
unsafe fn strict_strip(pdpt: *mut Pte, pd0: *mut Pte, kgb: usize) {
    for l3 in 0..512usize {
        let g = (l3 as u64) << 30;
        let e3 = pte_r(pdpt.add(l3));
        if e3 & PTE_P == 0 { continue; }
        if l3 == kgb {
            for l2 in 0..512usize {
                let a = g + ((l2 as u64) << 21);
                let e2 = pte_r(pd0.add(l2));
                if e2 & PTE_P == 0 || e2 & PTE_PRIV != 0 { continue; }
                if e2 & PTE_PS != 0 { if strict_drops(a) { pte_w(pd0.add(l2), 0); } continue; }
                // a copied 4K table (the image's NX split): its pages, one by one
                let pt = kptr::<Pte>(e2 & ADDR_MASK);
                for l1 in 0..512usize {
                    let e1 = pte_r(pt.add(l1));
                    if e1 & PTE_P != 0 && e1 & PTE_PRIV == 0 && e1 & ADDR_MASK == a + ((l1 as u64) << 12) && strict_drops(a) {
                        pte_w(pt.add(l1), 0);
                    }
                }
            }
            continue;
        }
        // a gigabyte with no RAM at all: its devices are in the window, nothing of it is the
        // kernel's by its identity in a process space any more (ivshmem's stays: ring 3's by it)
        if strict_gb_drops(g) { pte_w(pdpt.add(l3), 0); continue; }
        if e3 & PTE_PS == 0 { continue; } // a shared directory of the kernel (a device's gigabyte split for WC)
        if e3 & 0x000F_FFFF_C000_0000 != g { continue; } // not an identity leaf
        let covered = (0..512u64).filter(|m| strict_drops(g + (m << 21))).count();
        if covered == 0 { continue; }
        if covered == 512 { pte_w(pdpt.add(l3), 0); continue; }
        split_1g(pdpt.add(l3));
        let pd = kptr::<Pte>(pte_r(pdpt.add(l3)) & ADDR_MASK);
        for m in 0..512usize {
            if strict_drops(g + ((m as u64) << 21)) { pte_w(pd.add(m), 0); }
        }
    }
}
/// The gate's walk (`dmapstrict`): the identity leaves over the map's RAM in space `pml4`
/// (outside the image), as 4K pages — 0 under strict.
pub fn strict_count_in(pml4: u64) -> u64 {
    let mut n = 0u64;
    // SAFETY: P1 + P2 — a read-only walk of a space the caller owns.
    unsafe {
        let e4 = pte_r(kptr::<Pte>(pml4 & ADDR_MASK));
        if e4 & PTE_P == 0 { return 0; }
        let pdpt = kptr::<Pte>(e4 & ADDR_MASK);
        let keep = (crate::mem::kernel_hi() + 0x1F_FFFF) & !0x1F_FFFF;
        let ram = |a: u64| a >= keep && crate::mem::dmap_covers(a);
        for l3 in 0..512usize {
            let g = (l3 as u64) << 30;
            let e3 = pte_r(pdpt.add(l3));
            if e3 & PTE_P == 0 { continue; }
            if e3 & PTE_PS != 0 {
                if e3 & 0x000F_FFFF_C000_0000 == g { n += (0..512u64).filter(|m| ram(g + (m << 21))).count() as u64 * 512; }
                continue;
            }
            let pd = kptr::<Pte>(e3 & ADDR_MASK);
            for l2 in 0..512usize {
                let a = g + ((l2 as u64) << 21);
                let e2 = pte_r(pd.add(l2));
                if e2 & PTE_P == 0 || e2 & PTE_PRIV != 0 { continue; }
                if e2 & PTE_PS != 0 { if e2 & 0x000F_FFFF_FFE0_0000 == a && ram(a) { n += 512; } continue; }
                let pt = kptr::<Pte>(e2 & ADDR_MASK);
                for l1 in 0..512usize {
                    let va = a + ((l1 as u64) << 12);
                    let e1 = pte_r(pt.add(l1));
                    if e1 & PTE_P != 0 && e1 & PTE_PRIV == 0 && e1 & ADDR_MASK == va && ram(va) { n += 1; }
                }
            }
        }
    }
    n
}

/// The window's leaf slot for `va`, its tables created on the way (a CAS per level: two cores
/// may build stacks at once). None when a table frame cannot be had.
///
/// # Safety
/// P1 + P2 + P3: `va` is in the window and `kwin_init` succeeded.
unsafe fn kwin_pte(va: u64) -> Option<*mut Pte> {
    let pml4 = kptr::<Pte>(KERNEL_PML4.load(core::sync::atomic::Ordering::Relaxed));
    let mut t = kptr::<Pte>(pte_r(pml4.add(((va >> 39) & 0x1FF) as usize)) & ADDR_MASK);
    for shift in [30u64, 21] {
        let e = t.add(((va >> shift) & 0x1FF) as usize);
        let old = pte_r(e);
        if old & PTE_P == 0 {
            let f = crate::mem::alloc_any_frame()?;
            core::ptr::write_bytes(kptr::<u8>(f), 0, 4096);
            if !install_table(e, old, f, PTE_P | PTE_RW) { crate::mem::free_contig(f, 1); }
        }
        t = kptr::<Pte>(pte_r(e) & ADDR_MASK);
    }
    Some(t.add(((va >> 12) & 0x1FF) as usize))
}

/// Map frame `pa` at window page `va` (never mapped before). False when a table frame is
/// missing; the page then stays a hole.
pub fn kwin_map(va: u64, pa: u64) -> bool {
    // SAFETY: P1 + P2 + P3 + P4 — the page was never mapped, so no TLB holds it and nothing
    // runs on it; the leaf is written once and published by the barrier of `kwin_publish`.
    unsafe {
        let Some(e) = kwin_pte(va) else { return false; };
        pte_w(e, pa | PTE_P | PTE_RW | PTE_NX | PTE_D);
    }
    true
}
/// Make the leaves just written visible before another core may run on them (arm64: a store to
/// a table must be ordered before a walk by `dsb`; x86: a no-op beyond the compiler fence).
pub fn kwin_publish() { mmu::tables_published(); }
/// Top-level entry `slot` of the space `pml4_phys` (decoded) — the gates compare a process's
/// window and map slots with the reference table's.
pub fn top_entry_in(pml4_phys: u64, slot: usize) -> u64 {
    // SAFETY: P1 + P2 — one read of a live root the caller owns.
    unsafe { pte_r(kptr::<Pte>(pml4_phys & ADDR_MASK).add(slot & 0x1FF)) }
}
/// The frame behind window page `va`, or None (the guard, a page never mapped).
pub fn kwin_frame(va: u64) -> Option<u64> {
    // SAFETY: P1 + P2 — a read-only walk of the window's shared tables.
    unsafe {
        let pml4 = kptr::<Pte>(KERNEL_PML4.load(core::sync::atomic::Ordering::Relaxed));
        let mut t = pml4;
        for shift in [39u64, 30, 21] {
            let e = pte_r(t.add(((va >> shift) & 0x1FF) as usize));
            if e & PTE_P == 0 { return None; }
            t = kptr::<Pte>(e & ADDR_MASK);
        }
        let e = pte_r(t.add(((va >> 12) & 0x1FF) as usize));
        if e & PTE_P == 0 { None } else { Some(e & ADDR_MASK) }
    }
}

// ── the per-page / per-segment protection (hardening) ─────────────────────────
// A walker over the active tables (CR3): it splits 1G→2M→4K on demand and sets
// the rights on a page. The identity mapping (vaddr==paddr below 4 GB) => the splitting
// is trivial: the sub-entries inherit the physical base and the parent's flags.

/// A table entry as the hardware holds it. Distinct from `u64` on purpose: the ONLY way
/// to read or write one is `pte_r`/`pte_w`/`pte_cas`, which translate between the
/// canonical format this module computes in and the machine's (`arch::mmu::{enc, dec}`
/// — the identity on x86-64). A raw `*slot` does not compile, so no access can miss
/// the translation.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Pte(u64);
/// # Safety
/// P1 + P2: `p` points into a live table.
#[inline(always)]
unsafe fn pte_r(p: *const Pte) -> u64 { mmu::dec(core::ptr::read_volatile(p).0) }
/// # Safety
/// P1 + P2 + P4: `p` points into a live table; the caller flushes what the write staled.
#[inline(always)]
#[track_caller]
unsafe fn pte_w(p: *mut Pte, canonical: u64) {
    if PTLOG_ON.load(core::sync::atomic::Ordering::Relaxed) { ptlog(crate::mem::vaddr(p), core::ptr::read_volatile(p).0, mmu::enc(canonical), core::panic::Location::caller().line()); }
    core::ptr::write_volatile(p, Pte(mmu::enc(canonical)))
}

// ── THE PAGE TABLES' JOURNAL (`talaria.ptlog=1`): who wrote which entry ────────────────────
//
// A page whose content changes under a thread with no syscall touching it (a ClickHouse
// thread's stack word read back as 0 on arm64, ~30 % of runs on four cores, none on one) is a
// table entry replaced behind the owner's back. Every write of an entry goes into a ring:
// the entry's address, the old and the new raw value, the core, the task and the line of
// paging.rs that wrote it; `ptlog_dump(entry)` prints the history of one entry.
pub static PTLOG_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
const PTLOG_N: usize = 1 << 16;
static PTLOG: [[core::sync::atomic::AtomicU64; 4]; PTLOG_N] = [const { [const { core::sync::atomic::AtomicU64::new(0) }; 4] }; PTLOG_N];
static PTLOG_I: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
#[inline(never)]
fn ptlog(entry: u64, old: u64, new: u64, line: u32) {
    use core::sync::atomic::Ordering::Relaxed;
    let i = PTLOG_I.fetch_add(1, Relaxed) % PTLOG_N;
    let who = (crate::percpu::cpu_id() as u64) << 56 | (crate::sched::current_id() as u64 & 0xFFFF) << 32 | line as u64;
    PTLOG[i][0].store(entry, Relaxed); PTLOG[i][1].store(old, Relaxed); PTLOG[i][2].store(new, Relaxed); PTLOG[i][3].store(who, Relaxed);
}
/// The journal's records whose old or new value names the frame `pa` (any entry, any AS).
#[allow(dead_code)] // a tool for a hunt: called by hand where a frame is suspected
pub fn ptlog_frame(pa: u64) {
    use core::sync::atomic::Ordering::Relaxed;
    if !PTLOG_ON.load(Relaxed) { return; }
    let n = PTLOG_I.load(Relaxed);
    let from = n.saturating_sub(PTLOG_N);
    let mut k = 0;
    for j in from..n {
        let r = &PTLOG[j % PTLOG_N];
        let (o, nw) = (mmu::dec(r[1].load(Relaxed)), mmu::dec(r[2].load(Relaxed)));
        let hit = |v: u64| v & PTE_P != 0 && (v & ADDR_MASK) == pa || v & PTE_P != 0 && v & PTE_PS != 0 && (v & ADDR_MASK) <= pa && pa < (v & ADDR_MASK) + 0x20_0000;
        if !hit(o) && !hit(nw) { continue; }
        let who = r[3].load(Relaxed);
        crate::println!("ptlog: frame {:#x}: #{} entry {:#x}: {:#x} -> {:#x} by core {} t{} at paging.rs:{}", pa, j, r[0].load(Relaxed),
            r[1].load(Relaxed), r[2].load(Relaxed), who >> 56, (who >> 32) & 0xFFFF, who & 0xFFFF_FFFF);
        k += 1; if k >= 24 { break; }
    }
}
/// The journal's records of the entry that maps `vaddr` in the table `root`.
#[cfg_attr(target_arch = "x86_64", allow(dead_code))] // arm64's fault path prints it; x86's has not been wired yet
pub fn ptlog_dump(root: u64, vaddr: u64) {
    use core::sync::atomic::Ordering::Relaxed;
    if !PTLOG_ON.load(Relaxed) { return; }
    // SAFETY: P1 + P2 — a read-only walk for a diagnosis.
    let Some(pt) = (unsafe { pt_for(root, vaddr, false) }) else { crate::println!("ptlog: {:#x} has no 4K entry", vaddr); return; };
    let e = crate::mem::vaddr(unsafe { pt.add(pt_idx(vaddr)) });
    let n = PTLOG_I.load(Relaxed);
    let from = n.saturating_sub(PTLOG_N);
    crate::println!("ptlog: the entry of {:#x} at {:#x} (now {:#x}); {} writes journaled:", vaddr, e, unsafe { core::ptr::read_volatile(crate::mem::vptr::<u64>(e)) }, n);
    let mut k = 0;
    for j in from..n {
        let r = &PTLOG[j % PTLOG_N];
        if r[0].load(Relaxed) != e { continue; }
        let who = r[3].load(Relaxed);
        crate::println!("ptlog:   #{} {:#x} -> {:#x} by core {} t{} at paging.rs:{}", j, r[1].load(Relaxed), r[2].load(Relaxed), who >> 56, (who >> 32) & 0xFFFF, who & 0xFFFF_FFFF);
        k += 1; if k >= 24 { break; }
    }
}
/// A compare-and-swap of one entry (P3): the slot is a naturally aligned 8-byte word the
/// hardware reads atomically, on both machines.
/// # Safety
/// P1 + P2 + P3, as `install_table`.
#[inline(always)]
#[track_caller]
unsafe fn pte_cas(p: *mut Pte, expect: u64, new: u64) -> bool {
    let a = &*(p as *const core::sync::atomic::AtomicU64);
    let ok = a.compare_exchange(mmu::enc(expect), mmu::enc(new), core::sync::atomic::Ordering::SeqCst,
                       core::sync::atomic::Ordering::SeqCst).is_ok();
    if ok && PTLOG_ON.load(core::sync::atomic::Ordering::Relaxed) { ptlog(crate::mem::vaddr(p), mmu::enc(expect), mmu::enc(new), core::panic::Location::caller().line()); }
    ok
}

const PTE_P:  u64 = 1;         // present
const PTE_RW: u64 = 1 << 1;    // writable
const PTE_US: u64 = 1 << 2;    // user-accessible
const PTE_PS: u64 = 1 << 7;    // page-size (a 2M/1G leaf)
const PTE_PWT: u64 = 1 << 3;   // write-through (with PCD: the uncached type of MMIO)
const PTE_PCD: u64 = 1 << 4;   // cache-disable
const PTE_UC: u64 = PTE_PWT | PTE_PCD; // the PAT slot 3: UC — device memory
const PTE_NX: u64 = 1 << 63;   // no-execute (it requires EFER.NXE)
const PTE_PRIV: u64 = 1 << 10; // an OS bit: a process's private page (for the fork/COW bypass)
/// An OS bit: the page is mapped, but access to it is FORBIDDEN (`mprotect(PROT_NONE)`).
/// The frame behind it stays, present is removed — an access gives a #PF, and munmap must free
/// such a frame (otherwise a PROT_NONE reservation would leak).
///
/// What it is for at all: the JVM polls the "safepoint page" by making it PROT_NONE and catching a
/// SIGSEGV. While PROT_NONE stayed a readable page, the poll never gave
/// a fault — the threads did not come to a safepoint, and the garbage collection/a handshake would hang
/// silently. The same is needed by the guard pages of the threads' stacks.
const PTE_PNONE: u64 = 1 << 9;
/// An OS bit: the page is RESERVED, there is no frame behind it yet — it will be allocated and
/// zeroed at the first access (demand-zero). The rights the page will get
/// lie in LAZY_W/LAZY_X: an mprotect before the first touch corrects them without materialising it.
///
/// What it is for: an anonymous mmap is most often A RESERVATION OF ADDRESSES rather than memory. At its
/// start the JVM reserves the heap, the code cache and the class space entirely (hundreds of MB), touches
/// percent of it, and a greedy allocation of frames ate the machine's whole physical memory: with -Xmx256m
/// the kernel handed out ~3.4 GB of frames and ran into the ceiling. The same with jemalloc/glibc.
const PTE_LAZY: u64 = 1 << 11;
const LAZY_W: u64 = 1 << 52;   // materialise it writable
const LAZY_X: u64 = 1 << 53;   // materialise it executable
/// A lazy reservation at the level of THE DIRECTORY, but with the ordinary, 4-kilobyte granularity.
///
/// A reservation of address space is the most frequent operation of a large runtime:
/// ClickHouse and the JVM map hundreds of megabytes of which they touch a small part.
/// Previously its own table entry was written for every 4 KB of such a reservation, that is,
/// 131 thousand entries for half a gigabyte, and an `mmap` cost 1.7 ms (measured by
/// the histogram of the syscalls). Now a reservation is written as ONE directory entry per
/// every 2 MB, and at the first touch it is unfolded into a table of 512
/// lazy entries (`split_2m_lazy`) — a cluster of 64 KB is still materialised
/// rather than two megabytes. It differs from a huge page by exactly this bit:
/// there the first touch gives 2 MB entirely, here it unfolds a table.
const LAZY_4K: u64 = 1 << 54;
const PTE_D: u64 = 1 << 6;     // dirty (it is raised by the processor at a write)
/// An OS bit: a guard page — present is removed ON PURPOSE and an access to it
/// must give a #PF EVEN TO THE KERNEL (the bottom of a kernel stack: an overflow is better
/// caught by a fault than silently overwriting a neighbour). The only lawful
/// exception from the identity invariant (see `ident_leaf`/`ident_check`).
const PTE_GUARD: u64 = 1 << 55;
/// THE IDENTITY WINDOW'S BOUNDARY. Below it the kernel addresses PHYSICAL frames by
/// the same number as by a virtual one (the zeroing of a fresh frame, a COW copy,
/// the kernel stacks, the walk over the page tables, the FS block cache, the DMA rings).
/// The frame allocator hands out only from here (`mem::init` cuts END at 4 GB),
/// while the identity 0..4 GB is built in `init` and copied into EVERY address space.
const IDENT_HI: u64 = 4u64 << 30;

/// THE ENTRY THAT IS LEFT AFTER THE WORKLOAD REMOVES A MAPPING.
///
/// A MINE defused here. In a process's address space the low
/// region is ITS OWN copy of the GB0 directory, and the workload is entitled to punch a hole in
/// it with its `munmap`. Previously a NON-PRESENT stub was left in the place of a removed page —
/// and any kernel code that touches that physical frame by its
/// identity address, while executing under THAT AS, got a #PF in ring 0 and killed the machine.
/// One such path (the cleanup of an AS) was caught with PostgreSQL, but there were more of them:
/// the zeroing of a fresh frame, the cleanup of the kernel stacks FROM A TICK, the FS block cache.
///
/// THE CAUSE is cured rather than every place: the removal of a mapping in the identity window does not
/// leave a hole but RETURNS the original identity leaf without the U/S bit. For
/// ring 3 that is exactly the same #PF (there is no UAF), for the kernel it is an intact identity in ANY AS.
/// Above the window (the workload's private region) the former non-present stub stays.
#[inline]
fn ident_leaf(va: u64) -> u64 {
    if va < IDENT_HI { (va & ADDR_MASK) | PTE_P | PTE_RW | PTE_NX } else { va & ADDR_MASK }
}
/// The same for a 2M leaf (a `munmap` of a whole huge page).
#[inline]
fn ident_leaf_2m(va: u64) -> u64 {
    if va < IDENT_HI { (va & !0x1F_FFFF) | PTE_P | PTE_RW | PTE_PS | PTE_NX } else { 0 }
}
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000; // the physical address [51:12]
// P|RW|US|PWT|PCD|NX + the OS bits: the splitting of a 2M leaf OF THE WORKLOAD must preserve
// the sign of a private page (otherwise munmap will not return the frames, and fork will not do a COW).
// And D: the 4K pages of a written 2M leaf are written.
const INHERIT: u64 = PTE_P | PTE_RW | PTE_US | PTE_UC | PTE_NX | PTE_PRIV | PTE_COW | PTE_D;

/// # Safety
/// P1 + P2: the root table of the CURRENT address space, taken from CR3. The
/// pointer is valid for as long as this core does not switch CR3 — that is, for the
/// walk it is used in.
unsafe fn cr3_root() -> *mut Pte {
    kptr::<Pte>(mmu::root() & ADDR_MASK)
}

/// # Safety
/// P5: a CR3 reload on THIS core. It must be called after the table changes it
/// publishes (P4), and never while another core is mid-walk of a table this core is
/// freeing — that ordering is the shootdown's business.
unsafe fn flush_tlb() {
    // arm64 with ASIDs: this core's flush is not enough any more — a core that ran a space
    // keeps its translations past the switch (`mmu::asid_on`), so the flush that relied on the
    // switch to clean the other cores is the broadcast one (ARM64-PLAN §12 item 3).
    #[cfg(target_arch = "aarch64")]
    if mmu::asid_on() { mmu::flush_all_is(); return; }
    mmu::flush_all();
}

/// How many pages it still makes sense to knock out one by one (`invlpg`) rather than extinguishing the TLB
/// entirely. The threshold is deliberately small: an `invlpg` costs tens of cycles, a full flush
/// costs units, but after it the kernel will have to walk the tables anew under ITS WHOLE
/// working set. For a computing thread with a set of tens of megabytes that is more expensive by
/// orders of magnitude, and it was exactly that which made ClickHouse's query times ragged: a neighbouring
/// thread called madvise/munmap, and the computation started from a cold TLB.
const INVLPG_MAX: u64 = 64;

/// Knock a range out of THIS core's TLB (or everything, if it is large).
///
/// # Safety
/// P4 + P5: as `flush_tlb`; `pages` counts 4 KiB pages from `start`.
unsafe fn flush_range(start: u64, pages: u64) {
    if pages == 0 { return; }
    if pages > INVLPG_MAX { flush_tlb(); return; }
    // arm64: the broadcast invalidations back to back under one pair of barriers (see
    // `mmu::flush_range_is`); page by page each paid its own `dsb ish`.
    #[cfg(target_arch = "aarch64")]
    mmu::flush_range_is(start & !0xFFF, pages);
    #[cfg(not(target_arch = "aarch64"))]
    {
        let mut a = start & !0xFFF;
        for _ in 0..pages {
            mmu::flush_page(a);
            a += 4096;
        }
    }
}

/// Drop the range's translations on THIS core and on every core that may hold them —
/// the pair "the local flush, then `shoot` (one of the `shootdown_range*`)" every caller wrote.
///
/// On arm64 the local `flush_range` is ALREADY the broadcast `TLBI VAAE1IS` (and the large
/// case the broadcast `flush_all_is`), so the shootdown that followed it invalidated the same
/// range a second time: a second pass and a second `dsb ish` per `munmap` — 899 of the
/// 3.3 us of PostgreSQL's per-request `munmap` on a c7g ("the acknowledgements" of the stat
/// block's unmap line, MEASUREMENTS §10.19). A caller whose local flush is the LOCAL full one
/// (`flush_tlb` before `shootdown_range_auto` in `move_mapping`) still calls the shootdown
/// itself.
///
/// # Safety
/// P4 + P5: as `flush_range`.
unsafe fn flush_range_all(start: u64, pages: u64, shoot: fn(u64, u64)) {
    #[cfg(target_arch = "aarch64")]
    {
        let _ = shoot;
        // 0 is "everything" for the shootdown the pair ended with
        if pages == 0 || pages > INVLPG_MAX { mmu::flush_all_is(); } else { mmu::flush_range_is(start & !0xFFF, pages); }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        flush_range(start, pages);
        shoot(start, pages);
    }
}

// ── THE INSTALLATION OF A TABLE INTO THE PARENT ENTRY ─────────────────────────
//
// ALL the functions below put a new page table in the place of a leaf or of an empty
// entry. Doing that with a simple write is NOT ALLOWED, and that is not pedantry.
//
// A process's page tables are SHARED by all its threads, while a split happens on
// the path of handling a fault — that is, simultaneously on several cores. Two cores
// splitting ONE AND THE SAME entry both allocated a table and both wrote its address:
// the last one won, while the loser's table stayed "hanging". Everything that the
// loser managed to materialise in it disappeared from the address space —
// a page into which a thread had just been writing became lazy again or
// vanished entirely. From the outside that looked like a SIGSEGV at a perfectly lawful
// address, once in a hundred rounds, and it depended on how long the tick turned out to be.
//
// So the parent entry is changed ONLY through a compare-and-swap from the
// observed value, and the loser frees its own table and works with
// the foreign one. The cost is one atomic operation per split, that is, per the first
// access to 2 MB; on the data path there is none of it at all.
//
// The neighbouring branches (`mem::alloc_contig` for a table) do not affect the price: they were
// there anyway.
/// # Safety
/// P1 + P2 + P3: `slot` must point at an entry of a live table (a walk's result),
/// `table` at a frame filled COMPLETELY before this call — the CAS publishes it to
/// the other cores, so anything written after it may be seen half-built. The slot
/// is treated as an `AtomicU64`, which is what a page table entry is on x86: a
/// naturally aligned 8-byte word the hardware reads atomically.
#[inline]
unsafe fn install_table(slot: *mut Pte, expect: u64, table: u64, extra: u64) -> bool {
    pte_cas(slot, expect, table | extra)
}

/// Create an EMPTY table of the next level in the place of a missing entry.
/// The same protocol as at the splits: first fill it, then a CAS, and the
/// loser of the race returns its own table and takes the foreign one.
///
/// # Safety
/// P1 + P2 + P3, as `install_table`.
unsafe fn new_level(slot: *mut Pte, what: &str) {
    let old = pte_r(slot);
    if old & PTE_P != 0 { return; }
    let t = crate::mem::alloc_any_frame().expect(what);
    core::ptr::write_bytes(kptr::<u8>(t), 0, 4096);
    if !install_table(slot, old, t, PTE_P | PTE_RW | PTE_US) {
        crate::mem::free_contig(t, 1);
    }
}

/// The same for the paths that do not panic on a lack of memory (a `?` return).
///
/// # Safety
/// P1 + P2 + P3, as `install_table`.
unsafe fn new_level_opt(slot: *mut Pte) -> Option<()> {
    let old = pte_r(slot);
    if old & PTE_P != 0 { return Some(()); }
    let t = crate::mem::alloc_any_frame()?;
    core::ptr::write_bytes(kptr::<u8>(t), 0, 4096);
    if !install_table(slot, old, t, PTE_P | PTE_RW | PTE_US) { crate::mem::free_contig(t, 1); }
    Some(())
}

/// Split a 1G leaf (a PDPT entry) into a PD of 512 2M leaves (identity, inheriting the flags).
///
/// # Safety
/// P1 + P2 + P3: `e3` points at a PDPT entry of a live table. The 512 leaves are
/// written BEFORE the CAS, so a core that sees the new PD sees it complete.
unsafe fn split_1g(e3: *mut Pte) {
    let parent = pte_r(e3);
    if parent & PTE_PS == 0 { return; }          // a neighbour managed to split it — we work with its table
    let base = parent & 0x000F_FFFF_C000_0000; // the 1G-aligned base
    let flags = parent & (INHERIT); // inherit RW/US/NX/the cacheability
    let pd = kptr::<Pte>(crate::mem::alloc_any_frame().expect("split_1g: there is no frame"));
    for m in 0..512u64 {
        pte_w(pd.add(m as usize), (base + m * 0x20_0000) | flags | PTE_PS);
    }
    if !install_table(e3, parent, kpa_of(pd), PTE_P | PTE_RW | PTE_US) {
        crate::mem::free_contig(kpa_of(pd), 1);   // we lost the race — the neighbour's table is already in place
    }
}

/// Split a LAZY 2M reservation (P=0, PS+LAZY) into 512 lazy 4K entries of the same kind.
/// It is needed when the workload asks for a 4K granularity (an mprotect/munmap of a part of a piece):
/// there are no frames behind the reservation yet, so the "split" is simply an exchange of one
/// entry for 512 with the same rights.
/// # Safety
/// P1 + P2 + P3: `e2` points at a PD entry of a live table that carries a LAZY
/// reservation (P=0, PS+LAZY) — there are no frames behind it, so the split moves
/// no data.
unsafe fn split_2m_lazy(e2: *mut Pte) {
    let parent = pte_r(e2);
    // A neighbour managed it: the entry is no longer "a lazy 2M leaf" but a ready table (or
    // a materialised huge page). There is nothing to split.
    if parent & (PTE_LAZY | PTE_PS) != (PTE_LAZY | PTE_PS) || parent & PTE_P != 0 { return; }
    HUGE_SPLIT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let flags = parent & (PTE_LAZY | PTE_PRIV | PTE_PNONE | LAZY_W | LAZY_X); // without PS and LAZY_4K: they are about the directory entry
    let pt = kptr::<Pte>(crate::mem::alloc_any_frame().expect("split_2m_lazy: there is no frame"));
    for m in 0..512usize { pte_w(pt.add(m), flags); }
    if !install_table(e2, parent, kpa_of(pt), PTE_P | PTE_RW | PTE_US) {
        crate::mem::free_contig(kpa_of(pt), 1);
    }
}

/// Split a 2M leaf (a PD entry) into a PT of 512 4K leaves (identity, inheriting the flags).
/// # Safety
/// P1 + P2 + P3: `e2` points at a PD entry of a live table that carries a 2M leaf.
/// The private/COW bits are inherited (`INHERIT`) — dropping them would make
/// `munmap` leak the frames and `fork` skip the copy.
unsafe fn split_2m(e2: *mut Pte) {
    let parent = pte_r(e2);
    if parent & PTE_PS == 0 { return; }          // a neighbour has already split it
    let base = parent & 0x000F_FFFF_FFE0_0000; // the 2M-aligned base
    let flags = parent & (INHERIT);
    let pt = kptr::<Pte>(crate::mem::alloc_any_frame().expect("split_2m: there is no frame"));
    for m in 0..512u64 {
        pte_w(pt.add(m as usize), (base + m * 0x1000) | flags);
    }
    if !install_table(e2, parent, kpa_of(pt), PTE_P | PTE_RW | PTE_US) {
        crate::mem::free_contig(kpa_of(pt), 1);
    }
}

/// A pointer to the 4K PTE for vaddr, splitting the large leaves on the way. None — the path is not mapped.
/// # Safety
/// P1 + P2: it walks the CURRENT address space, creating the missing levels
/// (`new_level`), and returns a pointer to the 4K entry — valid while this core
/// keeps this CR3.
unsafe fn pte_ptr(vaddr: u64) -> Option<*mut Pte> {
    let l4 = ((vaddr >> 39) & 0x1FF) as usize;
    let l3 = ((vaddr >> 30) & 0x1FF) as usize;
    let l2 = ((vaddr >> 21) & 0x1FF) as usize;
    let l1 = ((vaddr >> 12) & 0x1FF) as usize;
    let pml4 = cr3_root();
    let e4 = pml4.add(l4);
    if pte_r(e4) & PTE_P == 0 { return None; }
    let pdpt = kptr::<Pte>(pte_r(e4) & ADDR_MASK);
    let e3 = pdpt.add(l3);
    if pte_r(e3) & PTE_P == 0 { return None; }
    if pte_r(e3) & PTE_PS != 0 { split_1g(e3); }
    let pd = kptr::<Pte>(pte_r(e3) & ADDR_MASK);
    let e2 = pd.add(l2);
    if pte_r(e2) & PTE_P == 0 {
        if pte_r(e2) & (PTE_LAZY | PTE_PS) == (PTE_LAZY | PTE_PS) { split_2m_lazy(e2); } // a lazy 2M reservation
        else { return None; }
    } else if pte_r(e2) & PTE_PS != 0 { split_2m(e2); }
    let pt = kptr::<Pte>(pte_r(e2) & ADDR_MASK);
    Some(pt.add(l1))
}

/// Read the effective leaf entry for vaddr (at any level: 1G/2M/4K), without splitting.
/// # Safety
/// P1 + P2: a read-only walk of the CURRENT address space; it stops at the first
/// entry that is not present or is a large leaf, so it never follows a
/// non-table.
unsafe fn walk_entry(vaddr: u64) -> u64 {
    let idx = [((vaddr >> 39) & 0x1FF) as usize, ((vaddr >> 30) & 0x1FF) as usize,
               ((vaddr >> 21) & 0x1FF) as usize, ((vaddr >> 12) & 0x1FF) as usize];
    let mut tbl = cr3_root();
    let mut e = pte_r(tbl.add(idx[0]));
    for lvl in 1..4 {
        if e & PTE_P == 0 { return e; }              // it is not mapped
        if e & PTE_PS != 0 { return e; }             // a large leaf
        tbl = kptr::<Pte>(e & ADDR_MASK);
        e = pte_r(tbl.add(idx[lvl]));
    }
    e
}


/// End of the canonical lower half: ring 3 never owns an address at or above it.
pub const USER_TOP: u64 = 1 << 47;

/// May the kernel read (or, with `write`, write) `[addr, addr + len)` on behalf
/// of the current process? Every page of the range must be reachable from ring 3
/// in the current address space:
///
/// * present with U/S set (and RW, or COW that the write fault will break), or
/// * a demand-zero reservation (`PTE_LAZY`, not `PROT_NONE`) that materialises
///   on first touch — writable if the reservation is (`LAZY_W`).
///
/// In a process address space the kernel image, page tables, kernel stacks and
/// the block cache are identity-mapped WITHOUT U/S (`new_process_space`), so a
/// syscall pointing into them is refused here instead of the kernel writing
/// there itself. The walk is per leaf, so a 2 MiB or 1 GiB mapping costs one
/// step, not 512.
pub fn user_range_ok(addr: u64, len: u64, write: bool) -> bool {
    if len == 0 { return true; }
    let end = match addr.checked_add(len) { Some(e) => e, None => return false };
    if end > USER_TOP { return false; }
    // ONE DESCENT PER TABLE, NOT PER PAGE. The check used to walk the four levels for every
    // 4K page of the range: a `recv` asking for 1 MiB (tlgen's TL_RBUF, on every poll of an
    // empty socket) paid 256 walks — 3 us on a c7g, the whole difference between its 4.3-us
    // empty receive and the 0.7 us it spent in the stack (ARM64-PLAN §11 item 2a). Linux pays
    // nothing for the length (it copies and faults). Now the walk stops at the 4K table and
    // reads its consecutive entries in place up to the table's 2 MiB boundary; a 2M/1G leaf
    // or a missing table still answers for its whole span at once. `kernel/user/gatecost`.
    let mut a = addr;
    while a < end {
        // SAFETY: P1 + P2 — a read-only walk of the CURRENT address space — the caller is the
        // syscall path, which runs in the process's own CR3.
        let (e, span, tbl) = unsafe { walk_leaf_tbl(a) };
        match tbl {
            None => { if !leaf_ok(e, write) { return false; } a = (a & !(span - 1)) + span; }
            Some(t) => {
                let lim = ((a & !((1u64 << 21) - 1)) + (1 << 21)).min(end); // this table's reach
                let mut i = ((a >> 12) & 0x1FF) as usize;
                while a < lim {
                    // SAFETY: P1 — `t` is the live 4K table of this 2 MiB region, `i` its index.
                    if !leaf_ok(unsafe { pte_r(t.add(i)) }, write) { return false; }
                    i += 1; a = (a & !0xFFF) + 0x1000;
                }
            }
        }
    }
    true
}
/// One entry's verdict for `user_range_ok`: present — the process's, writable if asked (a COW
/// page is written through the fault); absent — a lazy mark that is not PROT_NONE, writable
/// if asked.
#[inline(always)]
fn leaf_ok(e: u64, write: bool) -> bool {
    if e & PTE_P != 0 { e & PTE_US != 0 && !(write && e & PTE_RW == 0 && e & PTE_COW == 0) }
    else { e & PTE_LAZY != 0 && e & PTE_PNONE == 0 && !(write && e & LAZY_W == 0) }
}
/// The leaf entry for `vaddr` and the size of the span it answers for (4 KiB, 2 MiB or
/// 1 GiB; a non-present entry above the last level covers its whole span: nothing inside
/// it is mapped), and the 4K table itself when the walk reached one: the caller then reads
/// its neighbouring entries without descending again.
/// # Safety
/// P1 + P2, as `walk_entry`.
unsafe fn walk_leaf_tbl(vaddr: u64) -> (u64, u64, Option<*mut Pte>) {
    const SPAN: [u64; 4] = [1 << 39, 1 << 30, 1 << 21, 1 << 12];
    let idx = [((vaddr >> 39) & 0x1FF) as usize, ((vaddr >> 30) & 0x1FF) as usize,
               ((vaddr >> 21) & 0x1FF) as usize, ((vaddr >> 12) & 0x1FF) as usize];
    let mut tbl = cr3_root();
    let mut e = pte_r(tbl.add(idx[0]));
    for lvl in 1..4 {
        if e & PTE_P == 0 || e & PTE_PS != 0 { return (e, SPAN[lvl - 1], None); }
        tbl = kptr::<Pte>(e & ADDR_MASK);
        e = pte_r(tbl.add(idx[lvl]));
    }
    (e, SPAN[3], Some(tbl))
}

/// A page's rights (for introspection/tests): (present, writable, user, exec).
/// §8.5: take the Dirty bit of a 4K page (the processor raises it at a write) and reset
/// it. That way we learn that the user wrote into a MAP_SHARED file mapping without
/// intercepting every write with a fault. false — there is no page, the leaf is a large one or
/// there was no write.
/// Force the page of `vaddr` to a 4K leaf, splitting the 2M/1G leaf that covers it, and say
/// whether it now has a 4K entry. `protect`/`protect_none` deliberately keep a huge leaf
/// whole (a heap must not shatter), so a caller that needs a genuine per-page mapping in a
/// region the kernel maps as 2M (its own GB1, `mmu::gb_kind`) asks for it here — the dirty
/// probe of the bring-up and `talaria.test=dirtyprot` (both machines), which otherwise read
/// the 2M leaf's crude "always dirty".
pub fn split_to_4k(vaddr: u64) -> bool {
    // SAFETY: P1 + P2 + P3 — `pte_ptr` walks the current root, splitting the large leaves it
    // meets through the CAS; nothing else holds a pointer into those tables here.
    unsafe { pte_ptr(vaddr).is_some() }
}

pub fn take_dirty(vaddr: u64) -> bool {
    // SAFETY: P1 + P2 — a walk of the current CR3 with a presence check at every level; the write
    // is the atomic clearing of the Dirty bit of a 4K leaf, which the hardware may set
    // again at any time — losing that race only costs one extra writeback.
    unsafe {
        let idx = [((vaddr >> 39) & 0x1FF) as usize, ((vaddr >> 30) & 0x1FF) as usize,
                   ((vaddr >> 21) & 0x1FF) as usize, ((vaddr >> 12) & 0x1FF) as usize];
        let root = current_cr3() & ADDR_MASK;
        let e4 = pte_r(kptr::<Pte>(root).add(idx[0])); if e4 & PTE_P == 0 { return false; }
        let e3 = pte_r(kptr::<Pte>(e4 & ADDR_MASK).add(idx[1])); if e3 & PTE_P == 0 { return false; }
        if e3 & PTE_PS != 0 { return true; } // a 1G leaf: we consider it changed (crudely, but safely)
        let e2 = pte_r(kptr::<Pte>(e3 & ADDR_MASK).add(idx[2])); if e2 & PTE_P == 0 { return false; }
        if e2 & PTE_PS != 0 { return true; } // a 2M leaf (the kernel's identity region)
        let pte = kptr::<Pte>(e2 & ADDR_MASK).add(idx[3]);
        let v = pte_r(pte);
        if v & PTE_P == 0 || v & PTE_D == 0 { return false; }
        pte_w(pte, v & !PTE_D);
        mmu::flush_page(vaddr);
        true
    }
}

pub fn query(vaddr: u64) -> (bool, bool, bool, bool) {
    // SAFETY: P1 + P2 — a read-only walk.
    let e = unsafe { walk_entry(vaddr) };
    let present = e & PTE_P != 0;
    (present, e & PTE_RW != 0, e & PTE_US != 0, present && (e & PTE_NX == 0))
}

/// Is the page at `vaddr` marked copy-on-write (the fault report).
pub fn is_cow(vaddr: u64) -> bool { leaf_entry(vaddr).is_some_and(|e| e & PTE_COW != 0) }
/// A page's leaf entry in its entirety (for the cache-policy tests): None — it is not mapped.
/// The fault report: the entries on the way to `vaddr` in the current space, top level first
/// (0 past the first absent one), and each table's frame.
pub fn walk_path(vaddr: u64) -> [(u64, u64); 4] {
    let mut out = [(0u64, 0u64); 4];
    let mut table = current_cr3() & ADDR_MASK;
    for (lvl, shift) in [39u32, 30, 21, 12].iter().enumerate() {
        // SAFETY: P1 — a read-only walk of the current space's tables through the direct map.
        let e = unsafe { pte_r(kptr::<Pte>(table).add(((vaddr >> shift) & 511) as usize)) };
        out[lvl] = (table, e);
        if e & PTE_P == 0 || (lvl > 0 && lvl < 3 && e & PTE_PS != 0) { break; }
        table = e & ADDR_MASK;
    }
    out
}
pub fn leaf_entry(vaddr: u64) -> Option<u64> {
    // SAFETY: P1 + P2 — a read-only walk.
    let e = unsafe { walk_entry(vaddr) };
    if e & PTE_P == 0 { None } else { Some(e) }
}

#[cfg(target_arch = "x86_64")]
pub use mmu::pat_init;
pub use mmu::{pat_read, PAT_VALUE};

/// Switch the window [phys, phys+len) to write-combining (a 4K granularity, the leaf
/// is split on the way). The window's neighbouring pages do NOT change their type — in the ENA's case, in the same
/// gigabyte there lies BAR0 with the registers, for which WC is contraindicated.
/// true — the whole range has been switched. The descriptor bits are the MMU's
/// (`arch::mmu::with_wc`: the PAT slot on x86, Normal non-cacheable on arm64).
/// `phys` is the kernel's address of the range: a devices' window address (`mmio_va`, what
/// `pci::bar` gives) is split in the window's shared table; anything else is the identity.
pub fn map_mmio_wc(phys: u64, len: u64) -> bool {
    if len == 0 || (!crate::mem::in_mwin(phys) && map_1g(phys, false) == 0) { return false; }
    let (first, last) = (phys & !0xFFF, (phys + len - 1) & !0xFFF);
    // SAFETY: P1 + P2 + P4 — the range belongs to a device (a BAR), not to the allocator; the
    // entries are rewritten with the WC attribute and the range is flushed.
    unsafe {
        let mut p = first;
        loop {
            let e = match pte_ptr(p) { Some(e) => e, None => return false };
            pte_w(e, mmu::with_wc(pte_r(e)));
            if p == last { break; }
            p += 0x1000;
        }
        flush_tlb();
    }
    true
}

/// Set the rights of the user pages on the range [vaddr, vaddr+len): present+US,
/// writable by the flag, NX if !exec (W^X). The pages' physical base is preserved (identity).
/// `mprotect(PROT_NONE)`: remove present, preserve the frame and the OS bits (PNONE).
/// An access to such a page is a real #PF (a SIGSEGV to the workload), while a munmap
/// will free the frame by the PNONE just as by the present.
pub fn protect_none(vaddr: u64, len: u64) {
    if len == 0 { return; }
    let start = vaddr & !0xFFF;
    let end = (vaddr + len + 0xFFF) & !0xFFF;
    let mut a = start;
    while a < end {
        // SAFETY: P1 + P2 + P3 — a walk of the current AS, splitting the large leaves (by CAS) so
        // that the rights can be set per 4K page.
        unsafe {
            if a & 0x1F_FFFF == 0 && end - a >= 0x20_0000 {
                if let Some(pde) = pde_ptr_existing(a) {   // a whole huge page
                    let e = pte_r(pde);
                    if e & PTE_PS != 0 && (e & PTE_P != 0 || e & PTE_LAZY != 0) {
                        pte_w(pde, (e & !PTE_P) | PTE_PNONE);
                        a += 0x20_0000;
                        continue;
                    }
                }
            }
            let stop = chunk_end(a, end);
            let pt = match pt_for(kpa_of(cr3_root()), a, false) { Some(p) => p, None => { a = stop; continue; } };
            while a < stop {
                let pte = pt.add(pt_idx(a));
                let old = pte_r(pte);
                if old & PTE_LAZY != 0 { pte_w(pte, old | PTE_PNONE); } // a reservation: only the mark
                else if old & PTE_P != 0 { pte_w(pte, (old & !PTE_P) | PTE_PNONE); }
                a += 0x1000;
            }
        }
    }
    // SAFETY: P4 — the range's translations are stale after the loop above.
    unsafe { flush_range_all(start, (end - start) / 4096, shootdown_range_auto); }
}

pub fn protect(vaddr: u64, len: u64, writable: bool, exec: bool) {
    if len == 0 { return; }
    let start = vaddr & !0xFFF;
    let end = (vaddr + len + 0xFFF) & !0xFFF;
    let mut a = start;
    while a < end {
        // SAFETY: P1 + P2 + P3 — as `protect_none` — the splits go through the CAS, the leaves are
        // rewritten with the requested rights.
        unsafe {
            // A huge page entirely inside the range — we correct ONE entry.
            // Otherwise every mprotect on a heap would exchange 2M leaves for 4K ones and
            // the TLB gain would live until the very first call.
            if a & 0x1F_FFFF == 0 && end - a >= 0x20_0000 {
                if let Some(pde) = pde_ptr_existing(a) {
                    let e = pte_r(pde);
                    let huge_present = e & PTE_P != 0 && e & PTE_PS != 0;
                    let huge_lazy = e & PTE_P == 0 && e & (PTE_LAZY | PTE_PS) == (PTE_LAZY | PTE_PS);
                    if huge_present && e & PTE_PRIV != 0 && e & PTE_COW == 0 {
                        let mut ne = (e & (ADDR_MASK | PTE_PS | PTE_PRIV | PTE_D)) | PTE_P | PTE_US;
                        if writable { ne |= PTE_RW; }
                        if !exec { ne |= PTE_NX; }
                        pte_w(pde, ne);
                        if exec { crate::arch::icache::sync(kva(e & ADDR_MASK), 0x20_0000); } // mprotect(PROT_EXEC) over written code
                        a += 0x20_0000;
                        continue;
                    }
                    if huge_lazy {
                        let mut ne = (e & !(LAZY_W | LAZY_X | PTE_PNONE)) | PTE_LAZY | PTE_PS | PTE_PRIV;
                        if writable { ne |= LAZY_W; }
                        if exec { ne |= LAZY_X; }
                        pte_w(pde, ne);
                        a += 0x20_0000;
                        continue;
                    }
                }
            }
            let stop = chunk_end(a, end);
            let pt = match pt_for(kpa_of(cr3_root()), a, false) { Some(p) => p, None => { a = stop; continue; } };
            while a < stop {
                let pte = pt.add(pt_idx(a));
                // A LAZY reservation: there is no frame yet — we correct only the FUTURE rights (and remove PROT_NONE)
                if pte_r(pte) & PTE_LAZY != 0 {
                    let mut e = (pte_r(pte) & !(LAZY_W | LAZY_X | PTE_PNONE)) | PTE_LAZY;
                    if writable { e |= LAZY_W; }
                    if exec { e |= LAZY_X; }
                    pte_w(pte, e);
                    a += 0x1000;
                    continue;
                }
                // the page was PROT_NONE — return present and remove the mark
                if pte_r(pte) & PTE_PNONE != 0 { pte_w(pte, (pte_r(pte) | PTE_P) & !PTE_PNONE); }
                // A COW page + a request to make it writable: first untie the COW (a copy/the ownership),
                // otherwise a write without a fault would go into the frame shared with the child.
                // cow_fault CHANGES the table (it may split a 2M leaf), so the write
                // below goes by the reread pointer rather than by our cursor.
                if writable && pte_r(pte) & PTE_COW != 0 && pte_r(pte) & PTE_P != 0 {
                    let _ = cow_fault(a);
                    if let Some(fresh) = pte_ptr(a) {
                        let old = pte_r(fresh);
                        let mut e = (old & ADDR_MASK) | PTE_P | PTE_US | (old & (PTE_PRIV | PTE_COW | PTE_D));
                        if writable && old & PTE_COW == 0 { e |= PTE_RW; }
                        if !exec { e |= PTE_NX; }
                        pte_w(fresh, e);
                        if exec && old & PTE_P != 0 { crate::arch::icache::sync(a, 4096); }
                    }
                    a += 0x1000;
                    continue;
                }
                let old = pte_r(pte);
                let phys = old & ADDR_MASK;
                // PRIV/COW are preserved (fork/munmap), and D: a written page made read-only is
                // still written — dropping D here lost a MAP_SHARED page's writeback after an
                // mprotect(PROT_READ), on both machines (`talaria.test=dirtyprot`).
                let mut e = phys | PTE_P | PTE_US | (old & (PTE_PRIV | PTE_COW | PTE_D));
                if writable && old & PTE_COW == 0 { e |= PTE_RW; }
                if !exec { e |= PTE_NX; }
                pte_w(pte, e);
                // mprotect(PROT_EXEC) after a write (a JIT that does not `__clear_cache`, a loader
                // that maps RW and flips): the instruction cache must see the bytes.
                if exec && old & PTE_P != 0 { crate::arch::icache::sync(a, 4096); }
                a += 0x1000;
            }
        }
    }
    // SAFETY: P4 — the range's translations are stale after the loop above.
    // the rights could have been TIGHTENED (RW->RO, the removal of X) — the foreign TLBs must see that
    unsafe { flush_range_all(start, (end - start) / 4096, shootdown_range_auto); }
}
/// A pointer to the PD entry (level 2) in the pml4, creating the PML4E/PDPTE if
/// necessary. For installing 2M leaves — nothing needs to be split.
/// # Safety
/// P1 + P2: `pml4_phys` must be a page table of an address space THIS core owns
/// (one being built, or the current one) — the walk creates the missing levels in
/// it, so a stray value would write tables into somebody else's frames.
unsafe fn pde_ptr_in(pml4_phys: u64, vaddr: u64) -> *mut Pte {
    let idx = [((vaddr >> 39) & 0x1FF) as usize, ((vaddr >> 30) & 0x1FF) as usize, ((vaddr >> 21) & 0x1FF) as usize];
    let e4 = kptr::<Pte>(pml4_phys).add(idx[0]);
    if pte_r(e4) & PTE_P == 0 { new_level(e4, "pde_in: pdpt"); }
    let e3 = kptr::<Pte>(pte_r(e4) & ADDR_MASK).add(idx[1]);
    if pte_r(e3) & PTE_P == 0 { new_level(e3, "pde_in: pd"); }
    else if pte_r(e3) & PTE_PS != 0 { split_1g(e3); }
    kptr::<Pte>(pte_r(e3) & ADDR_MASK).add(idx[2])
}

/// The PD entry for vaddr in the active AS, WITHOUT creating tables. None — there is no path.
/// The page table (512 entries of 4 KB) covering the 2 MB around `vaddr`.
///
/// Why separately from `pte_ptr`. All the operations over a range — mmap, mprotect,
/// munmap, MADV_DONTNEED — went page by page, and for EVERY one a full
/// descent over the four levels of tables was made: four accesses into a structure scattered over memory.
/// On a mapping of half a gigabyte that is 131 thousand descents, and the measurement
/// showed exactly that: `mmap` 1.7 ms, `mprotect` 2.0 ms, `munmap` 1.3 ms per
/// ONE call. ClickHouse calls them hundreds of times per query — hence the gap
/// that was neither in the computation nor in the memory (our primitives are on a par with Linux's).
///
/// The descent is made once per 2 MB, and after that 512 neighbouring entries go in a row —
/// that is, a sequential walk over one page of the table.
/// `create` — create the missing levels (it is needed by the mapping paths; the removing ones
/// have no need: what is not there need not be removed).
/// # Safety
/// P1 + P2, as `pde_ptr_in`; with `create` off it only reads.
unsafe fn pt_for(pml4_phys: u64, vaddr: u64, create: bool) -> Option<*mut Pte> {
    let idx = [((vaddr >> 39) & 0x1FF) as usize, ((vaddr >> 30) & 0x1FF) as usize,
               ((vaddr >> 21) & 0x1FF) as usize];
    let e4 = kptr::<Pte>(pml4_phys).add(idx[0]);
    if pte_r(e4) & PTE_P == 0 {
        if !create { return None; }
        new_level_opt(e4)?;
    }
    let e3 = kptr::<Pte>(pte_r(e4) & ADDR_MASK).add(idx[1]);
    if pte_r(e3) & PTE_P == 0 {
        if !create { return None; }
        new_level_opt(e3)?;
    } else if pte_r(e3) & PTE_PS != 0 { split_1g(e3); }
    let e2 = kptr::<Pte>(pte_r(e3) & ADDR_MASK).add(idx[2]);
    if pte_r(e2) & PTE_P == 0 {
        if pte_r(e2) & (PTE_LAZY | PTE_PS) == (PTE_LAZY | PTE_PS) { split_2m_lazy(e2); } // a lazy 2M reservation
        else if !create { return None; }
        else { new_level_opt(e2)?; }
    } else if pte_r(e2) & PTE_PS != 0 { split_2m(e2); }
    // REREAD rather than remember. The split and the creation of a level go through a CAS, and
    // A NEIGHBOURING core could have won: one must work with the table that
    // really stands in the entry rather than with the one we tried to install.
    if pte_r(e2) & PTE_P == 0 { return None; }
    Some(kptr::<Pte>(pte_r(e2) & ADDR_MASK))
}

/// The page's index inside the table (0..511).
#[inline]
fn pt_idx(vaddr: u64) -> usize { ((vaddr >> 12) & 0x1FF) as usize }

/// The end of the current 2M piece, but not further than `end`.
#[inline]
fn chunk_end(a: u64, end: u64) -> u64 { core::cmp::min(end, (a & !0x1F_FFFF) + 0x20_0000) }

/// # Safety
/// P1 + P2: like `pte_ptr`, but it creates nothing — `None` when the level is
/// missing.
unsafe fn pde_ptr_existing(vaddr: u64) -> Option<*mut Pte> {
    let idx = [((vaddr >> 39) & 0x1FF) as usize, ((vaddr >> 30) & 0x1FF) as usize, ((vaddr >> 21) & 0x1FF) as usize];
    let e4 = cr3_root().add(idx[0]);
    if pte_r(e4) & PTE_P == 0 { return None; }
    let e3 = kptr::<Pte>(pte_r(e4) & ADDR_MASK).add(idx[1]);
    if pte_r(e3) & PTE_P == 0 { return None; }
    if pte_r(e3) & PTE_PS != 0 { return None; } // a 1G leaf of the kernel — not our case
    Some(kptr::<Pte>(pte_r(e3) & ADDR_MASK).add(idx[2]))
}

/// How many huge pages (2 MB) were really given to the workload — it is visible in `stat:`.
pub static HUGE_GIVEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many 2M reservations were created at a mapping (the ceiling for HUGE_GIVEN).
pub static HUGE_RESERVED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many 2M reservations were exchanged for 4K ones BEFORE materialisation (somebody asked for a PTE
/// inside a reservation: an mprotect of a piece, a query of the rights, a write by the kernel).
pub static HUGE_SPLIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many times 512 contiguous frames with a 2 MB alignment were not found.
pub static HUGE_NOMEM: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Is it worth taking a 2M leaf. A huge page saves TLB misses (under a
/// hypervisor doubly so: a miss also has a nested EPT walk), but it is materialised
/// entirely — 2 MB at the first touch. So only when there is known to be a lot of memory.
/// The threshold of huge pages IN PAGES (`talaria.huge=<MB>`; 0 — switch them off entirely).
///
/// THE DEFAULT IS 32 MB, that is, in practice OFF. That is NOT "the mechanism does not
/// work" — it is a measured choice in favour of latency, see below. Previously the same
/// number stood here, but for a different reason: it was thought that there was no gain.
/// In fact the gain was not seen because the pages were never handed out at all:
/// the counter `stat: 2 MB huge pages=` showed 0 handed out with 172 reservations.
/// The previous sessions' conclusion "huge pages give no gain" was made on a
/// measurement where they were NEVER HANDED OUT — and on a suite with a measurement quantum of 1 ms
/// with test cases of 5-40 ms, where such a difference cannot be seen anyway.
///
/// The measurement on the rebuilt ClickHouse suite (every test case is 150-600 ms,
/// the median of three runs back to back, milliseconds):
///
///   test case          32 MB (it was)   2 MB (it became)   Linux
///   uniqExact 500 thou.    253               197            188
///   GROUP BY 100 thou.     235               197            236
///
/// Both rows are about A HASH TABLE: a random access over several megabytes does not
/// fit into the TLB with four-kilobyte pages, while under a hypervisor a TLB
/// miss also drags a nested EPT walk. With a Linux guest THP is on by default
/// (`always`), so this is not a head start but an equalisation of the conditions.
///
/// THE PRICE, and it is measured too — that is why the default was NOT changed.
///
/// 1. The memory: a huge page is materialised ENTIRELY. On the same run
///    the frames handed out became 468 846 against 329 673 (+42 %), the page misses —
///    2343 against 5788. That is an exchange of memory for time.
/// 2. THE LATENCY. `redis ping`, the Talaria side, two repeats back to back, p50 us:
///       the rate ~7 thou.:  27.9 against 26.6      the rate ~17 thou.: 28.2 against 25.3
///    That is, one and a half microseconds per request — steadily, in both repeats, with
///    an unchanged ceiling. The reason is clear: the TLB flushes to the neighbours on
///    ClickHouse became 1059 against 779, while the waits on them — 69 ms against 27.
///
/// Hence the rule: huge pages go to a workload that WALKS RANDOMLY OVER
/// MEGABYTES (analytics, hash tables), and not to those who measure themselves in microseconds.
/// The machine is by default optimised for the second, so the knob is set by
/// THE WORKLOAD: `services/clickhouse/run.sh` passes `talaria.huge=2`, and that
/// levels the conditions with a Linux guest, where THP is on by default (`always`).
///
/// `huge_ok` also requires half a gigabyte of free memory: under pressure the
/// mechanism backs off by itself (and that is why it does not work at all on guests with `-m 512` —
/// the gate in the regression brings up 2 GB).
pub static HUGE_MIN_PAGES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(8192);

/// Set the threshold of huge pages (megabytes; 0 — switch them off). A common point for
/// the stand's command line and for the workload's manifest (`huge=` — see `cellman`).
pub fn huge_set_mb(mb: u64) {
    let pages = if mb == 0 { u64::MAX } else { (mb * 256).max(512) };
    HUGE_MIN_PAGES.store(pages, core::sync::atomic::Ordering::SeqCst);
    crate::println!("paging: the threshold of huge pages = {} MB{}", mb, if mb == 0 { " (they are off)" } else { "" });
}

/// Parse `talaria.huge=<MB>` from the command line.
pub fn huge_from_cmdline(cmdline: &str) {
    if let Some(mb) = crate::cmdline::num(cmdline, "talaria.huge") { huge_set_mb(mb); }
}

fn huge_ok(pages_left: u64) -> bool {
    // There is exactly one condition: the piece covers an aligned 2 MB entirely — and there is
    // plenty of memory (>= 512 MB free), because a huge page is materialised
    // ENTIRELY, and on a sparsely used mapping that is an overspend.
    //
    // Previously there was a threshold of 32 MB on THE WHOLE mapping here, and it extinguished huge
    // pages completely: the runtimes reserve address space in a large
    // piece, while they commit the memory in small ones (HotSpot — an mmap MAP_FIXED over
    // its own reservation). The counters showed that directly: 1211 reservations of 2 MB, zero
    // pages handed out. The conclusion "huge pages give no gain" was made on a
    // measurement where they were never handed out.
    pages_left >= HUGE_MIN_PAGES.load(core::sync::atomic::Ordering::Relaxed)
        && crate::mem::free_total_frames() > 512 * 256
}

/// Reserve [vaddr, vaddr+pages*4K) WITHOUT frames: the entries are not present, they are marked
/// LAZY (+ the future rights). The first access materialises the page (`lazy_fault`).
/// `none` — PROT_NONE: an access will stay a fault until an mprotect.
pub fn map_lazy_pages(vaddr: u64, pages: u64, writable: bool, exec: bool, none: bool) -> bool {
    let cr3 = current_cr3() & ADDR_MASK;
    unmap_proc(vaddr, pages); // a MAP_FIXED over a previous one — release the frames
    // SAFETY: P1 + P2 + P3 — a reservation without frames (P=0, LAZY) in the current AS; the frames
    // appear later, in `lazy_fault`.
    unsafe {
        let mut e = PTE_LAZY | PTE_PRIV;
        if writable { e |= LAZY_W; }
        if exec { e |= LAZY_X; }
        if none { e |= PTE_PNONE; }
        let end = vaddr + pages * 4096;
        let mut a = vaddr;
        while a < end {
            // An aligned piece of 2 MB goes as ONE PD entry (and one TLB page
            // for all the 512 pages when it is materialised).
            if a & 0x1F_FFFF == 0 && end - a >= 0x20_0000 {
                // An aligned 2 MB goes as ONE directory entry. As a huge page
                // (one TLB line per 2 MB, materialisation in its entirety) — only
                // if that is justified by the memory; otherwise as the same reservation but with
                // the ordinary granularity: it will unfold into a table at the first
                // touch. In both cases the entry is one instead of five hundred and twelve.
                let huge = huge_ok((end - a) / 4096);
                pte_w(pde_ptr_in(cr3, a), e | PTE_PS | if huge { 0 } else { LAZY_4K });
                if huge { HUGE_RESERVED.fetch_add(1, core::sync::atomic::Ordering::Relaxed); }
                a += 0x20_0000;
                continue;
            }
            // The descent over the levels is one per 2 MB, after that go 512 neighbouring entries.
            let stop = chunk_end(a, end);
            match pt_for(cr3, a, true) {
                Some(pt) => while a < stop { pte_w(pt.add(pt_idx(a)), e); a += 4096; },
                None => return false, // there is nothing to build a table from — an honest ENOMEM
            }
        }
        flush_tlb();
    }
    true
}

/// A TLB shootdown: make THE OTHER cores reload CR3. It is called after the removal
/// or the tightening of mappings (munmap/mprotect/MADV_DONTNEED). The installation of a NEW
/// present mapping does not require a shootdown: x86 does not cache absent entries.
/// The TLB flush mailbox: the initiator raises the counter of REQUESTS to a neighbour and waits
/// until that one raises the counter of the FULFILLED ones. The interrupt here is only a "push";
/// the agreement itself lives in these counters. It is done that way because the syscalls
/// go with IF=0 (FMASK extinguishes IF): two cores simultaneously removing mappings
/// would wait for an interrupt from each other and would jam for good. With the mailbox
/// a waiter SERVES the foreign requests itself, without enabling the interrupts.
#[cfg(target_arch = "x86_64")]
static TLB_REQ: [core::sync::atomic::AtomicU64; 64] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 64];
#[cfg(target_arch = "x86_64")]
static TLB_DONE: [core::sync::atomic::AtomicU64; 64] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 64];

/// How many times a wait for an acknowledgement ran into the time limit (the diagnostics:
/// normally it must be zero).
pub static SHOOT_TIMEOUT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// THE PRICE OF WAITING FOR AN ACKNOWLEDGEMENT — by time rather than only by the number of refusals.
///
/// The counter of refusals answers only the question "did it come to the limit of 10 ms".
/// It does not answer the one that decides: how much AN ORDINARY wait COSTS. And
/// it costs exactly as much as the neighbour does not look into the mailbox, and
/// this price falls inside `munmap`/`mprotect` — that is, straight into the request's
/// latency. So here: how many waits there were, how many cycles in total,
/// the worst one and the breakdown by thresholds. The steps are chosen so that they can be
/// applied to the workload's percentiles: 10 us — "unnoticeable", 100 us — "already
/// visible in a database's p99", 1 ms — "visible with the naked eye".
/// How many calls were not made because a neighbour is in A FOREIGN address space.
pub static SHOOT_SKIPPED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static SHOOT_WAITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static SHOOT_WAIT_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static SHOOT_MAX_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The waits longer than 10 us / 100 us / 1 ms.
pub static SHOOT_OVER: [core::sync::atomic::AtomicU64; 3] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 3];

/// The steps' thresholds IN CYCLES, computed once. Dividing by the frequency right in
/// the accounting is not allowed: mongod's `munmap` happens a quarter of a million times per run, and
/// a division on the hot path would change exactly what this counter measures.
#[cfg(target_arch = "x86_64")]
static SHOOT_STEP_TSC: [core::sync::atomic::AtomicU64; 3] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 3];

/// Account for one wait for an acknowledgement of a length of `cyc` cycles.
#[cfg(target_arch = "x86_64")]
fn shoot_account(cyc: u64) {
    use core::sync::atomic::Ordering::Relaxed;
    SHOOT_WAITS.fetch_add(1, Relaxed);
    SHOOT_WAIT_TSC.fetch_add(cyc, Relaxed);
    if cyc > SHOOT_MAX_TSC.load(Relaxed) { SHOOT_MAX_TSC.store(cyc, Relaxed); }
    if SHOOT_STEP_TSC[0].load(Relaxed) == 0 {
        let hz = crate::tsc::calibrate_cached().max(1);
        for (i, us) in [10u64, 100, 1000].iter().enumerate() {
            SHOOT_STEP_TSC[i].store(hz.saturating_mul(*us) / 1_000_000, Relaxed);
        }
    }
    for i in 0..3 {
        if cyc >= SHOOT_STEP_TSC[i].load(Relaxed) { SHOOT_OVER[i].fetch_add(1, Relaxed); }
    }
}
/// How many flushes were made at the neighbours' request (the diagnostics).
pub static TLB_SERVED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// A CORE STANDS IN `hlt` — THERE IS NO NEED to wait for an acknowledgement from it.
///
/// The flag is raised exactly in the window before `sti; hlt` (there IF=0) and is removed right
/// after. Hence the property on which everything rests: with a core that we see
/// stopped, our call is ALREADY hanging in the IRR (we sent it before we
/// looked at the flag), and it will take it at the nearest `sti` — that is, before
/// it executes a single instruction of its own. The acknowledgement will come; there is nothing to
/// wait for.
///
/// What for: on Nitro PV IPI is not declared (`smp: KVM PV IPI is not declared` in the stand's
/// console), the call goes by a write into the ICR, and waking a stopped neighbour by that
/// path does not fit into the wait's ceiling — over a `mongodb` run on 8 September
/// "did not acknowledge within 10 ms" was printed 440 times, and every one of them is ten
/// milliseconds that the initiator of a `munmap` stood in vain (and having given up, it
/// relied on the same hanging call all the same, only silently). Locally, where PV
/// IPI exists, the same sleep is acknowledged in 1057 us over 29 403 flushes — that is,
/// it is not about our wait loops but about the delivery of the call.
static HALTED: [core::sync::atomic::AtomicBool; 64] =
    [const { core::sync::atomic::AtomicBool::new(false) }; 64];
/// How many times a wait for an acknowledgement was stopped by the sign "the neighbour is in hlt".
pub static SHOOT_HALTED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// WE DO NOT CALL A STOPPED NEIGHBOUR AT ALL — it will flush the TLB itself when it wakes up.
///
/// Previously the call went to everybody, while by the `HALTED` sign only
/// THE WAIT was stopped — and that after 20 us (see `long_wait`). The mongodb measurement (session 28,
/// `diag28m2`): 215 thou. flushes, the acknowledgements 6.2 us per flush, while
/// with a neighbour in `hlt` the sign "it stands" fired 123 times — that is, almost every
/// call WOKE a sleeping core (on Nitro that is an exit to the host on both ends),
/// and the initiator of a `madvise`/`munmap` waited for exactly that wakeup. At a low
/// rate, where the neighbours sleep almost always, that is 2-3 flushes per request.
///
/// Now: the initiator sets the mark "the TLB is stale" on the neighbour and ONLY THEN looks at
/// `HALTED`. The neighbour, coming out of `hlt`, first removes `HALTED` and ONLY THEN
/// reads the mark. Both orders are SeqCst, so at least one side will see
/// the other: either the initiator sees "it does not stand" and calls as before, or the neighbour
/// sees the mark and extinguishes the TLB before its first instruction outside the kernel.
///
/// Where else a stopped core may go, bypassing `halt_exit`: a tick (or
/// a resched IPI) straight from the interrupt switches it to another task, and if
/// that one is in THE SAME address space, CR3 is not reloaded. So the mark
/// is checked by the scheduler too, at the point of the decision about CR3 (`lazy_tlb_take` in
/// `sched::switch`): with the mark CR3 is loaded even with the same AS. The interrupt
/// handlers do not touch user addresses (a wakeup call is only an
/// EOI, a tick is only the scheduler's structures), so the window between the wakeup
/// and either of these two points is safe.
static TLB_LAZY: [core::sync::atomic::AtomicBool; 64] =
    [const { core::sync::atomic::AtomicBool::new(false) }; 64];
/// How many calls were not made because the neighbour stood in `hlt` (it will flush by itself).
pub static SHOOT_LAZY: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many times a core, having woken up, extinguished the TLB by the mark.
pub static LAZY_FLUSHES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// `talaria.lazytlb=0` — call the stopped neighbours as before (an A/B).
pub static LAZY_TLB_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
pub fn lazytlb_from_cmdline(cmdline: &str) {
    if let Some(v) = crate::cmdline::value(cmdline, "talaria.lazytlb=") {
        let on = v.trim() != "0";
        LAZY_TLB_ON.store(on, core::sync::atomic::Ordering::SeqCst);
        crate::println!("talaria: lazytlb={} ({})", if on { 1 } else { 0 },
            if on { "we do not call a stopped neighbour" } else { "we call everybody, as before" });
    }
}
/// For the scheduler: is there a mark "the TLB is stale" on this core. It removes it; call it
/// ONLY where CR3 is really reloaded next.
#[inline]
pub fn lazy_tlb_take() -> bool {
    let me = crate::percpu::cpu_id();
    if me < 64 && TLB_LAZY[me].swap(false, core::sync::atomic::Ordering::SeqCst) {
        LAZY_FLUSHES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        true
    } else { false }
}
/// Mark this core as stopped (call it RIGHT NEXT TO `sti; hlt`).
#[inline]
pub fn halt_enter() {
    let me = crate::percpu::cpu_id();
    if me < 64 { HALTED[me].store(true, core::sync::atomic::Ordering::SeqCst); }
    crate::sched::halt_acct_enter(me); // the sleep's time belongs neither to the task nor to the core
    crate::umark::halt_in(me);         // a slow interval's instrument: the core's sleeps (off until a mark)
}
/// Remove the mark (right after the `hlt`) — and extinguish the TLB if during the sleep
/// a neighbour removed mappings without calling (see `TLB_LAZY`). The order "remove
/// HALTED, then read the mark" is part of the protocol, it must not be changed.
#[inline]
pub fn halt_exit() {
    let me = crate::percpu::cpu_id();
    if me < 64 {
        HALTED[me].store(false, core::sync::atomic::Ordering::SeqCst);
        // SAFETY: P5 — a core that has just come out of hlt takes the TLB flush it deferred while
        // it was asleep (`lazy_tlb`).
        if lazy_tlb_take() { unsafe { flush_tlb(); } }
    }
    crate::sched::halt_acct_exit(me);
    crate::umark::halt_out(me);
}
/// Does the core `cpu` stand in `hlt`.
#[inline]
pub fn is_halted(cpu: usize) -> bool {
    cpu < 64 && HALTED[cpu].load(core::sync::atomic::Ordering::SeqCst)
}

/// Fulfil the accumulated requests for a TLB flush on THIS core.
/// It is called from the handler of the vector 0x42 and from the wait in `shootdown`.
#[cfg(target_arch = "x86_64")]
#[unsafe(no_mangle)]
pub extern "C" fn tlb_service() {
    use core::sync::atomic::Ordering;
    let me = crate::percpu::cpu_id();
    if me >= 64 { return; }
    // The order matters: first we LEARN the request's number, then we flush, then we
    // report. A request that arrived between the reading and the flush will be
    // fulfilled by this same flush too, but it will not get into the report — its author will wait for
    // the next round. The error is on the safe side.
    let r = TLB_REQ[me].load(Ordering::SeqCst);
    if TLB_DONE[me].load(Ordering::SeqCst) >= r { return; }
    // WHOSE range to extinguish. Previously it was ONE PER MACHINE, and that is why it had
    // to be kept under a common lock — while the lock serialised the flushes of the whole
    // machine (see `shootdown_range`). Now every initiator has its own slot, while
    // here the mask of those who asked THIS core is taken off.
    //
    // The order matters here too and rests on the fact that the initiator sets its bit BEFORE
    // it raises the counter of requests: which means that everybody who brought the counter up to `r` is already
    // in the mask taken off after the reading of `r`. A request that arrived later will get into the
    // mask (an extra flush is safe), but it will be acknowledged by the next round.
    let mask = TLB_PEND[me].swap(0, Ordering::SeqCst);
    // SAFETY: P4 + P5 — the shootdown's acknowledgement path — this core flushes the ranges the
    // initiators asked for; the mask has already been taken atomically above.
    unsafe {
        // One initiator with a short range — pointwise; otherwise it is cheaper and
        // safer to extinguish the TLB entirely than to walk over several ranges.
        if mask.count_ones() == 1 {
            let i = mask.trailing_zeros() as usize;
            let pages = SHOOT_PAGES[i].load(Ordering::SeqCst);
            if pages == 0 || pages > INVLPG_MAX { flush_tlb(); }
            else { flush_range(SHOOT_START[i].load(Ordering::SeqCst), pages); }
        } else { flush_tlb(); }
    }
    TLB_DONE[me].store(r, Ordering::SeqCst);
    TLB_SERVED.fetch_add(1, Ordering::Relaxed);
}

/// arm64: no mailbox to serve — a neighbour's `TLBI ...IS` already reached this core.
#[cfg(target_arch = "aarch64")]
#[unsafe(no_mangle)]
pub extern "C" fn tlb_service() {}

/// The flush's range is ITS OWN FOR EVERY INITIATOR (see `tlb_service`).
///
/// It was one per machine, and that is why it had to be kept under a common lock, while the
/// lock was held for the whole time of collecting the acknowledgements. That serialised the TLB flushes
/// OF THE WHOLE MACHINE: with PostgreSQL, which does a `munmap` per request on
/// every core, out of 9373 ns of the call 8102 went into `shootdown_range`, while
/// the waiting for the acknowledgements itself is 0.5 us. The rest was waiting for the lock.
/// There is nobody to write its own slot except its owner, and it lives exactly as long as
/// the owner waits for the acknowledgements — a lock is not needed at all.
#[cfg(target_arch = "x86_64")]
static SHOOT_START: [core::sync::atomic::AtomicU64; 64] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 64];
#[cfg(target_arch = "x86_64")]
static SHOOT_PAGES: [core::sync::atomic::AtomicU64; 64] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 64];
/// Who asked THIS core to flush (a bit mask of the initiators).
#[cfg(target_arch = "x86_64")]
static TLB_PEND: [core::sync::atomic::AtomicU64; 64] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 64];


/// Flush the mappings [start, start+pages*4K) at the neighbours. `pages == 0` — everything.
/// The flush of the mappings of A USER address space: one must call not
/// everybody but only those who are executing that space right now.
///
/// We do not use PCID, so a `mov cr3` extinguishes the TLB entirely: a core that has gone into
/// ANOTHER address space no longer holds our entries, and there is no point in
/// calling it.
///
/// WHY THIS IS SAFE — by all three windows rather than "it is obvious anyway":
///
/// * a neighbour will ENTER our space after the check: the entry is a `mov cr3`,
///   that is, a full flush; it will start with an empty TLB;
/// * a neighbour SITS idle (hlt/mwait): the last thing it loaded was the CR3 of its task,
///   and if that one is not ours — it has none of our entries; if it is ours — the check sees that
///   and the call goes out;
/// * a neighbour is IN THE MIDDLE of a switch: `set_cur_task` runs ahead of the CR3 load itself
///   (it is done by the stub already on the target's kernel stack), so we may see the new
///   task with the CR3 still old and skip the call. The window is harmless: between these
///   two points the core executes only the switching code on the common kernel stack
///   and does NOT dereference the old space's user addresses, while the stub's very
///   first action loads CR3 and extinguishes the TLB. There are no interrupts in the window — the
///   scheduler's path goes with IF=0.
///
/// A task without its own space (a thread, `cr3_of == 0`) is interpreted in favour of
/// a call: uncertainty here must cost an extra IPI rather than the integrity of the memory.
///
/// Why this became necessary. `sysprof` on PostgreSQL: `munmap` 9373 ns per call,
/// of them 7955 inside `shootdown_range`, while the waiting for the acknowledgements is
/// 0.5 us, and the common lock (removed separately) cost 150 ns. What remains is THE SENDING of the IPI:
/// under a hypervisor that is an exit to the host. And PostgreSQL is a process per connection, and
/// a neighbouring core is almost always busy with ANOTHER backend, that is, with another AS: the call
/// was not needed a single time.
pub fn shootdown_range_user(start: u64, pages: u64) {
    use core::sync::atomic::Ordering;
    let my = current_cr3() & ADDR_MASK;
    shootdown_impl(start, pages, Some(my));
    let _ = Ordering::Relaxed;
}

pub fn shootdown_range(start: u64, pages: u64) { shootdown_impl(start, pages, None) }

/// The flush by address: above 4 GB there lie ONLY a process's private areas (the heap
/// `PROC_HEAP_BASE`, `MMAP_BASE`), their tables are their own in every AS, and one must call
/// only the cores executing that AS; below 4 GB it is identity, common to all, and
/// the call must go to everybody.
///
/// What for: the filter by AS stood ONLY on `munmap`, while `mprotect`, `madvise` and
/// `mremap` called everybody. PostgreSQL is a process per connection: every `madvise`
/// of one backend stopped the other three cores on an IPI (an exit to the host under a
/// hypervisor), although they could not have had any of our entries.
pub fn shootdown_range_auto(start: u64, pages: u64) {
    if start >= 4u64 << 30 { shootdown_range_user(start, pages) } else { shootdown_range(start, pages) }
}

/// How many times the acknowledgements were not waited for lawfully: the neighbour was TAKEN OFF THE CORE
/// by the host, and the flush of its TLB is entrusted to the host (see `smp::ask_host_flush`).
/// Telling this from `SHOOT_TIMEOUT` is obligatory: the first is the norm of a stand with
/// the reshuffling of vCPUs, the second is a lost acknowledgement.
pub static SHOOT_HOSTFLUSH: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

#[cfg(target_arch = "x86_64")]
fn shootdown_impl(start: u64, pages: u64, only_cr3: Option<u64>) {
    use core::sync::atomic::Ordering;
    if crate::syscall::NO_SHOOTDOWN.load(Ordering::Relaxed) { return; } // talaria.noshoot=1 — only for the diagnostics
    let online = crate::percpu::ONLINE.load(Ordering::SeqCst);
    if online & (online - 1) == 0 { return; } // one core — there is nobody to send to
    let me = crate::percpu::cpu_id();
    // SAFETY: P5 — a core outside the mask's width (>= 64) cannot be addressed by the mask, so it
    // extinguishes its whole TLB.
    if me >= 64 { unsafe { flush_tlb() }; return; }
    // Its own slot — its own range. The bit in the target's mask is set BEFORE the counter of
    // requests is raised: the one serving takes the mask off already after reading the counter, and that is why
    // it sees everybody who brought the counter up to the value it read.
    SHOOT_START[me].store(start, Ordering::SeqCst);
    SHOOT_PAGES[me].store(pages, Ordering::SeqCst);
    let mut want = [0u64; 64];
    let mut any = false;
    for cpu in 0..64usize {
        if online & (1 << cpu) == 0 || cpu == me { continue; }
        // Who is on this core and in which address space it is. Uncertainty
        // is interpreted IN FAVOUR OF A CALL: if we do not know, we call.
        if let Some(my) = only_cr3 {
            let t = crate::percpu::cur_task_of(cpu);
            if t != u64::MAX && (t as usize) < crate::sched::NTASKS {
                let their = crate::sched::cr3_of(t as usize) & ADDR_MASK;
                if their != 0 && their != my { SHOOT_SKIPPED.fetch_add(1, Ordering::Relaxed); continue; }
            }
        }
        // The neighbour is in `hlt` — we neither call nor wait: the mark, then the check (see `TLB_LAZY`).
        if LAZY_TLB_ON.load(Ordering::Relaxed) {
            TLB_LAZY[cpu].store(true, Ordering::SeqCst);
            if is_halted(cpu) { SHOOT_LAZY.fetch_add(1, Ordering::Relaxed); continue; }
        }
        TLB_PEND[cpu].fetch_or(1u64 << me, Ordering::SeqCst);
        want[cpu] = TLB_REQ[cpu].fetch_add(1, Ordering::SeqCst) + 1;
        any = true;
        crate::smp::send_ipi(crate::smp::apic_of(cpu), crate::idt::TLB_VECTOR as u8);
    }
    if !any { return; }
    // WE WAIT. Without the wait mprotect/munmap return while a neighbouring core is still
    // walking over the removed mapping through a stale TLB entry — that is, writing
    // into a frame that the allocator has already given to somebody else. And that is what looks like
    // "random" memory corruption in a large multi-threaded workload after minutes.
    let hz = crate::tsc::hz();
    let limit = if hz != 0 { hz / 100 } else { 30_000_000 }; // 10 ms — only an insurance
    let long_wait = if hz != 0 { hz / 50_000 } else { 60_000 }; // 20 us — "the wait is already abnormal"
    let t_start = crate::tsc::rdtsc();
    loop {
        let mut left = false;
        for cpu in 0..64usize {
            if want[cpu] != 0 && TLB_DONE[cpu].load(Ordering::SeqCst) < want[cpu] { left = true; break; }
        }
        if !left {
            shoot_account(crate::tsc::rdtsc().wrapping_sub(t_start));
            return;
        }
        tlb_service(); // a neighbour could have asked us — we serve it, otherwise a mutual jam
        core::hint::spin_loop();
        // THE NEIGHBOUR MIGHT NOT HAVE BEEN ON THE PROCESSOR. Then waiting for its acknowledgement
        // means waiting until the host gives it a core back: the measurement gave exactly that — 17
        // waits of the FULL 10 ms over a mysql run, and a request's p99.99 of 18.5 ms
        // against Linux's 0.57. We ask the host to flush such a neighbour's TLB at its
        // return (KVM_VCPU_FLUSH_TLB) and stop waiting for it: that is correct —
        // the flush will happen anyway BEFORE it touches the memory again.
        //
        // We ask NOT on every turn: an ordinary acknowledgement comes within
        // single microseconds, and walking 64 slots on every turn would cost
        // more than the wait itself. The threshold of 20 us means "this wait is already
        // abnormal".
        if crate::tsc::rdtsc().wrapping_sub(t_start) > long_wait {
            for cpu in 0..64usize {
                if want[cpu] == 0 || TLB_DONE[cpu].load(Ordering::SeqCst) >= want[cpu] { continue; }
                // THE NEIGHBOUR STANDS IN `hlt` — there is nothing to wait for (see `HALTED`): the call is already
                // hanging in its IRR and will be taken before any instruction of its own.
                if is_halted(cpu) {
                    want[cpu] = 0;
                    SHOOT_HALTED.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if crate::smp::steal_on() && crate::smp::vcpu_preempted(cpu) && crate::smp::ask_host_flush(cpu) {
                    want[cpu] = 0;
                    SHOOT_HOSTFLUSH.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if crate::tsc::rdtsc().wrapping_sub(t_start) > limit {
            shoot_account(crate::tsc::rdtsc().wrapping_sub(t_start));
            SHOOT_TIMEOUT.fetch_add(1, Ordering::Relaxed);
            // WHO STAYED SILENT — and what it was doing meanwhile. Without that "N did not
            // acknowledge" is a counter without an address: the gate caught it once in three runs, while
            // the culprit was guessed from the code.
            for cpu in 0..64usize {
                if want[cpu] == 0 || TLB_DONE[cpu].load(Ordering::SeqCst) >= want[cpu] { continue; }
                let t = crate::percpu::cur_task_of(cpu);
                let sys = if t != u64::MAX && (t as usize) < crate::sched::NTASKS {
                    let v = crate::syscall::LAST_SYS[t as usize].load(Ordering::Relaxed);
                    if v == u64::MAX { -1 } else { (v >> 48) as i64 }
                } else { -2 };
                crate::println!("shootdown: core {} did not acknowledge within 10 ms (the task {}, the last syscall {}, taken off by the host {}, in hlt {})",
                    cpu, t as i64, sys, crate::smp::vcpu_preempted(cpu), is_halted(cpu));
            }
            return;
        }
    }
}

/// arm64: there is no shootdown to organise. `TLBI ...IS` is broadcast to every core of
/// the inner-shareable domain by the hardware (docs/ARM64-PLAN.md §4.3), so the range is
/// simply invalidated here and the other cores see it without an IPI, a mailbox or a
/// wait. `only_cr3` (the x86 optimisation "only the cores in this space") has nothing
/// to optimise.
#[cfg(target_arch = "aarch64")]
fn shootdown_impl(start: u64, pages: u64, _only_cr3: Option<u64>) {
    // SAFETY: P4 — the entries were changed before the call; this makes every core forget them.
    // A large range or "everything" (pages == 0) is the broadcast full flush: `flush_range`
    // would take the LOCAL one for it (and nothing at all for 0).
    unsafe {
        if pages == 0 || pages > INVLPG_MAX { mmu::flush_all_is(); } else { flush_range(start, pages); }
    }
}

/// MOVE A MAPPING WITHOUT COPYING THE DATA (`mremap`).
///
/// Why this became necessary at all. Our `mremap` returned ENOMEM, and `realloc`
/// in libc degenerated into "allocate a new one — copy — free the old one". For
/// a growing buffer that is not only an extra copy: EVERY such freeing is a
/// `munmap`, and a `munmap` is a TLB flush on ALL the neighbouring cores. The measurement on
/// ClickHouse (a read of a table of 10 million rows) named the price directly:
///
///   1 thread:  unmap 134 calls, 674 thou. pages, 488 TLB flushes, we waited 17 ms
///   4 threads: unmap 1906 calls, 15.2 MILLION pages, 4090 flushes, we waited 285 ms
///
/// That is, with four threads we extinguished the whole machine's TLB fourteen times as often
/// — and it was exactly on that that we lost the scaling: a pure multi-threaded computation
/// (`numbers_mt`, without files) scales for us 3.59x against Linux's 3.84x,
/// while a read of a table is 2.4x against 3.4x.
///
/// Here THE TABLES' ENTRIES are moved rather than the bytes: the frames stay the same, there is no copy
/// at all. An aligned 2 MB is moved as ONE directory entry — otherwise the move
/// would exchange huge pages back for four-kilobyte ones and would eat up the
/// gain for whose sake they are switched on.
///
/// It returns false if there is nothing to move or the target is occupied.
pub fn move_mapping(old: u64, new: u64, pages: u64) -> bool {
    if pages == 0 || old == new { return false; }
    let cr3 = current_cr3() & ADDR_MASK;
    // SAFETY: P1 + P2 + P3 + P4 — `mremap` moves the ENTRIES rather than the bytes — both ranges
    // are walked in the current AS, and the source is cleared only after the destination
    // is written.
    unsafe {
        let mut i = 0u64;
        let mut moved = 0u64;
        while i < pages {
            let o = old + i * 4096;
            let n = new + i * 4096;
            // A large piece goes as one directory entry if both ends are aligned,
            // the remainder is not less than 2 MB, the source has a 2M leaf (a ready or a lazy one),
            // and the target has nothing at that address yet.
            if o & 0x1F_FFFF == 0 && n & 0x1F_FFFF == 0 && pages - i >= 512 {
                let po = pde_ptr_in(cr3, o);
                let e = pte_r(po);
                if e & PTE_PS != 0 {
                    let pn = pde_ptr_in(cr3, n);
                    if pte_r(pn) == 0 {
                        pte_w(pn, e);
                        pte_w(po, 0);
                        moved += 512;
                        i += 512;
                        continue;
                    }
                }
            }
            let po = match pte_ptr(o) { Some(p) => p, None => { i += 1; continue } };
            let e = pte_r(po);
            if e != 0 {
                let pn = pte_ptr_in(cr3, n);
                pte_w(pn, e);
                pte_w(po, 0);
                moved += 1;
            }
            i += 1;
        }
        if moved == 0 { return false; }
        flush_tlb();
    }
    // The old range has stopped being mapped — the neighbours could have kept
    // TLB entries for it, and that is exactly the case for whose sake the flush was introduced.
    shootdown_range_auto(old, pages);
    true
}

/// Is the range free in THE CURRENT space (not a single entry — neither a ready
/// nor a lazy one). It is needed by `mremap` in order to understand whether it can grow in place.
pub fn range_free(vaddr: u64, pages: u64) -> bool {
    // SAFETY: P1 + P2 — a read-only walk over the range.
    unsafe {
        let mut a = vaddr;
        for _ in 0..pages {
            let e = walk_entry(a);
            if e & (PTE_P | PTE_LAZY) != 0 { return false; }
            a += 4096;
        }
    }
    true
}

/// The rights of the last mapped page of the range (writable, exec) — so that
/// the grown tail gets the same as the mapping itself.
pub fn range_rights(vaddr: u64, pages: u64) -> (bool, bool) {
    // SAFETY: P1 + P2 — a read-only walk over the range.
    unsafe {
        let mut a = vaddr + pages.saturating_sub(1) * 4096;
        for _ in 0..pages.min(64) {
            let e = walk_entry(a);
            if e & (PTE_P | PTE_LAZY) != 0 {
                let w = e & PTE_RW != 0 || e & LAZY_W != 0;
                let x = if e & PTE_LAZY != 0 { e & LAZY_X != 0 } else { e & PTE_NX == 0 };
                return (w, x);
            }
            if a < 4096 { break; }
            a -= 4096;
        }
    }
    (true, false)
}

/// May THE KERNEL write into [vaddr, vaddr+len) in the current address space.
/// The LAZY pages on the way are materialised (that is a lawful first touch), while
/// the PROT_NONE/unmapped ones honestly give false.
///
/// It is needed where the kernel writes into the workload's memory BY ITS OWN INSTRUCTION and has no right
/// to fall: a signal's frame on a thread's stack. A ring-0 #PF on such a write is not "a bad
/// program" but a stop of the whole machine (it was caught exactly like that: the JVM overflows its stack,
/// the kernel builds a SIGSEGV frame in its guard page and falls itself).
pub fn user_writable(vaddr: u64, len: u64) -> bool {
    if len == 0 { return true; }
    let mut a = vaddr & !0xFFF;
    let end = vaddr + len;
    while a < end {
        // SAFETY: P1 + P2 — a read-only walk.
        let e = unsafe { walk_entry(a) };
        if e & PTE_LAZY != 0 && e & PTE_PNONE == 0 { if !lazy_fault(a) { return false; } }
        // A COW page (after a fork: present, US, but RW removed) MAY be written — the kernel
        // will untie the copy at the very first access. We untie it right here so that
        // the answer is honest: otherwise a signal's frame was not built in a fork child's stack
        // and a SIGUSR1 was simply not delivered to it.
        else if e & PTE_P != 0 && e & PTE_US != 0 && e & PTE_RW == 0 && e & PTE_COW != 0 {
            if !cow_fault(a) { return false; }
        }
        else if e & PTE_P == 0 || e & PTE_RW == 0 || e & PTE_US == 0 { return false; }
        a += 0x1000;
    }
    true
}

/// What kind of page is at the address: 0 — nothing (beyond the end of the mapping), 1 — a PROT_NONE
/// reservation (a guard, somebody made it so themselves), 2 — a lazy reservation, 3 — an ordinary one.
/// It is needed in order to tell "a thread ran into ITS OWN guard page" from "a thread went
/// beyond the mapping altogether" — those are different illnesses.
pub fn page_kind(vaddr: u64) -> u8 {
    // SAFETY: P1 + P2 — a read-only walk.
    let e = unsafe { walk_entry(vaddr & !0xFFF) };
    if e & PTE_P != 0 { return 3; }
    if e & PTE_PNONE != 0 { return 1; }
    if e & PTE_LAZY != 0 { return 2; }
    0
}

/// Is there at least one mapped/reserved page in [vaddr, +pages)
/// (present, a PROT_NONE reservation or LAZY). For MAP_FIXED_NOREPLACE.
pub fn any_mapped(vaddr: u64, pages: u64) -> bool {
    // SAFETY: P1 + P2 — a read-only walk over the range.
    unsafe {
        let mut a = vaddr & !0xFFF;
        for _ in 0..pages {
            let e = walk_entry(a);
            if e & (PTE_P | PTE_PNONE | PTE_LAZY) != 0 { return true; }
            a += 4096;
        }
    }
    false
}

/// MADV_DONTNEED: return the range's frames into the allocator and make the pages
/// LAZY again — the next access will give ZEROES (Linux's semantics for anonymous memory).
/// Frames whose last owner let go, given back to the allocator only AFTER the translations
/// to them are gone on every core. Freed in the walk, a frame could be handed out (and
/// zeroed, and written) by another core while a sibling thread's TLB — or, for a 2 MB leaf,
/// the entry itself — still pointed at it (both machines). Full -> the caller's flush of the
/// range, then the frees, and the walk goes on.
struct Deferred { f: [(u64, u64); 64], n: usize }
impl Deferred {
    const fn new() -> Self { Deferred { f: [(0, 0); 64], n: 0 } }
    #[track_caller]
    fn put(&mut self, phys: u64, cnt: u64, flush: &dyn Fn()) {
        if self.n == self.f.len() { flush(); self.free(); }
        self.f[self.n] = (phys, cnt); self.n += 1;
    }
    #[track_caller]
    fn free(&mut self) {
        for &(p, c) in &self.f[..self.n] { crate::mem::free_contig(p, c); }
        self.n = 0;
    }
}

/// The rights (LAZY_W/LAZY_X) are preserved from the former entry.
pub fn relazy_pages(vaddr: u64, pages: u64) {
    // SAFETY: P1 + P2 + P4 — the frames are returned to the allocator and the entries become lazy
    // reservations again; the range is flushed because the translations are gone.
    unsafe {
        let mut a = vaddr & !0xFFF;
        let end = a + pages * 4096;
        let mut touched = false;
        let first = vaddr & !0xFFF;
        let flush = || { flush_range_all(first, pages, shootdown_range_auto); };
        let mut dfr = Deferred::new();
        while a < end {
            if a & 0x1F_FFFF == 0 && end - a >= 0x20_0000 {
                if let Some(pde) = pde_ptr_existing(a) {   // a whole huge page
                    let e = pte_r(pde);
                    if e & PTE_PS != 0 && e & PTE_PRIV != 0 && e & PTE_P != 0 {
                        let phys = e & ADDR_MASK;
                        let mut ne = PTE_LAZY | PTE_PRIV | PTE_PS;
                        if e & PTE_RW != 0 { ne |= LAZY_W; }
                        if e & PTE_NX == 0 { ne |= LAZY_X; }
                        pte_w(pde, ne);
                        if crate::mem::rc_release(phys) { dfr.put(phys, 512, &flush); }
                        touched = true;
                        a += 0x20_0000;
                        continue;
                    }
                }
            }
            let stop = chunk_end(a, end);
            let pt = match pt_for(kpa_of(cr3_root()), a, false) { Some(p) => p, None => { a = stop; continue; } };
            while a < stop {
                let pte = pt.add(pt_idx(a));
                let e = pte_r(pte);
                a += 4096;
                if a & 0x3_FFFF == 0 { tlb_service(); } // the walk goes with IF=0 — once every 64 pages serve the neighbours' mailbox
                if e & PTE_LAZY != 0 { continue; }                 // it is already a reservation
                if e & (PTE_P | PTE_PNONE) == 0 || e & PTE_PRIV == 0 { continue; } // not our page
                let mut ne = PTE_LAZY | PTE_PRIV;
                if e & PTE_RW != 0 || e & PTE_COW != 0 { ne |= LAZY_W; }
                if e & PTE_NX == 0 { ne |= LAZY_X; }
                if e & PTE_PNONE != 0 { ne |= PTE_PNONE; }
                pte_w(pte, ne);
                if crate::mem::rc_release(e & ADDR_MASK) { dfr.put(e & ADDR_MASK, 1, &flush); }
                touched = true;
            }
        }
        if touched { flush(); }
        dfr.free();
    }
}

/// How many pages to materialise PER ONE fault (fault-around). One page per
/// fault is ~2-3 us for every 4 KB of memory touched for the first time: a ClickHouse query
/// that pumps 76 MiB would pay more with them than it computes. A cluster of
/// 64 KB removes 15 faults out of 16 and at the same time gives a contiguous piece of physical memory.
const LAZY_CLUSTER: u64 = 16;

/// Materialise a LAZY page (and the ones next to it — as a cluster): fresh
/// zeroed frames with the recorded rights. false — the page is not LAZY (which means the fault
/// is a real one) or there is no memory.
pub fn lazy_fault(vaddr: u64) -> bool {
    let a = vaddr & !0xFFF;
    // SAFETY: P1 + P2 + P3 + P4 — the fault path — a frame is allocated, zeroed through the
    // identity map and published in the entry; two cores faulting on the same page are
    // resolved by the CAS (P3), and the loser frees its frame.
    unsafe {
        // A HUGE PAGE. A lazy 2M reservation is materialised entirely: one
        // entry in the table and one TLB line per 2 MB instead of 512. Under a hypervisor
        // that is doubly important — a TLB miss drags a nested EPT walk along with it.
        if let Some(pde) = pde_ptr_existing(a) {
            let e = pte_r(pde);
            if e & PTE_P != 0 && e & PTE_PS != 0 { return true; }        // it has already been materialised by a neighbour
            if e & PTE_P == 0 && e & (PTE_LAZY | PTE_PS) == (PTE_LAZY | PTE_PS) {
                if e & PTE_PNONE != 0 { return false; }                  // PROT_NONE — the fault is a real one
                if e & LAZY_4K != 0 {
                    // A reservation of the ordinary granularity: unfold it into a table and go
                    // on along the 4K path (a cluster is materialised rather than 2 MB).
                    split_2m_lazy(pde);
                } else {
                let base = a & !0x1F_FFFF;
                match crate::mem::alloc_workload_aligned(512, 0x20_0000) {
                    Some(fr) => {
                        core::ptr::write_bytes(kptr::<u8>(fr), 0, 0x20_0000);
                        let mut ne = fr | PTE_P | PTE_US | PTE_PS | PTE_PRIV;
                        if e & LAZY_W != 0 { ne |= PTE_RW; }
                        if e & LAZY_X == 0 { ne |= PTE_NX; }
                                                if !pte_cas(pde, e, ne) {
                            crate::mem::free_contig(fr, 512); // a neighbouring core managed it earlier
                        } else {
                            HUGE_GIVEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                            mmu::flush_page(base);
                        }
                        return true;
                    }
                    // that many in a row and with the alignment were not found — exchange the reservation for 4K ones
                    None => { HUGE_NOMEM.fetch_add(1, core::sync::atomic::Ordering::Relaxed); split_2m_lazy(pde) }
                }
                }
            }
        }
        let pte = match pte_ptr(a) { Some(p) => p, None => return false };
        let e = pte_r(pte);
        // The page is already present: it was materialised by ANOTHER core while we were going into
        // the handler (or this is a stale TLB entry). The fault is not a real one — just
        // repeat the instruction. Without this branch a thread that lost the race got a
        // SIGSEGV on a perfectly normal page (the JVM fell in a memset over
        // fresh metaspace memory).
        if e & PTE_P != 0 { return true; }
        if e & PTE_LAZY == 0 || e & PTE_PNONE != 0 { return false; }
        // A RACE OF TWO CORES on one page: both threads see LAZY, both allocate a frame.
        // The one whose CAS went through wins; the loser MUST return its frame, otherwise
        // a page already filled by the neighbour would be replaced with zeroes (a corruption of the heap).
        // the frame must not coincide with a privately mapped identity vaddr of this AS
        // (the kernel writes into frames at their identity address — see map_fresh_pages)
        let node = crate::affinity::my_node();
        let mut fr = 0u64;
        // The frame must not coincide with a privately mapped identity vaddr of this AS
        // (the kernel writes into frames at their identity address). A frame that did not fit MUST
        // return into the allocator: previously it was simply lost — up to eight pages per
        // every such fault, and the longer the workload ran, the less
        // memory was left, the more often the refusal below happened. The leak fed
        // its own cause.
        let mut rejected: [u64; 8] = [0; 8];
        let mut nrej = 0usize;
        for _ in 0..8 {
            match crate::mem::alloc_contig_node(1, node) {
                Some(f) => {
                    // Above 4 GB too: the high area's gigabytes are identity leaves in every
                    // AS, and one a process has overlaid is not (`ident_leaf_overlap`).
                    if virt_to_phys(f) == f { fr = f; break; }
                    rejected[nrej] = f; nrej += 1;
                }
                None => break,
            }
        }
        for i in 0..nrej { crate::mem::free_contig(rejected[i], 1); }
        LAZY_REJECT.fetch_add(nrej as u64, core::sync::atomic::Ordering::Relaxed);
        if fr == 0 {
            // There IS a reservation at the address, while there is no frame. For the workload that is a SIGSEGV on
            // perfectly normal memory, and doing that silently is not allowed: from the outside
            // it is "the application fell on its own heap".
            LAZY_GIVEUP.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let left = LAZY_GIVEUP_TRACE.load(core::sync::atomic::Ordering::Relaxed);
            if left > 0 {
                LAZY_GIVEUP_TRACE.store(left - 1, core::sync::atomic::Ordering::Relaxed);
                crate::println!("[paging] the lazy reservation {:#x}: a frame was not allocated (rejected {}) — the workload will get a SIGSEGV on its own \
                    memory",
                    a, nrej);
            }
            return false;
        }
        core::ptr::write_bytes(kptr::<u8>(fr), 0, 4096);
        let mut ne = fr | PTE_P | PTE_US | PTE_PRIV;
        if e & LAZY_W != 0 { ne |= PTE_RW; }
        if e & LAZY_X == 0 { ne |= PTE_NX; }
                if !pte_cas(pte, e, ne) {
            crate::mem::free_contig(fr, 1); // we lost the race — the page has already been materialised by a neighbour
            return true;
        }
        mmu::flush_page(a);
        // The cluster: next come pages with THE SAME rights and untouched too —
        // we take them at once, without waiting for their own faults. We do not cross the 2 MB
        // boundary (further there is another page table — an extra walk).
        let end2m = (a | 0x1F_FFFF) + 1;
        let mut b = a + 4096;
        let mut left = LAZY_CLUSTER - 1;
        while left > 0 && b < end2m {
            let p2 = match pte_ptr(b) { Some(p) => p, None => break };
            let e2 = pte_r(p2);
            if e2 & PTE_P != 0 || e2 & PTE_LAZY == 0 || e2 & PTE_PNONE != 0 { break; }
            if e2 & (LAZY_W | LAZY_X) != e & (LAZY_W | LAZY_X) { break; } // different rights — a different mmap
            let f2 = match crate::mem::alloc_contig_node(1, node) { Some(f) => f, None => break };
            if virt_to_phys(f2) != f2 { crate::mem::free_contig(f2, 1); break; }
            core::ptr::write_bytes(kptr::<u8>(f2), 0, 4096);
            let mut n2 = f2 | PTE_P | PTE_US | PTE_PRIV;
            if e2 & LAZY_W != 0 { n2 |= PTE_RW; }
            if e2 & LAZY_X == 0 { n2 |= PTE_NX; }
                        if !pte_cas(p2, e2, n2) {
                crate::mem::free_contig(f2, 1); break; // the neighbour has already been materialised by somebody else
            }
            mmu::flush_page(b);
            b += 4096; left -= 1;
        }
    }
    true
}

/// Map the pages [vaddr, vaddr+pages*4K) in the active AS onto fresh zeroed frames (PRIV, RW, NX),
/// replacing the former mapping (MAP_FIXED). It returns false when there is not enough memory.
pub fn map_fresh_pages(vaddr: u64, pages: u64) -> bool {
    let cr3 = current_cr3();
    unmap_proc(vaddr, pages);
    // NUMA: a process's private mappings are malloc's arenas, that is, the very
    // memory the workload walks over. We take it from the process's node; 255 (a machine
    // with one node or no layout set) is exactly the former path.
    let node = crate::affinity::my_node();
    let mut a = vaddr;
    for _ in 0..pages {
        // the frame must not coincide with a privately mapped identity vaddr of this AS (a write through identity)
        let mut fr = 0u64;
        for _ in 0..8 { match crate::mem::alloc_contig_node(1, node) { Some(f) => { if virt_to_phys(f) == f { fr = f; break; } }
            None => return false } }
        if fr == 0 { return false; }
        // SAFETY: P1 — a frame straight from the allocator, zeroed through the identity map before
        // anybody can see it.
        unsafe { core::ptr::write_bytes(kptr::<u8>(fr), 0, 4096); }
        map_leaf(cr3, a, fr, true, true, false);
        a += 4096;
    }
    // SAFETY: P4 — the new entries are published, the TLB may hold the old (absent) ones.
    unsafe { flush_tlb(); }
    true
}

/// Make a page a guard page: remove present -> an access gives a clear #PF.
pub fn guard(vaddr: u64) {
    let a = vaddr & !0xFFF;
    // SAFETY: P1 + P2 + P3 + P4 — the page is made non-present on purpose (a stack guard), so the
    // entry must be flushed or a stale translation would keep it usable.
    unsafe {
        if let Some(pte) = pte_ptr(a) {
            // PTE_GUARD marks A DELIBERATE hole in the identity: it is skipped by
            // the check of the invariant (`ident_check`), all the other holes are a defect.
            let e = (pte_r(pte) & !PTE_P) | PTE_GUARD;
            pte_w(pte, e);
        }
        flush_tlb();
    }
}

/// `guard` in a GIVEN space (`sched::alloc_kstack_in`): the leaf is split in THAT space's
/// tables if it must be. The caller guarantees the space is not running elsewhere while its
/// directory may change (the kernel's reference one, or a fork's child not yet published).
pub fn guard_in(pml4_phys: u64, vaddr: u64) {
    let a = vaddr & !0xFFF;
    if pml4_phys == 0 { return; }
    // SAFETY: P1 + P2 + P3 + P4 — as `guard`, the entry in the space the caller names.
    unsafe {
        let pte = pte_ptr_in(pml4_phys, a);
        pte_w(pte, (pte_r(pte) & !PTE_P) | PTE_GUARD);
        flush_tlb();
    }
}

/// Is the page a guard in the space `pml4_phys` (the gate `kstackguard`) — a read-only walk.
pub fn is_guard_in(pml4_phys: u64, vaddr: u64) -> bool {
    let a = vaddr & !0xFFF;
    if pml4_phys == 0 { return false; }
    // SAFETY: P1 + P2 — a read-only descent.
    unsafe {
        let e4 = pte_r(kptr::<Pte>(pml4_phys).add(((a >> 39) & 0x1FF) as usize));
        if e4 & PTE_P == 0 { return false; }
        let e3 = pte_r(kptr::<Pte>(e4 & ADDR_MASK).add(((a >> 30) & 0x1FF) as usize));
        if e3 & PTE_P == 0 || e3 & PTE_PS != 0 { return false; }
        let e2 = pte_r(kptr::<Pte>(e3 & ADDR_MASK).add(((a >> 21) & 0x1FF) as usize));
        if e2 & PTE_P == 0 || e2 & PTE_PS != 0 { return false; }
        let e = pte_r(kptr::<Pte>(e2 & ADDR_MASK).add(((a >> 12) & 0x1FF) as usize));
        e & PTE_P == 0 && e & PTE_GUARD != 0
    }
}

/// Take a guard page back in the address space `pml4_phys` — a walk that creates nothing and
/// splits nothing: only a 4K entry that IS a guard (not present, `PTE_GUARD`) becomes the
/// kernel's identity leaf again. `sched::free_kstack` calls it for every address space when
/// a kernel stack is returned: `guard` makes the hole in ONE space (the one active when the
/// stack was allocated — a parent's, during its `fork`), and a stack freed without this left
/// that hole behind in it for good. The frame then went to another kernel stack, and the
/// kernel, running under the space with the hole, faulted on a read of that stack (the
/// scheduler reading the frame of the task it was about to enter — the arm64 `forkap` gate).
pub fn unguard_in(pml4_phys: u64, vaddr: u64) {
    let a = vaddr & !0xFFF;
    if pml4_phys == 0 { return; }
    // SAFETY: P1 + P2 — a read-only descent of a live space's tables (the entries above the
    // leaf are only read), and one store into the entry of a KERNEL-STACK page, which the
    // space's owner never writes: its `mprotect`/`munmap` work on its own ranges.
    unsafe {
        let e4 = pte_r(kptr::<Pte>(pml4_phys).add(((a >> 39) & 0x1FF) as usize));
        if e4 & PTE_P == 0 { return; }
        let e3 = pte_r(kptr::<Pte>(e4 & ADDR_MASK).add(((a >> 30) & 0x1FF) as usize));
        if e3 & PTE_P == 0 || e3 & PTE_PS != 0 { return; }
        let e2 = pte_r(kptr::<Pte>(e3 & ADDR_MASK).add(((a >> 21) & 0x1FF) as usize));
        if e2 & PTE_P == 0 || e2 & PTE_PS != 0 { return; }
        let pte = kptr::<Pte>(e2 & ADDR_MASK).add(((a >> 12) & 0x1FF) as usize);
        let e = pte_r(pte);
        if e & PTE_P == 0 && e & PTE_GUARD != 0 { pte_w(pte, ident_leaf(a)); }
    }
}

/// A `munmap` in identity mode (the kernel's AS): remove ring 3's access, PRESERVING the kernel's
/// identity mapping of the frame. Previously present was extinguished here — and a frame
/// that returned into the free list was inaccessible to the kernel until it was
/// "un-extinguished" by `unguard` at the next handout. That is the same mine as in
/// `unmap_proc`, only in the kernel AS itself: a freshly created process address
/// space COPIES the kernel's GB0 directory together with the holes.
pub fn unmap_ident(vaddr: u64, pages: u64) {
    let start = vaddr & !0xFFF;
    // SAFETY: P1 + P2 + P4 — the identity window of the current AS is closed page by page (the gate
    // `identhole` checks exactly this).
    unsafe {
        let mut a = start;
        for _ in 0..pages {
            if let Some(pte) = pte_ptr(a) { pte_w(pte, ident_leaf(a)); }
            a += 0x1000;
        }
        flush_range_all(start, pages, shootdown_range);
    }
}

/// THE STATE OF THE IDENTITY INVARIANT in the current address space.
///
/// It returns `(the holes, the substituted ones, the read-only ones)` in the window 0..4 GB:
/// * A HOLE — the page is not present while it is not marked with the guard mark: the kernel,
///   having touched that frame at its identity address, will get a #PF in ring 0 and kill the machine;
/// * A SUBSTITUTION — present, but it leads to ANOTHER frame: the kernel's write will go into a foreign
///   page silently (that is worse than a fault). The window of the workload's image (`mem::reserved`)
///   is excluded from the count — the frame allocator does not hand out from there by construction;
/// * READ-ONLY — present without RW: with CR0.WP the kernel's write will also give a #PF.
///
/// It is computed by a walk over the tables (a 2M leaf closes 512 pages with one check),
/// it is called by a gate and by `kstat` rather than by a hot path.
pub fn ident_check() -> (u64, u64, u64) {
    let (mut holes, mut shadow, mut ro) = (0u64, 0u64, 0u64);
    let (rb, re) = crate::mem::reserved();
    // under strict a process space has no identity over the map's RAM, nor over a gigabyte with
    // no RAM at all (its devices are in the window), by design: not a hole
    let in_proc = current_cr3() & ADDR_MASK != kernel_cr3() & ADDR_MASK;
    let dropped = |a: u64| in_proc && (strict_drops(a) || strict_gb_drops(a));
    let gb_holes = |g: u64| (0..512u64).filter(|m| !dropped(g + (m << 21))).count() as u64 * 512;
    // SAFETY: P1 + P2 — a read-only walk over the identity window.
    unsafe {
        let pml4 = cr3_root();
        // arm64's GB0 of devices has no identity at all by design (`init`: the devices' alias)
        let mut a = if cfg!(target_arch = "aarch64") { 1u64 << 30 } else { 0 };
        while a < IDENT_HI {
            let e4 = pte_r(pml4.add(((a >> 39) & 0x1FF) as usize));
            if e4 & PTE_P == 0 { holes += gb_holes(a); a += 1 << 30; continue; }
            let e3 = pte_r(kptr::<Pte>(e4 & ADDR_MASK).add(((a >> 30) & 0x1FF) as usize));
            if e3 & PTE_P == 0 { holes += gb_holes(a); a += 1 << 30; continue; }
            if e3 & PTE_PS != 0 { // a 1G leaf
                if e3 & ADDR_MASK != a { shadow += 512 * 512; } else if e3 & PTE_RW == 0 { ro += 512 * 512; }
                a += 1 << 30; continue;
            }
            let pd = kptr::<Pte>(e3 & ADDR_MASK);
            let e2 = pte_r(pd.add(((a >> 21) & 0x1FF) as usize));
            if e2 & PTE_PS != 0 || e2 & PTE_P == 0 {
                if e2 & PTE_P == 0 { if !dropped(a) { holes += 512; } }
                else if e2 & ADDR_MASK != a { shadow += 512; }
                else if e2 & PTE_RW == 0 { ro += 512; }
                a += 0x20_0000; continue;
            }
            let pt = kptr::<Pte>(e2 & ADDR_MASK);
            for l1 in 0..512usize {
                let va = a + (l1 as u64) * 4096;
                let e1 = pte_r(pt.add(l1));
                if e1 & PTE_GUARD != 0 { continue; }             // a guard page — that is how it is intended
                if e1 & PTE_P == 0 { if !dropped(va) { holes += 1; } continue; }
                if e1 & ADDR_MASK != va { if !(re > rb && va >= rb && va < re) { shadow += 1; } continue; }
                if e1 & PTE_RW == 0 { ro += 1; }
            }
            a += 0x20_0000;
        }
    }
    (holes, shadow, ro)
}

/// Remove the guard (return present) on a range WITHOUT allocations/splitting.
/// It is called when a frame from the free list is reused: a frame extinguished by a munmap guard
/// may be handed out for a page table/DMA by the kernel, and an access at its identity address
/// must not give a #PF. We do not touch the large leaves (2M/1G present) — they are already present.
/// Return present to the pages extinguished by guard()/munmap. IMPORTANT: the TLB flush happens only
/// if something was really changed. An unconditional flush_tlb() here cost a lot: unguard
/// is called from alloc_contig at EVERY allocation, while a reload of CR3 under KVM throws away
/// the caches of the page structures too. On the path "its own AS + 4K pages" (an mmap of 64 MB = 16384
/// calls of alloc_frame) that gave ~10 s for the allocation instead of ~100 ms.
pub fn unguard(vaddr: u64, len: u64) {
    if len == 0 { return; }
    let start = vaddr & !0xFFF;
    let end = (vaddr + len + 0xFFF) & !0xFFF;
    let mut a = start;
    let mut changed = false;
    // SAFETY: P1 + P2 + P3 + P4 — the reverse of `guard`.
    unsafe {
        while a < end {
            let l4 = ((a >> 39) & 0x1FF) as usize;
            let l3 = ((a >> 30) & 0x1FF) as usize;
            let l2 = ((a >> 21) & 0x1FF) as usize;
            let l1 = ((a >> 12) & 0x1FF) as usize;
            let e4 = cr3_root().add(l4);
            if pte_r(e4) & PTE_P != 0 {
                let pdpt = kptr::<Pte>(pte_r(e4) & ADDR_MASK);
                let e3 = pdpt.add(l3);
                if pte_r(e3) & PTE_P != 0 && pte_r(e3) & PTE_PS == 0 {
                    let pd = kptr::<Pte>(pte_r(e3) & ADDR_MASK);
                    let e2 = pd.add(l2);
                    if pte_r(e2) & PTE_P != 0 && pte_r(e2) & PTE_PS == 0 {
                        let pt = kptr::<Pte>(pte_r(e2) & ADDR_MASK);
                        let pte = pt.add(l1);
                        if pte_r(pte) & PTE_P == 0 && pte_r(pte) & PTE_LAZY == 0 && pte_r(pte) & ADDR_MASK != 0 {
                            // an extinguished 4K page: return present (RW for the kernel's needs);
                            // the guard mark is removed together with present — the frame has been reused
                            pte_w(pte, (pte_r(pte) | PTE_P | PTE_RW) & !(PTE_NX | PTE_GUARD));
                            changed = true;
                        }
                    }
                }
            }
            a += 0x1000;
        }
        if changed { flush_tlb(); }
    }
}

/// Mark a range NX, PRESERVING the 2M leaves where it is aligned (less TLB pressure
/// on a 128 MB heap and on mmap arenas); the partial 2M ones at the edges are split into 4K.
/// For the W^X of data that need not be executed: the heap/brk, an anonymous mmap.
pub fn nx_range(vaddr: u64, len: u64) {
    if len == 0 { return; }
    let mut a = vaddr & !0xFFF;
    let end = (vaddr + len + 0xFFF) & !0xFFF;
    // SAFETY: P1 + P2 + P3 + P4 — the NX bit is set over the range (W^X).
    unsafe {
        while a < end {
            let l4 = ((a >> 39) & 0x1FF) as usize;
            let l3 = ((a >> 30) & 0x1FF) as usize;
            let l2 = ((a >> 21) & 0x1FF) as usize;
            let pml4 = cr3_root();
            let e4 = pml4.add(l4);
            if pte_r(e4) & PTE_P == 0 { a += 0x1000; continue; }
            let pdpt = kptr::<Pte>(pte_r(e4) & ADDR_MASK);
            let e3 = pdpt.add(l3);
            if pte_r(e3) & PTE_P == 0 { a += 0x1000; continue; }
            if pte_r(e3) & PTE_PS != 0 { split_1g(e3); }
            let pd = kptr::<Pte>(pte_r(e3) & ADDR_MASK);
            let e2 = pd.add(l2);
            if pte_r(e2) & PTE_P == 0 { a += 0x1000; continue; }
            if pte_r(e2) & PTE_PS != 0 {
                if a & 0x1F_FFFF == 0 && end - a >= 0x20_0000 {
                    pte_w(e2, pte_r(e2) | PTE_NX); // NX on the whole 2M leaf
                    a += 0x20_0000; continue;
                }
                split_2m(e2); // a partial 2M — split it and set the rights line by line
            }
            let pt = kptr::<Pte>(pte_r(e2) & ADDR_MASK);
            let l1 = ((a >> 12) & 0x1FF) as usize;
            let pte = pt.add(l1);
            pte_w(pte, pte_r(pte) | PTE_NX);
            a += 0x1000;
        }
        flush_tlb();
    }
}

// ── per-process address spaces: its own PML4 + a CR3 switch ───────────────────
// The kernel part (the GB0 identity: the code/stack/data <32 MB, the RAM, the MMIO GB1-3) IS SHARED
// between all the spaces through a copy of the kernel PDPT; the private user
// pages are put into the PDPT slots unused by the kernel (the index >=4, i.e. vaddr>=4 GB),
// so a CR3 switch does not bring down the executing kernel.

/// The reference kernel PML4 (without the private user entries) — the source of
/// the shared kernel part when new ASes are built. It is set in init().
static KERNEL_PML4: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The image's high alias (stage 7.1): its virtual address minus the physical one, 0 before
/// `paging::init` built it (or on a machine without 1G pages, which keeps the boot tables).
static KIMG_ALIAS_OFF: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The image's high alias of image address `va`; None when there is no alias.
pub fn kimg_alias(va: u64) -> Option<u64> {
    match KIMG_ALIAS_OFF.load(core::sync::atomic::Ordering::Relaxed) { 0 => None, off => Some(crate::mem::kpa(va) + off) }
}

/// Build a new address space: a fresh PML4 with its own PDPT into which
/// the kernel entries are copied (GB0-3 are shared). It returns the PML4's physical address.
/// IMPORTANT: we copy from the reference kernel PML4 and NOT from the active CR3 — otherwise
/// the current process's private entries would leak into the new AS (a fork bug).
/// Map 1 GB of MMIO at the physical address phys as identity-uncached (PLAN-V1.1 §4.3/4.4).
/// The kernel identity-maps only the first 4 GB, while the EC2/UEFI firmware puts the 64-bit BARs
/// higher (with ENA/NVMe on Nitro that is tens of GB). Reassigning such a BAR into the 32-bit window
/// is dangerous: the host bridge/root port windows are programmed by the firmware — instead we add
/// a 1G entry into the kernel PDPT. Call it BEFORE the processes' address spaces are created
/// (they copy the kernel PDPT entirely). It returns the vaddr (== phys) or 0.
pub fn map_ram_1g(phys: u64) -> u64 { map_1g(phys, true) }
/// Does [vaddr, vaddr+pages*4K) touch a gigabyte the reference kernel table maps by a 1G
/// identity leaf (`map_1g`: the high area, the NUMA areas, the MMIO windows). Every AS
/// carries a copy of those leaves, and the kernel reaches the frames there at their
/// identity address whatever AS is active — a process mapping of its own over one would
/// turn the kernel's write into a write into the process's page. `mmap` refuses such a
/// MAP_FIXED the way it refuses one below 4 GB.
pub fn ident_leaf_overlap(vaddr: u64, pages: u64) -> bool {
    let end = vaddr.saturating_add(pages * 4096);
    let mut g = vaddr & !((1u64 << 30) - 1);
    // SAFETY: P1 + P2 — a read-only walk of the reference kernel table.
    unsafe {
        let k = KERNEL_PML4.load(core::sync::atomic::Ordering::Relaxed);
        let pml4 = if k != 0 { kptr::<Pte>(k) } else { cr3_root() };
        while g < end && g < USER_TOP {
            let e4 = pte_r(pml4.add(((g >> 39) & 0x1FF) as usize));
            if e4 & PTE_P == 0 { g = ((g >> 39) + 1) << 39; continue; }
            let e3 = pte_r(kptr::<Pte>(e4 & ADDR_MASK).add(((g >> 30) & 0x1FF) as usize));
            if g >= 4u64 << 30 && e3 & PTE_P != 0 && e3 & PTE_PS != 0 {
                // under strict a process space has none of the map's RAM here (`strict_strip`)
                if !strict() { return true; }
                let (lo, hi) = (vaddr.max(g) & !0x1F_FFFF, end.min(g + (1 << 30)));
                let mut c = lo;
                while c < hi { if !strict_drops(c) { return true; } c += 0x20_0000; }
            }
            g += 1 << 30;
        }
    }
    false
}
/// The same for a device's MMIO registers (uncached). With the devices' window ON the kernel
/// reaches the registers there (`mmio_va`), and no identity leaf is laid: the gigabyte gets its
/// window gigabyte now (0: it could not — RAM in its piece, or the window is full).
pub fn map_mmio_1g(phys: u64) -> u64 {
    if MWIN_ON.load(core::sync::atomic::Ordering::Acquire) {
        return if crate::mem::in_mwin(mmio_va(phys)) { phys } else { 0 };
    }
    map_1g(phys, false)
}
/// A device gigabyte by its IDENTITY, uncached, in the reference table (every later space has
/// it): what ring 3 is handed by its physical address — ivshmem, the host's memory (`syscall`'s
/// shm path), and the identity modes' view of it. The kernel itself uses the window.
pub fn map_mmio_1g_ident(phys: u64) -> u64 { map_1g(phys, false) }
fn map_1g(phys: u64, cached: bool) -> u64 {
    if phys < 4u64 << 30 { return phys; } // it is already in the identity 0..4 GB (GB3 uncached)
    let l4 = ((phys >> 39) & 0x1FF) as usize;
    let l3 = ((phys >> 30) & 0x1FF) as usize;
    // SAFETY: P1 + P2 + P3 — a 1G window for a device's registers in the REFERENCE kernel table (so
    // that every address space inherits it); the missing PML4 slot gets a PDPT through the
    // CAS.
    unsafe {
        let k = KERNEL_PML4.load(core::sync::atomic::Ordering::Relaxed);
        let pml4 = if k != 0 { kptr::<Pte>(k) } else { cr3_root() };
        // The PML4 slot may be empty (the firmware's BARs lie high: under OVMF q35
        // the NVMe's 64-bit BAR moves to 0x3800_0000_0000 — that is PML4[112]) —
        // then we create a PDPT table for it.
        let e4 = pml4.add(l4);
        if pte_r(e4) & PTE_P == 0 {
            let pdpt = match crate::mem::alloc_any_frame() { Some(f) => f, None => return 0 };
            core::ptr::write_bytes(kptr::<u8>(pdpt), 0, 4096);
            pte_w(e4, pdpt | PTE_P | PTE_RW);
        }
        let pdpt = kptr::<Pte>(pte_r(e4) & ADDR_MASK);
        let e3 = pdpt.add(l3);
        if pte_r(e3) & PTE_P == 0 {
            // A 1G identity leaf: P|RW|PS (+PCD|PWT for a device's registers)
            let e = ((phys >> 30) << 30) | PTE_P | PTE_RW | PTE_PS | if cached { 0 } else { 0b1_1000 };
            pte_w(e3, e);
            flush_tlb();
            // Slot 0 of the PML4 is the only one whose PDPT is a process's OWN (the private
            // GB0 directory is there). A window laid here after the workload's address
            // spaces have been created will not get into them: the kernel's access to it under
            // such an AS will give a #PF in ring 0. The devices are brought up at the boot, before
            // the first process — if that is not so, one must know.
            if l4 == 0 && AS_LIVE.load(core::sync::atomic::Ordering::Relaxed) > 0 {
                crate::println!("paging: AN ALARM: the window {:#x} was laid AFTER {} workload ASes were created — they do NOT have it",
                    (phys >> 30) << 30, AS_LIVE.load(core::sync::atomic::Ordering::Relaxed));
            }
            crate::println!("paging: the window {:#x}..{:#x} is mapped (1G, {})",
                (phys >> 30) << 30, ((phys >> 30) + 1) << 30, if cached { "RAM" } else { "uncached MMIO" });
        }
    }
    phys
}

/// An entry of the reference kernel PDPT (slot 0 of the PML4) for the gigabyte `l3` — for the gate
/// `sharedpd`: is the shared gigabyte split and is its directory intact after the cleanup of an AS.
pub fn kernel_pdpt_entry(l3: usize) -> u64 {
    // SAFETY: P1 + P2 — one read of the reference kernel PDPT (a gate's introspection).
    unsafe {
        let k = KERNEL_PML4.load(core::sync::atomic::Ordering::Relaxed);
        let pml4 = if k != 0 { kptr::<Pte>(k) } else { cr3_root() };
        pte_r(kptr::<Pte>(pte_r(pml4) & ADDR_MASK).add(l3 & 0x1FF))
    }
}
/// The same entry in a given AS (the gate `highmem`: a process inherits the kernel's leaf),
/// decoded as `kernel_pdpt_entry` decodes it — a raw read differs on arm64.
pub fn pdpt_entry_in(pml4_phys: u64, l3: usize) -> u64 {
    // SAFETY: P1 + P2 — one read of the AS `pml4_phys`, which the caller owns.
    unsafe {
        let e4 = pte_r(kptr::<Pte>(pml4_phys));
        if e4 & PTE_P == 0 { return 0; }
        pte_r(kptr::<Pte>(e4 & ADDR_MASK).add(l3 & 0x1FF))
    }
}

/// How many workload address spaces have been created over the machine's life (the counter is
/// monotonic — the question is not "how many are there now" but "have there been any already"). It is needed by exactly one
/// statement: the memory/register windows mapped by `map_1g` AFTER the appearance of
/// the first such space will no longer get into it (a process has its own copy of the
/// PDPT for the private GB0 directory). Staying silent about that is not allowed: the kernel's access to
/// such a window under a workload's AS is a #PF in ring 0.
static AS_LIVE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Copy ALL the non-empty entries of the kernel's PML4 except the zeroth one (that one is set by the
/// caller — the private PDPT is there). Previously ONLY the zeroth one was copied, and
/// a device's window from an upper slot (the NVMe's 64-bit BAR under OVMF moves to
/// 0x3800_0000_0000 — that is PML4[112]) was absent from the workload's address space
/// entirely. The PDPT of those slots is SHARED, so everything that appears in
/// them later is visible to all the spaces at once.
/// # Safety
/// P1 + P2: `src` is the kernel's reference PML4, `dst` a fresh frame for a new
/// address space. Both are identity-mapped frames of 4096 bytes, and `dst` is not
/// yet in any CR3 — nobody walks it while it is being filled.
unsafe fn copy_kernel_pml4(src: *const Pte, dst: u64) {
    for i in 1..512usize {
        let e = pte_r(src.add(i));
        if e & PTE_P != 0 { pte_w(kptr::<Pte>(dst).add(i), e); }
    }
}

pub fn new_address_space() -> u64 {
    // SAFETY: P1 + P2 — a fresh frame plus a copy of the kernel's PML4 halves — the table is not in
    // any CR3 yet, so nothing walks it while it is built.
    unsafe {
        let k = KERNEL_PML4.load(core::sync::atomic::Ordering::Relaxed);
        let base_pml4 = if k != 0 { kptr::<Pte>(k) } else { cr3_root() };
        let kern_pdpt = kptr::<Pte>(pte_r(base_pml4) & ADDR_MASK); // PML4[0] -> kernel PDPT
        let new_pml4 = crate::mem::alloc_any_frame().expect("as: pml4");
        let new_pdpt = crate::mem::alloc_any_frame().expect("as: pdpt");
        core::ptr::write_bytes(kptr::<u8>(new_pml4), 0, 4096);
        copy_kernel_pml4(base_pml4, new_pml4);
        for i in 0..512usize { // a copy of the kernel PDPT (0..3 are filled, 4.. are free for the private one)
            pte_w(kptr::<Pte>(new_pdpt).add(i), pte_r(kern_pdpt.add(i)));
        }
        pte_w(kptr::<Pte>(new_pml4), new_pdpt | PTE_P | PTE_RW | PTE_US);
        AS_LIVE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        #[cfg(target_arch = "aarch64")]
        mmu::asid_assign(new_pml4);
        new_pml4
    }
}

/// Build A PROCESS's address space: like new_address_space, but with A PRIVATE
/// copy of the GB0 PD (2M leaves). The kernel's 0-32 MB and the user region 32 MB-1 GB are initially identity
/// (as with the kernel), but now they can be remapped onto private frames per process
/// (the ELF loader privatises the segments). GB1-3 are shared. It returns the PML4's physical address.
pub fn new_process_space() -> u64 {
    // SAFETY: P1 + P2 — as `new_address_space`, and the identity leaves are made SUPERVISOR here —
    // that is what keeps a process out of the kernel's memory (the gate `isolation`).
    unsafe {
        let k = KERNEL_PML4.load(core::sync::atomic::Ordering::Relaxed);
        let base_pml4 = if k != 0 { kptr::<Pte>(k) } else { cr3_root() };
        let kern_pdpt = kptr::<Pte>(pte_r(base_pml4) & ADDR_MASK);
        let kgb = ident_gb();
        let kern_pd0 = kptr::<Pte>(pte_r(kern_pdpt.add(kgb)) & ADDR_MASK); // PDPT[kgb] -> the kernel's GB PD
        let pml4 = crate::mem::alloc_any_frame().expect("proc: pml4");
        let pdpt = crate::mem::alloc_any_frame().expect("proc: pdpt");
        let pd0 = crate::mem::alloc_any_frame().expect("proc: pd0");
        core::ptr::write_bytes(kptr::<u8>(pml4), 0, 4096);
        copy_kernel_pml4(base_pml4, pml4);
        // THE GB0 IDENTITY WINDOW IN A PROCESS'S AS IS FOR THE KERNEL ONLY. In the kernel's AS everything
        // above the kernel's image is marked U/S (that is how the modes in an identity AS live: `run`,
        // `talaria.cell=identity`). A copy of those leaves in a process's AS opened
        // to the workload in ring 3 ALL the physical memory from the end of the kernel's image up to 1 GB —
        // the page tables, the kernel stacks, the FS cache, other processes' frames: a probe
        // from ring 3 read 0x3600000 (the start of the frame allocator, the page tables
        // live there) both in `cell` and in `proc`. It was noticed by the size of
        // `talaria.core=…,full`: it walks the U/S pages and came out larger than
        // a gigabyte with an RSS of 97 MB. Ring 3 now gets access to the identity memory
        // only explicitly — a `protect()` on a handed-out region (MAP_SHARED,
        // shm, a zero-syscall region), and `fork` carries it over to the child.
        for i in 0..512usize {
            pte_w(kptr::<Pte>(pdpt).add(i), pte_r(kern_pdpt.add(i)));
            let e = pte_r(kern_pd0.add(i));
            if e & PTE_P != 0 && e & PTE_PS == 0 {
                // the entry is already split into 4K ones (e.g. the kernel's heap after nx_range): a DEEP COPY of the PT,
                // otherwise the process and the kernel would share one PT -> a map_leaf in this region would spoil the shared one.
                let src_pt = kptr::<Pte>(e & ADDR_MASK);
                let new_pt = crate::mem::alloc_any_frame().expect("proc: pt-copy");
                for j in 0..512usize {
                    let v = pte_r(src_pt.add(j));
                    let v = if v & PTE_PRIV == 0 { v & !PTE_US } else { v };
                    pte_w(kptr::<Pte>(new_pt).add(j), v);
                }
                pte_w(kptr::<Pte>(pd0).add(i), (new_pt & ADDR_MASK) | (e & 0xFFF));
            } else {
                let e = if e & PTE_P != 0 && e & PTE_PRIV == 0 { e & !PTE_US } else { e };
                pte_w(kptr::<Pte>(pd0).add(i), e); // a 2M identity (supervisor) or empty
            }
        }
        pte_w(kptr::<Pte>(pdpt).add(kgb), pd0 | PTE_P | PTE_RW | PTE_US); // the private PD of the kernel's GB
        if strict() { strict_strip(kptr::<Pte>(pdpt), kptr::<Pte>(pd0), kgb); }
        pte_w(kptr::<Pte>(pml4), pdpt | PTE_P | PTE_RW | PTE_US);
        AS_LIVE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        #[cfg(target_arch = "aarch64")]
        mmu::asid_assign(pml4);
        pml4
    }
}

/// A pointer to the 4K PTE for vaddr in A GIVEN space, allocating the intermediate
/// tables and splitting the large leaves (1G/2M) right in that AS.
/// # Safety
/// P1 + P2, as `pde_ptr_in`, down to the 4K level.
unsafe fn pte_ptr_in(pml4_phys: u64, vaddr: u64) -> *mut Pte {
    let idx = [((vaddr >> 39) & 0x1FF) as usize, ((vaddr >> 30) & 0x1FF) as usize,
               ((vaddr >> 21) & 0x1FF) as usize, ((vaddr >> 12) & 0x1FF) as usize];
    let e4 = kptr::<Pte>(pml4_phys).add(idx[0]);
    if pte_r(e4) & PTE_P == 0 { new_level(e4, "pte_in: pdpt"); }
    let e3 = kptr::<Pte>(pte_r(e4) & ADDR_MASK).add(idx[1]);
    if pte_r(e3) & PTE_P == 0 { new_level(e3, "pte_in: pd"); }
    else if pte_r(e3) & PTE_PS != 0 { split_1g(e3); }
    let e2 = kptr::<Pte>(pte_r(e3) & ADDR_MASK).add(idx[2]);
    if pte_r(e2) & PTE_P == 0 {
        if pte_r(e2) & (PTE_LAZY | PTE_PS) == (PTE_LAZY | PTE_PS) { split_2m_lazy(e2); } // a lazy 2M reservation
        else { new_level(e2, "pte_in: pt"); }
    } else if pte_r(e2) & PTE_PS != 0 { split_2m(e2); }
    kptr::<Pte>(pte_r(e2) & ADDR_MASK).add(idx[3])
}

/// Map the 4K page vaddr->phys in A GIVEN space with full rights
/// (writable/user/exec — W^X through NX). With the splitting of the large leaves.
/// Like `map_leaf`, but WITHOUT the PRIV mark: the page is shared (the vDSO), its frame does not
/// belong to the process and must not be freed when it ends.
pub fn map_leaf_shared(pml4_phys: u64, vaddr: u64, phys: u64, writable: bool, user: bool, exec: bool) {
    // SAFETY: P1 + P2 + P3 — the caller owns `pml4_phys` (a process being built or its own AS); the
    // entry is written into a table of THAT space, not of the current one.
    unsafe {
        let pte = pte_ptr_in(pml4_phys, vaddr);
        let mut fl = PTE_P;
        if writable { fl |= PTE_RW; }
        if user { fl |= PTE_US; }
        if !exec { fl |= PTE_NX; }
        pte_w(pte, (phys & ADDR_MASK) | fl);
    }
}

pub fn map_leaf(pml4_phys: u64, vaddr: u64, phys: u64, writable: bool, user: bool, exec: bool) {
    // SAFETY: P1 + P2 + P3 — as `map_leaf_shared`, with the private bit set.
    unsafe {
        let pte = pte_ptr_in(pml4_phys, vaddr);
        let mut fl = PTE_P;
        if writable { fl |= PTE_RW; }
        if user { fl |= PTE_US; }
        if !exec { fl |= PTE_NX; }
        fl |= PTE_PRIV; // mark it private (fork/COW tells the private ones from the kernel identity)
        pte_w(pte, (phys & ADDR_MASK) | fl);
    }
}

/// §3.3: the window of virtual addresses for FILE mappings in the kernel's identity AS.
/// In identity mode vaddr == phys, so the mmap's window previously required that
/// the block cache's pages should lie CONTIGUOUSLY, otherwise an ENODEV was honestly returned.
/// A separate window outside the identity removes the requirement: the cache's pages are mapped
/// one by one, as in a process's address space too.
const FMAP_BASE: u64 = 0x4000_0000_0000;
static FMAP_NEXT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(FMAP_BASE);
/// Occupy a vaddr for a mapping of len bytes (a bump with a separator page).
pub fn fmap_alloc(len: u64) -> u64 {
    FMAP_NEXT.fetch_add(((len + 0xFFF) & !0xFFF) + 0x1000, core::sync::atomic::Ordering::SeqCst)
}
/// Flush the TLB (after a series of map_leaf in the current AS).
// SAFETY: P4 + P5 — the callers have just changed the tables of the current AS.
pub fn flush_tlb_now() { unsafe { flush_tlb(); } }

/// The physical address of the 4K page vaddr in a given AS if it is already mapped by a 4K leaf (for the shared pages of ELF segments).
pub fn phys_of_in(pml4_phys: u64, vaddr: u64) -> Option<u64> {
    // SAFETY: P1 + P2 — a read-only walk of the address space `pml4_phys`, which the caller owns.
    unsafe {
        let idx = [((vaddr >> 39) & 0x1FF) as usize, ((vaddr >> 30) & 0x1FF) as usize, ((vaddr >> 21) & 0x1FF) as usize, ((vaddr >> 12) & 0x1FF) as usize];
        let e4 = pte_r(kptr::<Pte>(pml4_phys).add(idx[0])); if e4 & PTE_P == 0 { return None; }
        let e3 = pte_r(kptr::<Pte>(e4 & ADDR_MASK).add(idx[1])); if e3 & PTE_P == 0 || e3 & PTE_PS != 0 { return None; }
        let e2 = pte_r(kptr::<Pte>(e3 & ADDR_MASK).add(idx[2])); if e2 & PTE_P == 0 || e2 & PTE_PS != 0 { return None; }
        let e1 = pte_r(kptr::<Pte>(e2 & ADDR_MASK).add(idx[3])); if e1 & PTE_P == 0 || e1 & PTE_PRIV == 0 { return None; }
        Some(e1 & ADDR_MASK)
    }
}
/// An OR of a page's rights (writable/exec) in a given AS — for a page shared by two ELF segments.
pub fn or_perms_in(pml4_phys: u64, vaddr: u64, writable: bool, exec: bool) {
    // SAFETY: P1 + P2 + P3 — rights are ADDED to an existing entry of the space `pml4_phys`, which
    // the caller owns.
    unsafe {
        let pte = pte_ptr_in(pml4_phys, vaddr);
        let mut e = pte_r(pte);
        if writable { e |= PTE_RW; }
        if exec { e &= !PTE_NX; }
        pte_w(pte, e);
    }
}
/// Map the 4K page vaddr->phys in A GIVEN space (executable).
/// It suits both a private region (vaddr>=4 GB) and GB0.
pub fn map_in(pml4_phys: u64, vaddr: u64, phys: u64, writable: bool, user: bool) {
    map_leaf(pml4_phys, vaddr, phys, writable, user, true);
}

/// Switch the active address space (load CR3). The TLB is flushed.
/// # Safety
/// P1 + P4 + P5: `pml4_phys` must be a COMPLETE page table (its kernel half copied,
/// see `copy_kernel_pml4`) of an address space that is alive — a task's, or the
/// kernel's own. After the write every address on this core means what that table
/// says, so the caller must not hold a pointer into the previous space across it,
/// and the kernel stack it runs on must be mapped in the new one (it is: the kernel
/// half is shared).
pub unsafe fn switch(pml4_phys: u64) {
    mmu::set_root(pml4_phys);
    crate::sched::note_real_cr3();
}

/// The identity gigabyte mapped by 2M leaves: the RAM's lowest gigabyte with the kernel — the
/// identity modes' user region (`init`), and the gigabyte a process's space gets a private PD
/// for (`new_process_space`). GB0 on x86, the RAM's first gigabyte on arm64. (It was `user_gb`
/// while processes lived in it; under strict they do not — ARM64-PLAN §11 item 4.)
fn ident_gb() -> usize { (crate::mem::kernel_lo() >> 30) as usize }

/// The current CR3 (the active PML4's physical address + the flags).
pub fn kernel_cr3() -> u64 { KERNEL_PML4.load(core::sync::atomic::Ordering::Relaxed) }

pub fn current_cr3() -> u64 { mmu::root() }

/// Clone an address space (a fork's memory): it creates a new AS and COPIES
/// src's private pages (an eager copy; COW is an optimisation for later). The kernel part
/// is shared (through new_address_space). It returns the new PML4's physical address.
pub fn fork_address_space(src_pml4: u64) -> u64 {
    let dst = new_address_space();
    crate::vdso::map_into(dst); // see fork_cow_space: the vDSO's image is shared rather than private
    // SAFETY: P1 + P2 + P3 — a copy of the parent's tables into a child's — both spaces belong to
    // the caller (`fork` runs with the parent's threads stopped at this point), and the
    // child's table is not in any CR3 yet.
    unsafe {
        let src_pdpt = kptr::<Pte>(pte_r(kptr::<Pte>(src_pml4)) & ADDR_MASK);
        for l3 in 4..512usize { // the private region (the kernel has slots 0..3)
            let e3 = pte_r(src_pdpt.add(l3));
            if e3 & PTE_P == 0 || e3 & PTE_PS != 0 { continue; }
            let pd = kptr::<Pte>(e3 & ADDR_MASK);
            for l2 in 0..512usize {
                let e2 = pte_r(pd.add(l2));
                if e2 & PTE_P == 0 || e2 & PTE_PS != 0 { continue; }
                let pt = kptr::<Pte>(e2 & ADDR_MASK);
                for l1 in 0..512usize {
                    let e1 = pte_r(pt.add(l1));
                    if e1 & PTE_P == 0 { continue; }
                    let vaddr = ((l3 as u64) << 30) | ((l2 as u64) << 21) | ((l1 as u64) << 12);
                    let np = crate::mem::alloc_frame().expect("fork: page");
                    core::ptr::copy_nonoverlapping(kptr::<u8>(e1 & ADDR_MASK), kptr::<u8>(np), 4096);
                    map_in(dst, vaddr, np, e1 & PTE_RW != 0, e1 & PTE_US != 0);
                }
            }
        }
    }
    dst
}

// ── copy-on-write (COW) for fork: the lazy copying of private pages ────────────
const PTE_COW: u64 = 1 << 9; // an OS-available bit: the page is shared by COW
use core::sync::atomic::AtomicU64 as CowU64;
static COW_COUNT: CowU64 = CowU64::new(0);
/// How many frames were rejected in the lazy filling (they coincided with a private
/// identity vaddr) and how many times the filling gave up entirely.
static LAZY_REJECT: CowU64 = CowU64::new(0);
static LAZY_GIVEUP: CowU64 = CowU64::new(0);
static LAZY_GIVEUP_TRACE: CowU64 = CowU64::new(4);

/// The counters of the lazy filling for `kstat paging`.
pub fn lazy_stats() -> (u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    (LAZY_REJECT.load(Relaxed), LAZY_GIVEUP.load(Relaxed))
}

/// How many COW copies were made (for the tests/telemetry).
pub fn cow_count() -> u64 { COW_COUNT.load(core::sync::atomic::Ordering::Relaxed) }

/// A fork through COW: the new space shares the source's PHYSICAL pages; the writable
/// private pages are marked RO+COW on both sides (the copy happens at the first write).
/// The RO pages (the code) are shared as they are. It is cheaper than an eager fork: there are 0 copies before a write.
static FORK_PNONE2M: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub fn fork_cow_space(src: u64) -> u64 {
    let dst = new_process_space(); // dst with a private GB0 PD, without copies of the pages
    // The vDSO's image is ONE for all the processes, so its pages are NOT private, while
    // the loop below copies the private ones. Which means the child must be given it explicitly,
    // otherwise it gets an address space WITHOUT the clock in user space: the very first
    // call of __vdso_clock_gettime is a #PF on an instruction fetch at 0x7ffd…, and
    // the process is killed by signal 11. That is how ALL of PostgreSQL's backends died, while the
    // postmaster itself lived: it did get the vDSO when its image was loaded.
    crate::vdso::map_into(dst);
    // SAFETY: P1 + P2 + P3 — as `fork_address_space`, but the leaves are shared read-only (COW);
    // the reference counting of the frames is `mem`'s business.
    unsafe {
        let src_pml4 = kptr::<Pte>(src);
        for l4 in 0..512usize {
            let e4 = pte_r(src_pml4.add(l4));
            if e4 & PTE_P == 0 { continue; }
            let pdpt = kptr::<Pte>(e4 & ADDR_MASK);
            for l3 in 0..512usize {
                let e3 = pte_r(pdpt.add(l3));
                if e3 & PTE_P == 0 || e3 & PTE_PS != 0 { continue; }
                let pd = kptr::<Pte>(e3 & ADDR_MASK);
                for l2 in 0..512usize {
                    let mut e2 = pte_r(pd.add(l2));
                    // A huge page of the workload: our COW is page-by-page, so
                    // before a fork we exchange it for 512 entries. A lazy 2M reservation
                    // is copied to the child as it is — there are no frames behind it.
                    if e2 & PTE_PS != 0 && e2 & PTE_PRIV != 0 {
                        if e2 & PTE_P != 0 { split_2m(pd.add(l2)); e2 = pte_r(pd.add(l2)); }
                        else {
                            let vaddr2 = ((l4 as u64) << 39) | ((l3 as u64) << 30) | ((l2 as u64) << 21);
                            if e2 & PTE_PNONE != 0 && e2 & PTE_LAZY == 0 && FORK_PNONE2M.fetch_add(1, core::sync::atomic::Ordering::Relaxed) < 4 {
                                crate::println!("fork: a PROT_NONE 2 MB page WITH frames at {:#x} (frame {:#x}) copied to the child UNCOUNTED", vaddr2, e2 & ADDR_MASK);
                            }
                            pte_w(pde_ptr_in(dst, vaddr2), e2);
                            continue;
                        }
                    }
                    if e2 & PTE_P == 0 || e2 & PTE_PS != 0 { continue; }
                    let pt = kptr::<Pte>(e2 & ADDR_MASK);
                    for l1 in 0..512usize {
                        let e1p = pt.add(l1);
                        let e1 = pte_r(e1p);
                        let vaddr = ((l4 as u64) << 39) | ((l3 as u64) << 30) | ((l2 as u64) << 21) | ((l1 as u64) << 12);
                        // A LAZY reservation has no frame: the child inherits it as a reservation
                        // (it will get its own zeroes at the first touch — Linux's semantics)
                        if e1 & PTE_LAZY != 0 { pte_w(pte_ptr_in(dst, vaddr), e1); continue; }
                        if e1 & PTE_P == 0 { continue; }
                        if e1 & PTE_PRIV == 0 {
                            // Not private: identity memory. The one HANDED OUT to the workload
                            // (U/S — MAP_SHARED, shm) stays shared with the child too —
                            // PostgreSQL keeps its shared buffers that way; the rest of the window in
                            // a process's AS is closed (see `new_process_space`).
                            // The window is the gigabyte with the private PD — GB0 on x86, the
                            // RAM's first gigabyte on arm64 (`ident_gb`); "< 1 GB" here left an
                            // arm64 child without its parent's MAP_SHARED page (a rights fault on
                            // the kernel's own 2M leaf at the child's first write).
                            // And since stage 4a of the direct map a MAP_SHARED is a non-private
                            // user leaf at an address of its own: any such leaf below USER_TOP is
                            // the same shared frame for the child (the vDSO's two pages are too).
                            if e1 & PTE_US != 0 && vaddr < USER_TOP
                                && ((vaddr >> 30) as usize == ident_gb() && e1 & ADDR_MASK == vaddr || (vaddr >> 30) as usize != ident_gb()) {
                                pte_w(pte_ptr_in(dst, vaddr), e1);
                            }
                            continue;
                        }
                        // ANY private page (RW, RO after an mprotect, already COW) -> RO + a COW mark on both
                        // sides + a reference: an RO page is not "code forever" — an mprotect(RW) in the parent without
                        // COW would give a write into the frame shared with the child (PostgreSQL: an mprotect on the heap).
                        // The untying of the COW happens at a write #PF (cow_fault) or at an mprotect(RW) (protect).
                        let cow = (e1 & !PTE_RW) | PTE_COW;
                        pte_w(e1p, cow);                 // the parent's side
                        pte_w(pte_ptr_in(dst, vaddr), cow); // the child's side (the same phys)
                        crate::mem::rc_share(e1 & ADDR_MASK);
                    }
                }
            }
        }
        flush_tlb(); // src is active: the removal of RW must get into the TLB
    }
    // ...and into the TLBs of the parent's OTHER threads on other cores: a writable entry
    // cached there would let their writes after the fork land in the frame the child now
    // shares (both machines; the local flush above was all there was).
    shootdown_range_user(0, 0);
    um_mark(1, dst);
    dst
}

// THE COW TRACE (with `talaria.freetrace=1`, ARM64-PLAN §11 item 0): the last copies made by
// `cow_fault` — the page, the source frame, the copy. A process that reads zeros from a fresh
// copy points at its SOURCE: was that frame freed (and reused) before it was copied from?
const CT_N: usize = 1024;
static CT: [[core::sync::atomic::AtomicU64; 4]; CT_N] = [const { [const { core::sync::atomic::AtomicU64::new(0) }; 4] }; CT_N];
static CT_SEQ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
fn cow_note(va: u64, src: u64, dst: u64) {
    use core::sync::atomic::Ordering::Relaxed;
    if !crate::mem::FREETRACE.load(Relaxed) { return; }
    let i = (CT_SEQ.fetch_add(1, Relaxed) as usize) % CT_N;
    CT[i][0].store(va, Relaxed); CT[i][1].store(src, Relaxed); CT[i][2].store(dst, Relaxed);
    CT[i][3].store(crate::sched::current_id() as u64 | (crate::mem::free_seq() << 16), Relaxed);
}
/// The COW copy whose destination is `dst`: (the page, the source frame, the task, the free
/// counter at the copy).
pub fn cow_find(dst: u64) -> Option<(u64, u64, u64, u64)> {
    use core::sync::atomic::Ordering::Relaxed;
    let now = CT_SEQ.load(Relaxed);
    for back in 1..=now.min(CT_N as u64) {
        let i = ((now - back) as usize) % CT_N;
        if CT[i][2].load(Relaxed) == dst {
            let w = CT[i][3].load(Relaxed);
            return Some((CT[i][0].load(Relaxed), CT[i][1].load(Relaxed), w & 0xFFFF, w >> 16));
        }
    }
    None
}

// THE AT-CRASH AUDIT (`talaria.faultaudit=1`, ARM64-PLAN §11 item 0). Nothing runs before the
// fault — every instrument that touched the hot path hid the race — so the consistency of what
// the live spaces map is checked only when a process has already crashed: every private user
// frame against the ledger (handed out?) and the refcount (as many owners as mappings?), every
// table frame against the leaves (a page table mapped as somebody's page?).
pub static FAULTAUDIT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
const AU_N: usize = 1 << 16;
static mut AU_K: [u64; AU_N] = [0; AU_N];
static mut AU_C: [u16; AU_N] = [0; AU_N];
static mut AU_T: [u64; 8192] = [0; 8192];
pub fn audit_spaces() {
    let mut cr3s = [0u64; 64];
    let ns = crate::sched::live_spaces(&mut cr3s);
    // SAFETY: at a crash, single-threaded use of the audit's own tables; the walks only read.
    unsafe {
        let (k, c, tb) = (&mut *core::ptr::addr_of_mut!(AU_K), &mut *core::ptr::addr_of_mut!(AU_C), &mut *core::ptr::addr_of_mut!(AU_T));
        k.fill(0); c.fill(0);
        let mut nt = 0usize; let mut leaves = 0u64; let mut over = false;
        let mut add = |f: u64| {
            let mut h = ((f >> 12).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 48) as usize % AU_N;
            for _ in 0..AU_N { if k[h] == 0 || k[h] == f { k[h] = f; c[h] = c[h].saturating_add(1); return true; } h = (h + 1) % AU_N; }
            false
        };
        let kern = kernel_cr3() & ADDR_MASK;
        let kpml4 = if kern != 0 { kptr::<Pte>(kern).cast_const() } else { core::ptr::null() };
        let kpdpt = if kpml4.is_null() { core::ptr::null() } else { kptr::<Pte>(pte_r(kpml4) & ADDR_MASK).cast_const() };
        for &cr3 in &cr3s[..ns] {
            let pml4 = kptr::<Pte>(cr3);
            for l4 in 0..256usize {
                let e4 = pte_r(pml4.add(l4));
                if e4 & PTE_P == 0 || (!kpml4.is_null() && e4 == pte_r(kpml4.add(l4))) { continue; }
                if nt < tb.len() { tb[nt] = e4 & ADDR_MASK; nt += 1; }
                let pdpt = kptr::<Pte>(e4 & ADDR_MASK);
                for l3 in 0..512usize {
                    let e3 = pte_r(pdpt.add(l3));
                    if e3 & PTE_P == 0 || e3 & PTE_PS != 0 { continue; }
                    if l4 == 0 && !kpdpt.is_null() && e3 == pte_r(kpdpt.add(l3)) { continue; }
                    if nt < tb.len() { tb[nt] = e3 & ADDR_MASK; nt += 1; }
                    let pd = kptr::<Pte>(e3 & ADDR_MASK);
                    for l2 in 0..512usize {
                        let e2 = pte_r(pd.add(l2));
                        if e2 & PTE_PS != 0 {
                            if e2 & PTE_PRIV != 0 && e2 & PTE_LAZY == 0 && e2 & (PTE_P | PTE_PNONE) != 0 { leaves += 1; over |= !add(e2 & ADDR_MASK); }
                            continue;
                        }
                        if e2 & PTE_P == 0 { continue; }
                        if nt < tb.len() { tb[nt] = e2 & ADDR_MASK; nt += 1; }
                        let pt = kptr::<Pte>(e2 & ADDR_MASK);
                        for l1 in 0..512usize {
                            let e1 = pte_r(pt.add(l1));
                            if e1 & PTE_PRIV == 0 || e1 & PTE_LAZY != 0 || e1 & (PTE_P | PTE_PNONE) == 0 { continue; }
                            leaves += 1; over |= !add(e1 & ADDR_MASK);
                        }
                    }
                }
            }
        }
        crate::println!("[audit] {} live spaces, {} private leaves, {} table frames{}", ns, leaves, nt, if over { " (the table OVERFLOWED)" } else { "" });
        let mut bad = 0;
        for h in 0..AU_N {
            if k[h] == 0 { continue; }
            let (f, n) = (k[h], c[h] as u64);
            let rc = crate::mem::rc_get(f) as u64;
            let handed = crate::mem::fc_handed(f);
            let wrong = handed == Some(false) || (n >= 2 && rc < n);
            if wrong && bad < 12 {
                crate::println!("[audit] FRAME {:#x}: mapped {} times, refcount {}, handed out {:?}{}", f, n, rc, handed,
                    if handed == Some(false) { " <-- MAPPED BUT FREE" } else { " <-- THE REFCOUNT IS LOWER THAN THE OWNERS" });
                if let Some((sp, va, _)) = crate::sched::frame_mapped_elsewhere(f, 0) { crate::println!("[audit]   e.g. in the space {:#x} at {:#x}", sp, va); }
            }
            if wrong { bad += 1; }
        }
        let mut tbad = 0;
        for i in 0..nt {
            let t = tb[i];
            let handed = crate::mem::fc_handed(t);
            let mut h = ((t >> 12).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 48) as usize % AU_N;
            let mut as_leaf = false;
            for _ in 0..AU_N { if k[h] == 0 { break; } if k[h] == t { as_leaf = true; break; } h = (h + 1) % AU_N; }
            let dup = tb[..i].contains(&t);
            if handed == Some(false) || as_leaf || dup {
                if tbad < 8 { crate::println!("[audit] TABLE FRAME {:#x}: handed out {:?}{}{}", t, handed, if as_leaf { ", ALSO MAPPED AS A USER PAGE" } else { "" }, if dup { ", IN TWO SPACES" } else { "" }); }
                tbad += 1;
            }
        }
        crate::println!("[audit] {} frame(s) and {} table(s) inconsistent", bad, tbad);
    }
}

/// Handle a COW #PF at vaddr in the active AS: if the page is present+COW+RO —
/// allocate a private copy, copy it, set RW and remove COW. true = it has been handled.
/// Spurious write faults (a stale read-only translation of a page already writable) taken.
pub static SPURIOUS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static SPUR_LAST: [core::sync::atomic::AtomicU64; 64] = [const { core::sync::atomic::AtomicU64::new(0) }; 64];
static SPUR_RUN: [core::sync::atomic::AtomicU64; 64] = [const { core::sync::atomic::AtomicU64::new(0) }; 64];
static SPUR_TSC: [core::sync::atomic::AtomicU64; 64] = [const { core::sync::atomic::AtomicU64::new(0) }; 64];
pub fn cow_fault(vaddr: u64) -> bool {
    let a = vaddr & !0xFFF;
    // A fast NON-SPLITTING check of the COW bit: not COW -> false at once (we do not touch
    // the kernel's large leaves at ordinary/wild write faults). A COW page is always 4K.
    // SAFETY: P1 + P2 — the walk that decides whether this fault is a COW one.
    let we = unsafe { walk_entry(a) };
    // A STALE TRANSLATION, NOT A FAULT. The entry is already the user's, present and writable:
    // another core made it so — an in-place un-COW (the last owner) or a copy — and this core
    // still held the old read-only translation (a sibling thread, or a lazily kept CR3). Linux
    // calls it a spurious fault: drop the translation and retry. Without this a multithreaded
    // process died with SIGSEGV on its own writable page after a fork (`cowstale threads`,
    // ARM64-PLAN §11 item 0). Eight in a row (within a millisecond each) at one address on one
    // core is not staleness — it goes on as a fault.
    if we & PTE_P != 0 && we & PTE_RW != 0 && we & PTE_US != 0 && we & PTE_COW == 0 && a < USER_TOP {
        use core::sync::atomic::Ordering::Relaxed;
        let me = crate::percpu::cpu_id().min(63);
        // "in a row" is in TIME: the same page goes stale legitimately once per fork
        let now = crate::tsc::rdtsc();
        let close = now.wrapping_sub(SPUR_TSC[me].swap(now, Relaxed)) < crate::tsc::hz().max(1_000_000) / 1000;
        let n = if SPUR_LAST[me].swap(a, Relaxed) == a && close { SPUR_RUN[me].fetch_add(1, Relaxed) + 1 } else { SPUR_RUN[me].store(0, Relaxed); 0 };
        if n < 8 {
            // SAFETY: P5 — this core's own translation of one page.
            unsafe { mmu::flush_page(a); }
            SPURIOUS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            return true;
        }
        return false;
    }
    if we & PTE_P == 0 || we & PTE_COW == 0 || we & PTE_RW != 0 { return false; }
    // SAFETY: P1 + P2 + P3 + P4 — the copy — a fresh frame, the old page's bytes through the
    // identity map, then the entry; the CAS keeps two threads faulting on the same page
    // from making two copies.
    unsafe {
        if let Some(pte) = pte_ptr(a) {
            let e = pte_r(pte);
            if e & PTE_P != 0 && e & PTE_COW != 0 && e & PTE_RW == 0 {
                let phys = e & ADDR_MASK;
                let keep = e & (PTE_US | PTE_NX | PTE_PRIV); // preserve US/NX/PRIV
                if crate::mem::rc_get(phys) <= 1 {
                    // the last co-owner -> we make it writable IN PLACE (without a copy, without an orphan)
                    // (a sibling thread faulting on the same page at once: the CAS lets one install)
                    if pte_cas(pte, e, phys | PTE_P | PTE_RW | keep) { crate::mem::rc_set(phys, 0); }
                } else {
                    // there are other co-owners -> a private copy, the original has one reference less.
                    // The copy comes from the node OF THE ONE WHO WRITES: the page moves to it forever.
                    let np = crate::mem::alloc_contig_node(1, crate::affinity::my_node()).expect("cow: page");
                    core::ptr::copy_nonoverlapping(kptr::<u8>(phys), kptr::<u8>(np), 4096);
                    if keep & PTE_NX == 0 { crate::arch::icache::sync(kva(np), 4096); }   // an executable page: fetch must see the copy
                    // The copy is installed by a CAS over the entry the decision was made on: two
                    // threads of one process faulting on the same page used to both install
                    // (the second overwrote the first's copy, and the writes made into it were
                    // lost) and both drop a reference for ONE mapping. The loser gives its copy
                    // back and returns: the instruction re-runs on the winner's page.
                    if !pte_cas(pte, e, (np & ADDR_MASK) | PTE_P | PTE_RW | keep) {
                        crate::mem::free_contig(np, 1);
                        return true;
                    }
                    cow_note(a, phys, np);
                    // THE OTHER CORES' TRANSLATIONS OF THIS PAGE GO BEFORE THE ORIGINAL MAY.
                    // The entry now names the copy, but a core that holds this space — a sibling
                    // thread, or a core that ran this process and then a kernel task without
                    // reloading CR3 (the lazy switch) — still translates the page to the
                    // ORIGINAL, read-only. Freed below at once, the original was handed out
                    // and zeroed, and the process, back on that core with "the same CR3, no
                    // reload", read its stack from it: PostgreSQL's postmaster returned into 0
                    // on a c6i (and under TCG locally, ~1 boot in 5), and a reused page table
                    // killed the machine (ARM64-PLAN §11 item 0). As `munmap` does: the local
                    // flush, the shootdown to every core that may hold this space, then the free.
                    mmu::flush_page(a);
                    shootdown_range_user(a, 1);
                    // The other owners may have let go meanwhile (a child's own copy, an exit):
                    // then the original is nobody's and goes back to the allocator.
                    if crate::mem::rc_release(phys) { crate::mem::free_contig(phys, 1); }
                    COW_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                }
                mmu::flush_page(a);
                return true;
            }
        }
    }
    false
}


/// A munmap in process mode: for every page — if it is PRIV: COW co-owners -> only
/// remove a reference; the sole owner -> free the frame. A non-PRIV one (shared/identity) — only
/// remove present (we do not free the frame: another process may own it).
pub static UNMAP_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static UNMAP_PAGES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static UNMAP_HIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static UNMAP_WALK_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static UNMAP_FLUSH_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static UNMAP_SHOOT_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The last `unmap_proc` calls: the range, the task and core, the space (the fault report:
/// "who removed this page of a live process?" — ARM64-PLAN §11 item 0).
static UM_RING: [[core::sync::atomic::AtomicU64; 4]; 64] = [const { [const { core::sync::atomic::AtomicU64::new(0) }; 4] }; 64];
static UM_N: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// A marker in the same ring (a fork: `what` = 1, the child's root in `pages`).
pub fn um_mark(what: u64, val: u64) {
    use core::sync::atomic::Ordering::Relaxed;
    let i = (UM_N.fetch_add(1, Relaxed) % 64) as usize;
    UM_RING[i][0].store(what, Relaxed); UM_RING[i][1].store(val, Relaxed);
    UM_RING[i][2].store(crate::sched::current_id() as u64 & 0xFFFF | (crate::percpu::cpu_id() as u64) << 16, Relaxed);
    UM_RING[i][3].store(current_cr3() & ADDR_MASK, Relaxed);
}
/// The whole ring, oldest first: (vaddr or marker, pages or value, task|core<<16|line<<24, cr3).
pub fn um_dump(mut f: impl FnMut(u64, u64, u64, u64, u64)) {
    use core::sync::atomic::Ordering::Relaxed;
    let n = UM_N.load(Relaxed);
    for k in n.saturating_sub(64)..n {
        let i = (k % 64) as usize;
        f(k, UM_RING[i][0].load(Relaxed), UM_RING[i][1].load(Relaxed), UM_RING[i][2].load(Relaxed), UM_RING[i][3].load(Relaxed));
    }
}
#[track_caller]
pub fn unmap_proc(vaddr: u64, pages: u64) {
    {
        use core::sync::atomic::Ordering::Relaxed;
        let i = (UM_N.fetch_add(1, Relaxed) % 64) as usize;
        UM_RING[i][0].store(vaddr, Relaxed); UM_RING[i][1].store(pages, Relaxed);
        let t = crate::sched::current_id();
        let sys = if t < crate::sched::NTASKS { crate::syscall::LAST_SYS[t].load(Relaxed) >> 48 } else { 0xFFFF };
        UM_RING[i][2].store(t as u64 & 0xFFFF | (crate::percpu::cpu_id() as u64 & 0xFF) << 16 | (core::panic::Location::caller().line() as u64 & 0xFFFF) << 24 | (sys & 0xFFFF) << 40, Relaxed);
        UM_RING[i][3].store(current_cr3() & ADDR_MASK, Relaxed);
    }
    // A BREAKDOWN OF THE PRICE. `sysprof` on PostgreSQL showed a `munmap` of 9918 ns against
    // its own `mmap` of 983 — while both call THIS function. Which means it is not about
    // the call but about the fact that `munmap`'s pages REALLY exist: a walk with
    // the freeing of the frames, a local flush and the collection of the neighbours' acknowledgements.
    // In order to fix things not at random, the three addends are counted separately.
    let t_call = crate::tsc::rdtsc();
    UNMAP_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    UNMAP_PAGES.fetch_add(pages, core::sync::atomic::Ordering::Relaxed);
    // SAFETY: P1 + P2 + P4 — the process's pages are removed and their frames returned; the range
    // is flushed and the other cores are shot down by the caller.
    unsafe {
        let mut a = vaddr;
        let end = vaddr + pages * 4096;
        // The TLB must be flushed at the neighbours only if we removed AT LEAST ONE real
        // (present) mapping: a non-present entry has nothing to come
        // from in a neighbour's TLB — x86 does not cache absent entries (this same rule is already
        // written above `shootdown`). And there is very often nothing to remove: an `mmap`
        // starts with an `unmap_proc` over the requested range, and for
        // A FRESH address there is nothing there at all.
        //
        // The price of an unconditional flush is not cycles but THE WAIT: the initiator stands
        // until every neighbouring core acknowledges, while a core in a long syscall
        // goes with IF=0 and will acknowledge no earlier than it reaches the servicing point.
        // The measurement on ClickHouse: an `mmap` of 157 us per call on four cores against
        // 69 on one — that is, more than half of the call's time went into collecting
        // the acknowledgements of a flush that was not required.
        let mut present_gone = false;
        let flush = || { flush_range_all(vaddr & !0xFFF, pages, shootdown_range_user); };
        let mut dfr = Deferred::new();
        while a < end {
            // A whole huge page: free 2 MB in one piece rather than
            // splitting it into 512 entries only to throw them away at once.
            if a & 0x1F_FFFF == 0 && end - a >= 0x20_0000 {
                if let Some(pde) = pde_ptr_existing(a) {
                    let e = pte_r(pde);
                    if e & PTE_PS != 0 && e & PTE_PRIV != 0 {
                        // A LAZY reservation has no frame — there is nothing to free.
                        // Without this check a reservation marked PROT_NONE (and it is
                        // exactly like that that a runtime reserves the heap: an mmap PROT_NONE of
                        // hundreds of megabytes) gave the allocator 512
                        // frames at the physical address ZERO at a munmap: a reservation's address
                        // bits are empty. After that those frames were handed out to the workload —
                        // and it got memory on top of which everything
                        // else lies. From the outside that is "random" corruption after minutes.
                        // Instead of a hole there is the original identity leaf without U/S (see ident_leaf).
                        pte_w(pde, proc_ident_leaf_2m(a));
                        if e & PTE_LAZY == 0 && (e & PTE_P != 0 || e & PTE_PNONE != 0) {
                            if e & PTE_P != 0 { present_gone = true; }
                            let phys = e & ADDR_MASK;
                            if crate::mem::rc_release(phys) { dfr.put(phys, 512, &flush); }
                        }
                        a += 0x20_0000;
                        continue;
                    }
                }
            }
            let stop = chunk_end(a, end);
            let pt = match pt_for(kpa_of(cr3_root()), a, false) { Some(p) => p, None => { a = stop; continue; } };
            while a < stop {
                let pte = pt.add(pt_idx(a));
                let e = pte_r(pte);
                if e & PTE_LAZY != 0 { pte_w(pte, proc_ident_leaf(a)); } // a reservation without a frame — just remove it
                else if e & (PTE_P | PTE_PNONE) != 0 { // present OR PROT_NONE (there is a frame behind it)
                    if e & PTE_P != 0 { present_gone = true; }
                    let phys = e & ADDR_MASK;
                    // In the identity window it is the return of a supervisor leaf (the kernel will still need the frame at
                    // this address; none under strict), above the window it is the former non-present stub,
                    // by which protect()/unguard() will give a mapping again.
                    pte_w(pte, proc_ident_leaf(a));
                    if e & PTE_PRIV != 0 && crate::mem::rc_release(phys) { dfr.put(phys, 1, &flush); }
                }
                a += 0x1000;
                if a & 0x3_FFFF == 0 { tlb_service(); } // the walk goes with IF=0 — once every 64 pages serve the neighbours' mailbox
            }
        }
        UNMAP_WALK_TSC.fetch_add(crate::tsc::rdtsc().wrapping_sub(t_call), core::sync::atomic::Ordering::Relaxed);
        if present_gone {
            UNMAP_HIT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            let t1 = crate::tsc::rdtsc();
            #[cfg(target_arch = "aarch64")]
            let (t2, t3) = { flush_range_all(vaddr & !0xFFF, pages, shootdown_range_user); let t = crate::tsc::rdtsc(); (t, t) };
            #[cfg(not(target_arch = "aarch64"))]
            let (t2, t3) = {
                flush_range(vaddr & !0xFFF, pages);
                let t2 = crate::tsc::rdtsc();
                shootdown_range_user(vaddr & !0xFFF, pages);
                (t2, crate::tsc::rdtsc())
            };
            UNMAP_FLUSH_TSC.fetch_add(t2.wrapping_sub(t1), core::sync::atomic::Ordering::Relaxed);
            UNMAP_SHOOT_TSC.fetch_add(t3.wrapping_sub(t2), core::sync::atomic::Ordering::Relaxed);
        }
        dfr.free(); // after the flush: no core translates to these frames any more
    }
}
/// Release a private frame: the co-owners (COW/shared RO after a fork) -> the reference -1, otherwise into the free list.
fn release_priv_frame(phys: u64) {
    if crate::mem::rc_release(phys) { crate::mem::free_contig(phys, 1); }
}
/// Find the vaddr at which phys is mapped by a PRIV 4K leaf in the AS pml4 (the diagnostics of a double ownership).
#[allow(dead_code)]
pub fn find_phys_in(pml4: u64, phys: u64) -> Option<(u64, u64)> {
    // SAFETY: P1 + P2 — a read-only search over the tables of `pml4_phys`, which the caller owns.
    unsafe {
        for l4 in 0..512usize {
            let e4 = pte_r(kptr::<Pte>(pml4).add(l4)); if e4 & PTE_P == 0 { continue; }
            let p3 = e4 & ADDR_MASK;
            for l3 in 0..512usize {
                let e3 = pte_r(kptr::<Pte>(p3).add(l3)); if e3 & PTE_P == 0 || e3 & PTE_PS != 0 { continue; }
                let pd = e3 & ADDR_MASK;
                for l2 in 0..512usize {
                    let e2 = pte_r(kptr::<Pte>(pd).add(l2)); if e2 & PTE_P == 0 || e2 & PTE_PS != 0 { continue; }
                    let pt = e2 & ADDR_MASK;
                    for l1 in 0..512usize {
                        let e1 = pte_r(kptr::<Pte>(pt).add(l1));
                        let va = ((l4 as u64) << 39) | ((l3 as u64) << 30) | ((l2 as u64) << 21) | ((l1 as u64) << 12);
                        if e1 & PTE_P != 0 && e1 & ADDR_MASK == phys && (e1 & PTE_PRIV != 0 || va != phys) { return Some((va, e1)); }
                    }
                }
            }
        }
    }
    None
}
/// # Safety
/// P1 + P4: `pt` is a 4K-level table of an address space BEING DESTROYED — no core
/// may still be walking it (the callers switch the APs to the kernel's CR3 first),
/// otherwise a walk would follow freed frames.
unsafe fn free_pt(pt: u64) {
    for l1 in 0..512usize {
        let e1 = pte_r(kptr::<Pte>(pt).add(l1));
        if e1 & PTE_PRIV == 0 || e1 & PTE_LAZY != 0 { continue; } // identity/shared ones are not ours; LAZY — there is no frame
        if e1 & (PTE_P | PTE_PNONE) == 0 { continue; }            // a PROT_NONE page holds a frame — free it
        release_priv_frame(e1 & ADDR_MASK);
    }
    crate::mem::free_contig(pt, 1);
}
/// # Safety
/// P1 + P4, as `free_pt`, one level up.
unsafe fn free_pd(pd: u64) {
    for l2 in 0..512usize {
        let e2 = pte_r(kptr::<Pte>(pd).add(l2));
        if e2 & PTE_PS != 0 {
            // A huge page OF THE WORKLOAD (PRIV) — its 2 MB must be returned; a 2M leaf
            // of the kernel (identity) is shared, we do not touch it. Behind a lazy reservation there are no
            // frames at all.
            if e2 & PTE_PRIV != 0 && (e2 & PTE_P != 0 || e2 & PTE_PNONE != 0) {
                let phys = e2 & ADDR_MASK;
                if crate::mem::rc_release(phys) { crate::mem::free_contig(phys, 512); }
            }
            continue;
        }
        if e2 & PTE_P == 0 { continue; }
        free_pt(e2 & ADDR_MASK);
    }
    crate::mem::free_contig(pd, 1);
}
/// Free the address space of a finished process: the private frames (COW/shared ones — by the
/// refcount), the split GB0 tables, the private region's tables, the PDPT/PML4. It must NOT be called
/// under this CR3 (it is called by the scheduler from another task, see sched::reap_dead_spaces).
/// The last roots `free_address_space` freed, with the caller and the task (the fault report: "is
/// this process running on a space already freed?" — ARM64-PLAN §11 item 0). One entry per
/// process exit, nothing on a hot path.
pub static FREED_ROOTS: [core::sync::atomic::AtomicU64; 16] = [const { core::sync::atomic::AtomicU64::new(0) }; 16];
static FREED_ROOT_LOC: [core::sync::atomic::AtomicPtr<core::panic::Location<'static>>; 16] =
    [const { core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()) }; 16];
static FREED_ROOT_N: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Where `root` was freed lately: (file:line, the core, the order of the free) — or None.
pub fn freed_root_find(root: u64, mut f: impl FnMut(&'static str, u32, u64, u64)) {
    use core::sync::atomic::Ordering::Relaxed;
    let n = FREED_ROOT_N.load(Relaxed);
    for k in 0..16u64.min(n) {
        let i = ((n - 1 - k) % 16) as usize;
        if FREED_ROOTS[i].load(Relaxed) & !0xFFF == root & !0xFFF {
            // SAFETY: P1 — null or a `&'static Location` stored by `free_address_space`.
            let Some(loc) = (unsafe { FREED_ROOT_LOC[i].load(Relaxed).as_ref() }) else { continue };
            f(loc.file(), loc.line(), FREED_ROOTS[i].load(Relaxed) & 0xFFF, n - 1 - k);
        }
    }
}
#[track_caller]
pub fn free_address_space(pml4: u64) {
    {
        use core::sync::atomic::Ordering::Relaxed;
        let k = (FREED_ROOT_N.fetch_add(1, Relaxed) % 16) as usize;
        let core = crate::percpu::cpu_id() as u64 & 0xFFF;
        FREED_ROOTS[k].store((pml4 & !0xFFF) | core, Relaxed);
        FREED_ROOT_LOC[k].store(core::ptr::from_ref(core::panic::Location::caller()).cast_mut(), Relaxed);
    }
    // THE WALK GOES UNDER THE REFERENCE KERNEL AS rather than under the current one.
    //
    // The tables are walked by their PHYSICAL addresses through the identity mapping. In
    // A WORKLOAD's address space the low region is its own copy of the
    // GB0 directory, and the workload itself is entitled to punch a hole in it with its `munmap`:
    // the virtual address 0x26eae000 is removed for it, while for the kernel at that same address there lies
    // THE PHYSICAL frame 653 MB that must be freed. The result is a #PF in ring 0 and
    // a stop of the machine.
    //
    // It was caught as "PostgreSQL stops accepting connections": a backend
    // finished, the scheduler cleaned up its AS from a tick — and brought the kernel down. From the outside
    // the server looked alive right up to the next access.
    //
    // In the reference kernel AS the identity is intact by construction, so we make the walk
    // under it and return CR3 back. The kernel's stack and code are mapped there — it is
    // the very AS the kernel started from.
    //
    // THE KERNEL'S SHARED TABLES ARE NOT OURS. A process gets a copy of the kernel PDPT (slot 0 of the PML4)
    // and copies of the other PML4 entries — the tables behind them are SHARED rather than its own. Previously
    // the walk considered ANY directory that is not a 1G leaf to be its own — and on EC2 that
    // was enough to bring the machine down: the ENA's LLQ window lies in the same gigabyte
    // as the NVMe's BAR, and `map_mmio_wc` splits that gigabyte in the kernel PDPT into
    // a directory and a table. The very first process that finished (with PostgreSQL that is the
    // startup process 0.3 s after the start) "freed" THE KERNEL's directory,
    // the frame went into the free list and was handed out again, while the next write into the NVMe's
    // doorbell (`nvme_batch`, BAR+0x1008) under any CR3 gave a #PF in ring 0.
    // It was not caught locally: without ENA the gigabyte stays a 1G leaf, and the
    // PostgreSQL gate with 30 backends passed. The rule: an entry EQUAL to the entry of
    // the reference kernel AS in the same slot is a shared one, we do not touch it (the gate
    // `talaria.test=sharedpd`).
    // arm64: the space leaves the ASID table first (a new space that gets this root's frame
    // must not find its ASID); the ASID itself is returned only after the broadcast flush below.
    #[cfg(target_arch = "aarch64")]
    let asid = mmu::asid_forget(pml4);
    let saved = current_cr3() & ADDR_MASK;
    let kern = kernel_cr3() & ADDR_MASK;
    let swapped = kern != 0 && kern != saved;
    // SAFETY: P1 + P4 + P5 — the table being freed must not be the one this core is running on —
    // that is why the current CR3 is swapped to the kernel's first.
    if swapped { unsafe { switch(kern); } }
    // SAFETY: P1 + P2 — the walk that collects the frames to free; no core walks this table any
    // more (the APs have been moved to the kernel's CR3).
    unsafe {
        let kpml4 = if kern != 0 { kptr::<Pte>(kern).cast_const() } else { core::ptr::null() };
        let kpdpt = if kpml4.is_null() { core::ptr::null() } else { kptr::<Pte>(pte_r(kpml4) & ADDR_MASK) };
        for l4 in 0..512usize {
            let e4 = pte_r(kptr::<Pte>(pml4).add(l4));
            if e4 & PTE_P == 0 { continue; }
            // A copied entry of the kernel's PML4: the PDPT behind it is shared entirely.
            if !kpml4.is_null() && e4 == pte_r(kpml4.add(l4)) { continue; }
            let pdpt = e4 & ADDR_MASK;
            for l3 in 0..512usize {
                let e3 = pte_r(kptr::<Pte>(pdpt).add(l3));
                if e3 & PTE_P == 0 || e3 & PTE_PS != 0 { continue; } // a 1G leaf of the kernel (GB1-3) — shared
                // A directory copied from the kernel PDPT (GB1-3, split for
                // a device's window) is shared. The process's GB0 PD is a copy, it has its own
                // entry, and it is freed together with the private region's PD.
                if l4 == 0 && !kpdpt.is_null() && e3 == pte_r(kpdpt.add(l3)) { continue; }
                free_pd(e3 & ADDR_MASK);
            }
            crate::mem::free_contig(pdpt, 1);
        }
        crate::mem::free_contig(pml4, 1);
        flush_tlb();
    }
    #[cfg(target_arch = "aarch64")]
    mmu::asid_free(asid);
    // SAFETY: P5 — the CR3 this core came in with is restored.
    if swapped { unsafe { switch(saved); } }
}
/// The address space's footprint: how much memory the process REALLY occupies.
///
/// It returns `(resident pages, of them huge 2M ones (in 4K pages),
/// shared with another AS, reserved lazily)`. It is computed BY A WALK OVER THE TABLES
/// rather than by a counter, and that is a conscious choice: a counter would have to be kept at EVERY
/// mapping and removal, that is, on the fault's path, for the sake of a line that the operator
/// reads once a second. The walk costs milliseconds and is paid for only when
/// `top`/`ps` have really asked.
///
/// We count only the process's PRIVATE pages (`PTE_PRIV`): the kernel's identity
/// mapping is shared by all the ASes, and without that filter every process's
/// "RSS" would equal the volume of the whole RAM. The upper half (l4 >= 256) is
/// the kernel's, we do not go there at all.
pub fn as_footprint(pml4: u64) -> (u64, u64, u64, u64) {
    let (mut res, mut huge, mut shared, mut lazy) = (0u64, 0u64, 0u64, 0u64);
    if pml4 == 0 { return (0, 0, 0, 0); }
    // SAFETY: P1 + P2 — a read-only walk over an address space (`top`/`ps` introspection).
    unsafe {
        for l4 in 0..256usize {
            let e4 = pte_r(kptr::<Pte>(pml4).add(l4));
            if e4 & PTE_P == 0 { continue; }
            let pdpt = e4 & ADDR_MASK;
            for l3 in 0..512usize {
                let e3 = pte_r(kptr::<Pte>(pdpt).add(l3));
                if e3 & PTE_P == 0 || e3 & PTE_PS != 0 { continue; } // a 1G leaf — shared with the kernel
                let pd = e3 & ADDR_MASK;
                for l2 in 0..512usize {
                    let e2 = pte_r(kptr::<Pte>(pd).add(l2));
                    if e2 & PTE_PS != 0 {
                        if e2 & PTE_PRIV == 0 { continue; }
                        if e2 & PTE_P != 0 { res += 512; huge += 512;
                            if crate::mem::rc_get(e2 & ADDR_MASK) > 1 { shared += 512; } }
                        else if e2 & PTE_LAZY != 0 { lazy += 512; }
                        continue;
                    }
                    if e2 & PTE_P == 0 {
                        if e2 & PTE_LAZY != 0 { lazy += 512; } // a lazy reservation of a whole table
                        continue;
                    }
                    let pt = e2 & ADDR_MASK;
                    for l1 in 0..512usize {
                        let e1 = pte_r(kptr::<Pte>(pt).add(l1));
                        if e1 & PTE_PRIV == 0 { continue; }
                        if e1 & PTE_P != 0 {
                            res += 1;
                            if crate::mem::rc_get(e1 & ADDR_MASK) > 1 { shared += 1; }
                        } else if e1 & PTE_LAZY != 0 { lazy += 1; }
                    }
                }
            }
        }
    }
    (res, huge, shared, lazy)
}

/// One run of `as_runs`: resident private pages at consecutive addresses with the same rights.
#[derive(Clone, Copy, Default)]
pub struct Run {
    pub start: u64,
    pub end: u64,
    /// Writable (RW, or COW — written into after a copy), executable.
    pub write: bool,
    pub exec: bool,
    /// In 4K pages: shared with another space (a COW refcount above one) or not, written or not.
    pub shared_clean: u64,
    pub shared_dirty: u64,
    pub private_clean: u64,
    pub private_dirty: u64,
}

/// The address space as `/proc/self/smaps` shows it, reconstructed from the tables: the
/// kernel keeps no list of mappings, so a "mapping" here is a run of RESIDENT private pages
/// with the same rights (a reserved, untouched range is not in it). The same walk and the
/// same filter as `as_footprint`; a 2M leaf counts as 512 pages with its one dirty bit.
pub fn as_runs(pml4: u64, mut emit: impl FnMut(&Run)) {
    if pml4 == 0 { return; }
    let mut cur: Option<Run> = None;
    let mut page = |va: u64, n: u64, e: u64, emit: &mut dyn FnMut(&Run)| {
        let write = e & (PTE_RW | PTE_COW) != 0;
        let exec = e & PTE_NX == 0;
        let shared = crate::mem::rc_get(e & ADDR_MASK) > 1;
        let dirty = e & PTE_D != 0;
        let joins = matches!(cur, Some(r) if r.end == va && r.write == write && r.exec == exec);
        if !joins {
            if let Some(r) = cur.take() { emit(&r); }
            cur = Some(Run { start: va, end: va, write, exec, ..Run::default() });
        }
        if let Some(r) = cur.as_mut() {
            r.end = va + n * 4096;
            match (shared, dirty) {
                (true, true) => r.shared_dirty += n, (true, false) => r.shared_clean += n,
                (false, true) => r.private_dirty += n, (false, false) => r.private_clean += n,
            }
        }
    };
    // SAFETY: P1 + P2 — a read-only walk over an address space, as `as_footprint`.
    unsafe {
        for l4 in 0..256usize {
            let e4 = pte_r(kptr::<Pte>(pml4).add(l4));
            if e4 & PTE_P == 0 { continue; }
            let pdpt = e4 & ADDR_MASK;
            for l3 in 0..512usize {
                let e3 = pte_r(kptr::<Pte>(pdpt).add(l3));
                if e3 & PTE_P == 0 || e3 & PTE_PS != 0 { continue; }
                let pd = e3 & ADDR_MASK;
                for l2 in 0..512usize {
                    let e2 = pte_r(kptr::<Pte>(pd).add(l2));
                    let va2 = ((l4 as u64) << 39) | ((l3 as u64) << 30) | ((l2 as u64) << 21);
                    if e2 & PTE_P == 0 { continue; }
                    if e2 & PTE_PS != 0 {
                        if e2 & PTE_PRIV != 0 { page(va2, 512, e2, &mut emit); }
                        continue;
                    }
                    let pt = e2 & ADDR_MASK;
                    for l1 in 0..512usize {
                        let e1 = pte_r(kptr::<Pte>(pt).add(l1));
                        if e1 & PTE_PRIV == 0 || e1 & PTE_P == 0 { continue; }
                        page(va2 | ((l1 as u64) << 12), 1, e1, &mut emit);
                    }
                }
            }
        }
    }
    if let Some(r) = cur { emit(&r); }
}

/// The translation vaddr->phys by the active CR3 (for DMA under a non-identity AS, e.g. a COW process).
/// Not mapped -> return the vaddr (an identity fallback). It takes the 1G/2M/4K leaves into account.
pub fn virt_to_phys(vaddr: u64) -> u64 {
    // SAFETY: P1 + P2 — a read-only walk of the current AS — a DMA address under a non-identity
    // space.
    unsafe {
        let l4 = ((vaddr >> 39) & 0x1FF) as usize; let l3 = ((vaddr >> 30) & 0x1FF) as usize;
        let l2 = ((vaddr >> 21) & 0x1FF) as usize; let l1 = ((vaddr >> 12) & 0x1FF) as usize;
        let e4 = pte_r(cr3_root().add(l4)); if e4 & PTE_P == 0 { return vaddr; }
        let e3 = pte_r(kptr::<Pte>(e4 & ADDR_MASK).add(l3)); if e3 & PTE_P == 0 { return vaddr; }
        if e3 & PTE_PS != 0 { return (e3 & 0x000F_FFFF_C000_0000) | (vaddr & 0x3FFF_FFFF); }
        let e2 = pte_r(kptr::<Pte>(e3 & ADDR_MASK).add(l2)); if e2 & PTE_P == 0 { return vaddr; }
        if e2 & PTE_PS != 0 { return (e2 & 0x000F_FFFF_FFE0_0000) | (vaddr & 0x1F_FFFF); }
        let e1 = pte_r(kptr::<Pte>(e2 & ADDR_MASK).add(l1)); if e1 & PTE_P == 0 { return vaddr; }
        (e1 & ADDR_MASK) | (vaddr & 0xFFF)
    }
}
