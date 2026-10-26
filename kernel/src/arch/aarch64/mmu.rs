//! The AArch64 MMU for `paging.rs`: the descriptor translation (`enc`/`dec`), the
//! root register, the TLB, and the early identity map the kernel runs on until
//! `paging::init` builds its own tables (docs/ARM64-PLAN.md §4.3).
//!
//! ## The canonical format and the hardware's
//!
//! `paging.rs` computes in the CANONICAL descriptor format — x86-64's: present bit 0,
//! RW bit 1, U/S bit 2, PWT/PCD bits 3-4 (together: device memory), the large-leaf bit
//! 7, NX bit 63, the OS bits COW/PNONE 9, PRIV 10, LAZY 11, GUARD 55 (in present
//! entries) and LAZY_W/X/4K 52-54 (in non-present ones). Every write into a table goes
//! through `enc`, every read through `dec`, so the hardware sees the AArch64 stage-1
//! descriptor and the module sees what it wrote:
//!
//! | canonical | AArch64 |
//! |---|---|
//! | P (bit 0) | VALID (bit 0); a non-present entry is stored AS IS — the hardware ignores its other bits |
//! | no PS in a present entry | bit 1 = 1: a table at levels 0-2, a page at level 3 — the same encoding, which is why no level is needed |
//! | PS | bit 1 = 0: a block at levels 1-2 |
//! | RW | AP[2] (bit 7) = 0; read-only = AP[2] = 1 |
//! | US | AP[1] (bit 6) = 1 (EL0 access), and PXN (bit 53) — the kernel never executes a user page, x86's SMEP |
//! | NX | UXN (bit 54); on a kernel page also PXN |
//! | PWT or PCD | MAIR index 1 (Device-nGnRE); otherwise index 0 (Normal write-back, inner shareable) |
//! | bits 9, 10, 11, 55 | the software bits 55, 56, 57, 58 (the hardware never reads them) |
//! | A (5), D (6), G (8) | A and G dropped: AF is always set by us, everything is nG; D — below |
//!
//! Every present entry also carries AF (the access flag — there is no hardware walk to
//! set it) and nG (not global: translations are per ASID, which is how x86 without
//! PCID behaves too — a root switch drops them).
//!
//! ## The dirty bit
//!
//! x86 sets D (bit 6) in hardware on the first write and `paging::take_dirty` clears it;
//! `ext2`'s mmap writeback lives on that. AArch64 has no dirty bit as such, but with
//! FEAT_HAFDBS (`TCR.HD = 1`) it has the DBM scheme: a page with DBM (bit 51) set and
//! AP[2] = 1 is WRITABLE, only read-only until its first write, and the hardware clears
//! AP[2] on that write without a fault. So a canonical RW page without D is encoded as
//! DBM + AP[2] = 1, a dirty one as DBM + AP[2] = 0, and `dec` reads RW from DBM and D from
//! AP[2] — exactly x86's semantics, `take_dirty` re-arms the page by clearing D. Without
//! the feature (`cpu::has_hw_dirty` false) the fallback is CONSERVATIVE: every writable
//! page decodes as dirty, so a writeback never misses a write; it only does more of them.

use super::cpu;

const VALID: u64 = 1 << 0;
const TABLE_OR_PAGE: u64 = 1 << 1;
const ATTR_SHIFT: u32 = 2;
const AP1_EL0: u64 = 1 << 6;
const AP2_RO: u64 = 1 << 7;
const SH_INNER: u64 = 0b11 << 8;
const AF: u64 = 1 << 10;
const NG: u64 = 1 << 11;
const DBM: u64 = 1 << 51;
/// The Contiguous hint of a level-3 page: sixteen aligned entries that map sixteen
/// consecutive frames with the same attributes share one TLB entry (`paging::cont_fold`).
const CONT: u64 = 1 << 52;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
/// The output address of a 4K-granule descriptor: bits 47:12. NOT x86's 51:12 — bit 51 is
/// DBM here, and a mask that swallowed it once turned a frame number into a data abort.
const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

// The canonical bits (paging.rs).
const C_P: u64 = 1 << 0;
const C_RW: u64 = 1 << 1;
const C_US: u64 = 1 << 2;
const C_PWT: u64 = 1 << 3;
const C_PCD: u64 = 1 << 4;
const C_D: u64 = 1 << 6;
const C_PS: u64 = 1 << 7;
const C_NX: u64 = 1 << 63;
const C_SW9: u64 = 1 << 9;
const C_SW10: u64 = 1 << 10;
const C_SW11: u64 = 1 << 11;
const C_SW55: u64 = 1 << 55;
/// The canonical Contiguous bit of a present entry (`paging::PTE_CONT`): bit 56, because
/// the hardware's own bit 52 is LAZY_W in the canonical format (a non-present entry's).
const C_CONT: u64 = 1 << 56;
// Where they live in a hardware descriptor: the software-defined bits 55..58.
const H_SW9: u64 = 1 << 55;
const H_SW10: u64 = 1 << 56;
const H_SW11: u64 = 1 << 57;
const H_SW55: u64 = 1 << 58;
/// The dirty state of a READ-ONLY page. The scheme above keeps D in AP[2], which a read-only
/// page needs for its protection, so write-protecting a written page (fork's COW, an
/// `mprotect(PROT_READ)`) dropped D — the bug Linux fixed in ff1712f953e2 ("arm64: pgtable:
/// Ensure dirty bit is preserved across pte_wrprotect()"), which Redis tests for at start-up
/// and refused to run on. On a MAP_SHARED page it lost the write's writeback. Bit 59 is a
/// leaf's PBHA[0], IGNORED by the walker while TCR.HWU059 is clear (`tcr_value`). Only a
/// read-only entry carries it; table entries are always made writable, where bit 59 would
/// be PXNTable.
const H_SWD: u64 = 1 << 59;

/// The MAIR indices (`MAIR_VALUE` below). Attr0 = Normal write-back (RAM), Attr1 =
/// Device-nGnRE (MMIO), Attr2 = Normal non-cacheable (the write-combining analogue: the
/// ENA LLQ window — not reachable through the canonical format yet).
pub const ATTR_NORMAL: u64 = 0;
pub const ATTR_DEVICE: u64 = 1;
pub const ATTR_NC: u64 = 2;
const MAIR_VALUE: u64 = 0xFF | (0x04 << 8) | (0x44 << 16);

/// Canonical -> hardware.
#[inline(always)]
pub fn enc(c: u64) -> u64 {
    if c & C_P == 0 { return c; }
    let mut h = (c & ADDR_MASK) | VALID | AF | NG;
    if c & C_PS == 0 { h |= TABLE_OR_PAGE; }
    if c & C_RW == 0 { h |= AP2_RO; if c & C_D != 0 { h |= H_SWD; } }
    else if hw_dirty() { h |= DBM; if c & C_D == 0 { h |= AP2_RO; } } // writable; read-only until the first write
    if c & C_US != 0 { h |= AP1_EL0 | PXN; }
    if c & C_NX != 0 { h |= UXN; if c & C_US == 0 { h |= PXN; } }
    if c & (C_PWT | C_PCD) != 0 { h |= ATTR_DEVICE << ATTR_SHIFT; } else { h |= SH_INNER; }
    if c & C_SW9 != 0 { h |= H_SW9; }
    if c & C_SW10 != 0 { h |= H_SW10; }
    if c & C_SW11 != 0 { h |= H_SW11; }
    if c & C_SW55 != 0 { h |= H_SW55; }
    if c & C_CONT != 0 { h |= CONT; }
    h
}

/// Hardware -> canonical.
#[inline(always)]
pub fn dec(h: u64) -> u64 {
    if h & VALID == 0 { return h; }
    let mut c = (h & ADDR_MASK) | C_P;
    if h & TABLE_OR_PAGE == 0 { c |= C_PS; }
    if h & DBM != 0 { c |= C_RW; if h & AP2_RO == 0 { c |= C_D; } }   // tracked: written since the last take_dirty
    else if h & AP2_RO == 0 { c |= C_RW; if !hw_dirty() { c |= C_D; } } // untracked: conservatively dirty
    else if h & H_SWD != 0 { c |= C_D; }                                // read-only, written before it was protected
    if h & AP1_EL0 != 0 { c |= C_US; }
    if h & UXN != 0 { c |= C_NX; }
    if (h >> ATTR_SHIFT) & 0x7 == ATTR_DEVICE { c |= C_PWT | C_PCD; }
    if h & H_SW9 != 0 { c |= C_SW9; }
    if h & H_SW10 != 0 { c |= C_SW10; }
    if h & H_SW11 != 0 { c |= C_SW11; }
    if h & H_SW55 != 0 { c |= C_SW55; }
    if h & CONT != 0 { c |= C_CONT; }
    c
}

/// Whether the dirty state is managed by the hardware (decided once, at the early map).
static HW_DIRTY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
#[inline(always)] fn hw_dirty() -> bool { HW_DIRTY.load(core::sync::atomic::Ordering::Relaxed) }
/// A 4K leaf re-typed as write-combining: Normal non-cacheable (MAIR index 2), outer
/// shareable — what a device's LLQ window wants (docs/ARM64-PLAN.md §4.11).
pub fn with_wc(desc: u64) -> u64 { (desc & !(0x7 << ATTR_SHIFT) & !(0x3 << 8)) | (ATTR_NC << ATTR_SHIFT) | (0x2 << 8) }
/// The memory attribute table, the arm64 twin of the PAT (`talaria.test=wc` prints it).
pub fn pat_read() -> u64 { MAIR_VALUE }
/// What `pat_read` must show: the MAIR as `enable` wrote it.
pub const PAT_VALUE: u64 = MAIR_VALUE;

pub fn hw_dirty_on() -> bool { hw_dirty() }

/// The physical address of the active root table (`TTBR0_EL1` without the ASID).
#[inline(always)]
pub fn root() -> u64 { cpu::ttbr0_el1() & ADDR_MASK }

// ── ASIDs: a space's translations survive a switch (ARM64-PLAN §4.3, §12 item 3) ─────────
//
// Every process space gets its own 16-bit ASID (`TCR.AS = 1`, written into `TTBR0[63:48]`), so
// a switch between two of them is `msr ttbr0_el1; isb` with no invalidation — x86's PCID, which
// x86 Talaria never used. Before, every space shared ASID 0 and EVERY switch was a full
// `tlbi vmalle1`: the PostgreSQL server's waiters rotate between processes every few
// microseconds (~170 000 switches a second per core on the board's stand), each one entered
// the next process with a cold TLB — the kernel's translations included, everything is nG —
// and the slow state of the server ran its user code at IPC 0.53-0.62.
//
// WHAT KEEPS IT CORRECT. The kernel was written for x86 without PCID, where a space that is not
// running on a core holds nothing in that core's TLB: the root switch dropped it. With ASIDs a
// core keeps the translations of every space that ran on it. The broadcast invalidations
// (`flush_page`, `flush_range_is`, `flush_all_is`: `tlbi ...is` over all ASIDs) already reach
// every core; the one LOCAL operation, `paging::flush_tlb`, becomes the broadcast full flush
// while ASIDs are on (`asid_on`), so whatever the switch used to drop is dropped at once
// everywhere. A space's ASID goes back to the pool only after `free_address_space`'s flush
// (broadcast), so a reused ASID finds nothing of its previous owner. A root the table does not
// know (the kernel's own, a space created before the table) runs on ASID 0 with the old
// sequence — a full invalidation on entering AND on leaving ASID 0 — so nothing of a stranger is
// ever tagged with a live ASID. `talaria.asid=0` is the old behaviour throughout (the A/B).
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

/// ASIDs on (`talaria.asid=0` switches them off at boot, before the first process).
pub static ASID_ON: AtomicBool = AtomicBool::new(true);
/// The table root -> ASID: open addressing, linear probing, backward-shift deletion. Writers
/// take `ASID_LOCK`; readers are lock-free under the sequence `ASID_SEQ` and fall back to
/// ASID 0 (always safe) when a writer moved under them.
const ASID_SLOTS: usize = 8192;
/// At most this many live spaces carry an ASID (the rest run on ASID 0, correctly and slower).
const ASID_MAX: usize = 4096;
static ASID_KEY: [AtomicU64; ASID_SLOTS] = [const { AtomicU64::new(0) }; ASID_SLOTS];
static ASID_VAL: [AtomicU16; ASID_SLOTS] = [const { AtomicU16::new(0) }; ASID_SLOTS];
static ASID_SEQ: AtomicU64 = AtomicU64::new(0);
static ASID_LOCK: AtomicBool = AtomicBool::new(false);
/// The ASIDs in use (bit `a` — ASID `a`; ASID 0 is never handed out).
static ASID_USED: [AtomicU64; ASID_MAX / 64] = [const { AtomicU64::new(0) }; ASID_MAX / 64];
/// For `stat:` — spaces given an ASID, spaces that found the pool empty, switches that kept
/// the TLB, switches that had to invalidate.
pub static ASID_GIVEN: AtomicU64 = AtomicU64::new(0);
pub static ASID_EMPTY: AtomicU64 = AtomicU64::new(0);
pub static SWITCH_KEPT: AtomicU64 = AtomicU64::new(0);
pub static SWITCH_FLUSHED: AtomicU64 = AtomicU64::new(0);

#[inline(always)]
pub fn asid_on() -> bool { ASID_ON.load(Ordering::Relaxed) }

#[inline(always)]
fn asid_hash(root: u64) -> usize {
    (((root >> 12).wrapping_mul(0x9E37_79B9_7F4A_7C15)) >> (64 - 13)) as usize & (ASID_SLOTS - 1)
}

fn asid_lock() {
    while ASID_LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
    // odd: a writer is inside
    ASID_SEQ.fetch_add(1, Ordering::AcqRel);
}
fn asid_unlock() {
    ASID_SEQ.fetch_add(1, Ordering::AcqRel);
    ASID_LOCK.store(false, Ordering::Release);
}

/// The ASID of `root` (0 — none: the kernel's root, an unknown one, ASIDs off).
#[inline]
pub fn asid_of(root: u64) -> u64 {
    if !asid_on() { return 0; }
    let key = root & ADDR_MASK;
    if key == 0 { return 0; }
    let s0 = ASID_SEQ.load(Ordering::Acquire);
    if s0 & 1 != 0 { return 0; }
    let mut i = asid_hash(key);
    let mut a = 0u64;
    for _ in 0..ASID_SLOTS {
        let k = ASID_KEY[i].load(Ordering::Acquire);
        if k == 0 { break; }
        if k == key { a = ASID_VAL[i].load(Ordering::Acquire) as u64; break; }
        i = (i + 1) & (ASID_SLOTS - 1);
    }
    if ASID_SEQ.load(Ordering::Acquire) != s0 { return 0; }
    a
}

/// The `TTBR0_EL1` value for `root`: the table's address and its ASID.
#[inline]
pub fn ttbr_of(root: u64) -> u64 { (root & ADDR_MASK) | (asid_of(root) << 48) }

/// Give the new space `root` an ASID (`paging::new_address_space`/`new_process_space`, before the
/// space is ever loaded). No ASID left — the space runs on ASID 0, correctly.
pub fn asid_assign(root: u64) {
    if !asid_on() { return; }
    let key = root & ADDR_MASK;
    asid_lock();
    let mut a = 0usize;
    'find: for w in 0..ASID_MAX / 64 {
        let v = ASID_USED[w].load(Ordering::Relaxed);
        let free = !v & if w == 0 { !1u64 } else { !0u64 };
        if free != 0 {
            let b = free.trailing_zeros() as usize;
            ASID_USED[w].store(v | (1 << b), Ordering::Relaxed);
            a = w * 64 + b;
            break 'find;
        }
    }
    if a == 0 {
        asid_unlock();
        ASID_EMPTY.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let mut i = asid_hash(key);
    loop {
        let k = ASID_KEY[i].load(Ordering::Relaxed);
        if k == 0 || k == key { break; }
        i = (i + 1) & (ASID_SLOTS - 1);
    }
    ASID_VAL[i].store(a as u16, Ordering::Release);
    ASID_KEY[i].store(key, Ordering::Release);
    asid_unlock();
    ASID_GIVEN.fetch_add(1, Ordering::Relaxed);
}

/// Take `root` out of the table (the space is being freed; nobody runs it). The ASID is returned
/// separately, by `asid_free`, AFTER the broadcast flush — until then it is reserved.
pub fn asid_forget(root: u64) -> u64 {
    if !asid_on() { return 0; }
    let key = root & ADDR_MASK;
    asid_lock();
    let mut i = asid_hash(key);
    let mut a = 0u64;
    for _ in 0..ASID_SLOTS {
        let k = ASID_KEY[i].load(Ordering::Relaxed);
        if k == 0 { break; }
        if k == key {
            a = ASID_VAL[i].load(Ordering::Relaxed) as u64;
            // backward-shift deletion: the entries after the hole that hash at or before it move up
            let mut hole = i;
            let mut j = (i + 1) & (ASID_SLOTS - 1);
            loop {
                let kj = ASID_KEY[j].load(Ordering::Relaxed);
                if kj == 0 { break; }
                let h = asid_hash(kj);
                // is `h` cyclically outside (hole, j]? then the entry may fill the hole
                let outside = if hole <= j { h <= hole || h > j } else { h <= hole && h > j };
                if outside {
                    ASID_VAL[hole].store(ASID_VAL[j].load(Ordering::Relaxed), Ordering::Release);
                    ASID_KEY[hole].store(kj, Ordering::Release);
                    hole = j;
                }
                j = (j + 1) & (ASID_SLOTS - 1);
            }
            ASID_KEY[hole].store(0, Ordering::Release);
            ASID_VAL[hole].store(0, Ordering::Release);
            break;
        }
        i = (i + 1) & (ASID_SLOTS - 1);
    }
    asid_unlock();
    a
}

/// Return an ASID to the pool — only after a broadcast invalidation that covered it.
pub fn asid_free(a: u64) {
    if a == 0 || a as usize >= ASID_MAX { return; }
    ASID_USED[a as usize / 64].fetch_and(!(1u64 << (a % 64)), Ordering::AcqRel);
}

/// Make `root` the active root table. Two spaces with their own ASIDs: the register write
/// alone, the old space's translations stay tagged with its ASID. ASID 0 on either side (the
/// kernel's root, an unknown one, ASIDs off): as a CR3 write on x86 without PCID — this core's
/// translations are dropped.
#[inline(always)]
pub unsafe fn set_root(root: u64) {
    let new = ttbr_of(root);
    if new >> 48 != 0 && cpu::ttbr0_el1() >> 48 != 0 {
        SWITCH_KEPT.fetch_add(1, Ordering::Relaxed);
        core::arch::asm!("msr ttbr0_el1, {}", "isb", in(reg) new, options(nostack));
        return;
    }
    SWITCH_FLUSHED.fetch_add(1, Ordering::Relaxed);
    // ASID 0 is shared by every space without its own, so the old space's translations must be gone
    // BEFORE the new root can be walked — and any that a speculative walk brings in during
    // the switch, after it: the invalidate on both sides, completed (`dsb ish`, not `nsh`:
    // the walks are inner-shareable), and an `isb` so that nothing fetched under the old
    // regime is used. With the single trailing invalidate the first instruction of a
    // process at 0x50002190 read, one boot in three under KVM, the ZEROS the kernel's
    // identity mapping of that same address holds — an UNDEF at `_start`.
    core::arch::asm!(
        "dsb ishst",
        "tlbi vmalle1",
        "dsb ish",
        "msr ttbr0_el1, {}",
        "isb",
        "tlbi vmalle1",
        "dsb ish",
        "isb",
        in(reg) new, options(nostack));
}

/// Drop every translation this core has cached. The `dsb` first: the entry changed before
/// the call is a store, and a walk after the invalidation must not find the old one in a
/// store buffer the `tlbi` overtook (the Arm ARM's break-before-make sequence).
#[inline(always)]
/// Leaves written to tables no TLB holds (new, never-mapped pages) become visible to another
/// core's walker after `dsb ishst` (the stores complete in the inner-shareable domain); `isb`
/// for this core's own next access. No `tlbi`: there is nothing stale to drop.
pub fn tables_published() {
    // SAFETY: barriers only.
    unsafe { core::arch::asm!("dsb ishst", "isb", options(nostack, preserves_flags)); }
}
pub unsafe fn flush_all() { core::arch::asm!("dsb nshst", "tlbi vmalle1", "dsb nsh", "isb", options(nostack)); }

/// Drop every translation on EVERY core (`IS`): what a flush too large to go page by page
/// is on a machine where the hardware broadcasts (`paging::shootdown_impl`). Before it the
/// large range and the "everything" flush were `flush_all` — this core only — and the
/// others kept the old entries of a large `munmap` over frames already back in the list.
#[inline(always)]
pub unsafe fn flush_all_is() { core::arch::asm!("dsb ishst", "tlbi vmalle1is", "dsb ish", "isb", options(nostack)); }

/// Drop the translation of one page — on EVERY core: `IS` broadcasts to the
/// inner-shareable domain, which is what makes `paging::shootdown_impl` a plain flush here.
/// `dsb ishst` first, so that another core's walker cannot refill the old entry after the
/// invalidation from a store not yet visible.
/// Drop the translations of a RANGE on every core with ONE pair of barriers: the `tlbi`s are
/// queued back to back and a single `dsb ish` waits for the whole broadcast (Linux's
/// `__flush_tlb_range`). Page by page, `flush_page` paid a `dsb ish` per page — on a
/// Graviton ~400 ns each, and PostgreSQL's `munmap` of 14 pages per query stood 5.6 us in
/// the flush and the same again in the "shootdown" that is this same flush (ARM64-PLAN §12
/// item 3).
#[inline(always)]
pub unsafe fn flush_range_is(start: u64, pages: u64) {
    core::arch::asm!("dsb ishst", options(nostack, preserves_flags));
    let mut a = start >> 12;
    for _ in 0..pages {
        core::arch::asm!("tlbi vaae1is, {}", in(reg) a, options(nostack, preserves_flags));
        a += 1;
    }
    core::arch::asm!("dsb ish", "isb", options(nostack, preserves_flags));
}

#[inline(always)]
pub unsafe fn flush_page(vaddr: u64) {
    core::arch::asm!("dsb ishst", "tlbi vaae1is, {}", "dsb ish", "isb", in(reg) vaddr >> 12, options(nostack));
}

/// 1 GiB blocks exist at level 1 with the 4K granule on every AArch64 core.
pub fn has_1g() -> bool { true }

/// Nothing to enable: the canonical U/S maps to AP[1] + PXN in `enc`, so the kernel
/// never executes a user page (x86's SMEP) by construction; `true` says so to the caller.
pub unsafe fn init_hw() -> bool { true }

/// What each of the four identity gigabytes is (`paging::init`): the kernel's gigabyte is
/// RAM by 2M leaves with the user identity region; any other gigabyte is RAM if the
/// firmware's map puts RAM in it, devices otherwise. On `virt` that reads as before (GB0
/// the GIC, the UART, virtio-mmio, the low ECAM; GB1 the kernel; GB2-3 RAM when the guest
/// has it); on a Graviton (c7g.large's firmware map) GB0 is devices too (the GIC at
/// 0x1000_0000, the ECAM at 0x2000_0000), RAM starts at 1 GiB and GB2 is the PCI window with the
/// 16550 — a fixed "only GB0 is devices" would have cached the console's registers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GbKind { Device, User2M, Ram1G }
pub fn gb_kind(gb: usize, kernel_gb: usize) -> GbKind {
    if gb == kernel_gb { return GbKind::User2M; }
    let (lo, hi) = ((gb as u64) << 30, ((gb as u64) + 1) << 30);
    for i in 0..crate::mem::ram_regions() {
        let (b, l) = crate::mem::ram_region(i);
        if l != 0 && b < hi && b + l > lo { return GbKind::Ram1G; }
    }
    GbKind::Device
}

// ── The early identity map: what the kernel runs on until `paging::init` ─────────────
#[repr(C, align(4096))]
struct Table([u64; 512]);
/// Level 0 (one entry: the first 512 GiB) and level 1 (1 GiB blocks) — a 4-level tree with
/// `T0SZ = 16`, the same shape `paging.rs` builds, so switching to its tables changes
/// the root and nothing else.
static mut L0: Table = Table([0; 512]);
static mut L1: Table = Table([0; 512]);
/// The level-1 table of the devices' alias (`DEV_SLOT`): entry 0 is GB0's Device block.
static mut L1DEV: Table = Table([0; 512]);

// ── The devices' alias: the kernel reaches GB0's devices away from the low addresses ──
//
// Every process of this kernel shares the kernel's identity map, and on QEMU `virt` its first
// gigabyte is the devices (the GIC, the UART, virtio-mmio, the 32-bit PCIe window). A workload
// whose image wants those addresses — ClickHouse's arm64 ET_EXEC is linked at 2 MB..458 MB —
// can have them only if the kernel itself does not touch GB0 through its identity. So the
// kernel touches the devices of GB0 through an alias: the same 1 GiB Device block once more,
// in an L0 slot above every user range (`USER_TOP` is 1 << 47; the slot is 384, VA
// 0xC000_0000_0000 + PA). The slot is in the early map and in the kernel's root before the
// first address space exists, and `copy_kernel_pml4` hands every process the same L1.
// `mmio_va` is applied where a device's base is FIRST read (the tree, ACPI, a BAR); an address
// the DEVICE sees (an MSI doorbell, an ITS command's redistributor) stays physical (`mmio_pa`).
pub const DEV_SLOT: usize = 384;
pub const DEV_BASE: u64 = (DEV_SLOT as u64) << 39;
const _: () = assert!(DEV_BASE == crate::mem::MWIN_BASE); // the alias IS the devices' window's gigabyte 0
/// The kernel's pointer to a device register at physical `pa`: the devices' window
/// (`paging::mmio_va` — GB0 is window gigabyte 0 from the moment the MMU is on, any other
/// device gigabyte once `paging::mwin_init` has run), the identity otherwise.
#[inline]
pub fn mmio_va(pa: u64) -> u64 { crate::paging::mmio_va(pa) }
/// The physical address behind a pointer `mmio_va` gave (what a device is told).
#[inline]
pub fn mmio_pa(va: u64) -> u64 { crate::paging::mmio_pa(va) }
/// The alias's L1 entry for `paging::init`'s root: GB0 as a Device block, supervisor.
pub fn dev_alias_l1_entry() -> u64 { device_block(0) }

// What `boot.rs`'s early map (stage 7.3: the MMU on before any Rust) writes: the same block
// encodings, MAIR and TCR as this file's, so the switch to `early_enable`'s tables changes the
// root and (HA/HD) nothing else. The IPS field is the core's PARange, set by the asm.
pub const BOOT_DEVICE_BLOCK: u64 = device_block(0);
pub const BOOT_NORMAL_BLOCK: u64 = normal_block(0);
pub const BOOT_MAIR: u64 = MAIR_VALUE;
pub const BOOT_TCR: u64 = 16 | (1 << 8) | (1 << 10) | (0b11 << 12) | (1 << 23) | (1 << 36);
/// The image's gigabyte at its high alias (stage 7): L0[511] -> this L1, entry 510.
static mut L1HI: Table = Table([0; 512]);

/// A 1 GiB Normal RAM block for the kernel alone (no EL0 access).
const fn normal_block(pa: u64) -> u64 { pa | VALID | (ATTR_NORMAL << ATTR_SHIFT) | SH_INNER | AF | UXN }
/// A 1 GiB Device block, never executed.
const fn device_block(pa: u64) -> u64 { pa | VALID | (ATTR_DEVICE << ATTR_SHIFT) | AF | UXN | PXN }

/// Build the early tables and turn the MMU on. Block 0 is the devices; every 1 GiB block a
/// RAM range of `rams` (base, size) touches becomes a Normal block — mapping RAM that is
/// not there is harmless while nothing touches it. `devs` are the devices the machine's
/// description named (the console, the GIC, the ECAM): a block of theirs that holds no RAM
/// is a Device block — on QEMU `virt` everything sits below 1 GiB, a Graviton's firmware
/// puts its 16550 at 0xE2F0_0000 in a gigabyte with nothing else, and the first line would
/// otherwise go through a cacheable mapping (or fault, with no mapping at all). A device in
/// a gigabyte that also holds RAM is left Normal: 1 GiB is this map's whole resolution. The
/// first 512 GiB only: a device above that (a Graviton's GIC) is `paging`'s to map.
///
/// # Safety
/// Once, on the boot core, from `kmain`, before any other core exists. The MMU is already on
/// with `boot.rs`'s early map (stage 7.3): the kernel runs at its high alias, which both maps
/// carry, so the switch keeps the code under it.
pub unsafe fn early_enable(rams: &[(u64, u64)], devs: &[u64]) {
    let l1 = &mut *core::ptr::addr_of_mut!(L1);
    l1.0[0] = device_block(0);
    for &(base, size) in rams {
        if size == 0 { continue; }
        let first = (base >> 30) as usize;
        let last = ((base + size - 1) >> 30) as usize;
        for i in first.max(1)..=last.min(511) { l1.0[i] = normal_block((i as u64) << 30); }
    }
    for &d in devs {
        let i = (d >> 30) as usize;
        if d != 0 && i < 512 && l1.0[i] == 0 { l1.0[i] = device_block((i as u64) << 30); }
    }
    let l0 = &mut *core::ptr::addr_of_mut!(L0);
    l0.0[0] = crate::mem::kpa_of(core::ptr::addr_of!(L1)) | VALID | TABLE_OR_PAGE;
    // the devices' alias (see DEV_SLOT): GB0's Device block once more, high
    (*core::ptr::addr_of_mut!(L1DEV)).0[0] = device_block(0);
    l0.0[DEV_SLOT] = crate::mem::kpa_of(core::ptr::addr_of!(L1DEV)) | VALID | TABLE_OR_PAGE;
    // The image's high alias (stage 7): the kernel runs there — this switch must keep it.
    let img_gb = crate::mem::kernel_lo() & !((1u64 << 30) - 1);
    let (s4, s3) = (((crate::mem::KIMG_VBASE >> 39) & 0x1FF) as usize, ((crate::mem::KIMG_VBASE >> 30) & 0x1FF) as usize);
    (*core::ptr::addr_of_mut!(L1HI)).0[s3] = normal_block(img_gb);
    l0.0[s4] = crate::mem::kpa_of(core::ptr::addr_of!(L1HI)) | VALID | TABLE_OR_PAGE;
    // The intermediate physical address size: what the core supports, capped at 48 bits
    // (the encodings of PARange and TCR.IPS coincide up to that point).
    HW_DIRTY.store(cpu::has_hw_dirty(), core::sync::atomic::Ordering::Relaxed);
    let tcr = tcr_value();
    core::arch::asm!(
        "dsb ishst",            // the tables' stores before the walker may use them (the MMU is on)
        "msr mair_el1, {mair}",
        "msr tcr_el1, {tcr}",
        "msr ttbr0_el1, {ttbr}",
        "isb",
        "tlbi vmalle1",
        "dsb ish",
        "isb",
        mair = in(reg) MAIR_VALUE, tcr = in(reg) tcr, ttbr = in(reg) crate::mem::kpa_of(core::ptr::addr_of!(L0)),
        options(nostack),
    );
    enable_here();
    crate::mem::mwin_publish(0, 0); // GB0 is the devices' window gigabyte 0 (L1DEV above)
    super::uart::rebase();
}

/// Turn the MMU on for THIS core with the table the boot core built (`early_enable`):
/// a secondary core comes up with the MMU off. SCTLR: M (the MMU), C (the data caches), I
/// (the instruction cache); the RES1 bits are whatever the core came up with —
/// read-modify-write, never a constant.
pub unsafe fn enable_here() {
    // The same MAIR/TCR/TTBR as the boot core (a secondary's registers start undefined).
    let tcr = tcr_value();
    core::arch::asm!(
        "dsb ishst",            // the tables' stores before the walker may use them (the MMU is on)
        "msr mair_el1, {mair}",
        "msr tcr_el1, {tcr}",
        "msr ttbr0_el1, {ttbr}",
        "isb",
        "tlbi vmalle1",
        "dsb ish",
        "isb",
        mair = in(reg) MAIR_VALUE, tcr = in(reg) tcr, ttbr = in(reg) root_for_new_core(),
        options(nostack),
    );
    let sctlr = cpu::sctlr_el1() | (1 << 0) | (1 << 2) | (1 << 12);
    core::arch::asm!("msr sctlr_el1, {}", "isb", in(reg) sctlr, options(nostack));
}

/// `TCR_EL1` for every core: a 48-bit VA in four levels (as x86's PML4), cacheable
/// inner-shareable walks, no TTBR1, the supported physical address size, 16-bit ASIDs,
/// and — where the core has FEAT_HAFDBS — the hardware access flag and dirty state.
fn tcr_value() -> u64 {
    // The intermediate physical address size: what the core supports, capped at 48 bits
    // (the encodings of PARange and TCR.IPS coincide up to that point).
    let parange = (cpu::id_aa64mmfr0() & 0xF).min(5);
    let mut tcr: u64 = 16                // T0SZ: a 48-bit VA, four levels — as x86's PML4
        | (1 << 8) | (1 << 10)           // IRGN0/ORGN0: the walks are write-back cacheable
        | (0b11 << 12)                   // SH0: inner shareable
        | (1 << 23)                      // EPD1: no TTBR1 walks
        | (parange << 32)                // IPS
        | (1 << 36);                     // AS: 16-bit ASIDs
    if hw_dirty() { tcr |= (1 << 39) | (1 << 40); } // HA | HD
    tcr
}

/// The TCR a core coming up should use (`ap_start`'s asm turns its MMU on with it).
pub fn tcr_for_new_core() -> u64 { tcr_value() }

/// The root a core coming up should use: `paging::init`'s table once it exists, the early one before.
pub fn root_for_new_core() -> u64 {
    let k = crate::paging::kernel_cr3();
    if k != 0 { k } else { crate::mem::kpa_of(core::ptr::addr_of!(L0)) }
}
