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
//! | A (5), D (6), G (8) | dropped: AF is always set by us, there is no dirty tracking (`take_dirty` never fires), everything is nG |
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
// Where they live in a hardware descriptor: the software-defined bits 55..58.
const H_SW9: u64 = 1 << 55;
const H_SW10: u64 = 1 << 56;
const H_SW11: u64 = 1 << 57;
const H_SW55: u64 = 1 << 58;

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
    if c & C_RW == 0 { h |= AP2_RO; }
    else if hw_dirty() { h |= DBM; if c & C_D == 0 { h |= AP2_RO; } } // writable; read-only until the first write
    if c & C_US != 0 { h |= AP1_EL0 | PXN; }
    if c & C_NX != 0 { h |= UXN; if c & C_US == 0 { h |= PXN; } }
    if c & (C_PWT | C_PCD) != 0 { h |= ATTR_DEVICE << ATTR_SHIFT; } else { h |= SH_INNER; }
    if c & C_SW9 != 0 { h |= H_SW9; }
    if c & C_SW10 != 0 { h |= H_SW10; }
    if c & C_SW11 != 0 { h |= H_SW11; }
    if c & C_SW55 != 0 { h |= H_SW55; }
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
    if h & AP1_EL0 != 0 { c |= C_US; }
    if h & UXN != 0 { c |= C_NX; }
    if (h >> ATTR_SHIFT) & 0x7 == ATTR_DEVICE { c |= C_PWT | C_PCD; }
    if h & H_SW9 != 0 { c |= C_SW9; }
    if h & H_SW10 != 0 { c |= C_SW10; }
    if h & H_SW11 != 0 { c |= C_SW11; }
    if h & H_SW55 != 0 { c |= C_SW55; }
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

/// Make `root` the active root table. As a CR3 write on x86 without PCID: this core's
/// translations are dropped (ASID 0 is shared by every space for now).
#[inline(always)]
pub unsafe fn set_root(root: u64) {
    // Every space shares ASID 0 for now, so the old space's translations must be gone
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
        in(reg) root & ADDR_MASK, options(nostack));
}

/// Drop every translation this core has cached.
#[inline(always)]
pub unsafe fn flush_all() { core::arch::asm!("tlbi vmalle1", "dsb nsh", "isb", options(nostack)); }

/// Drop the translation of one page — on EVERY core: `IS` broadcasts to the
/// inner-shareable domain, which is what makes `paging::shootdown_impl` a plain flush here.
#[inline(always)]
pub unsafe fn flush_page(vaddr: u64) {
    core::arch::asm!("tlbi vaae1is, {}", "dsb ish", "isb", in(reg) vaddr >> 12, options(nostack));
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
/// has it); on a Graviton GB0 is RAM (from 5 MiB) and GB3 is the PCI window with the
/// 16550 — a fixed "below the kernel is devices" would have cached the console's registers.
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
/// Once, on the boot core, with the MMU off, from `kmain`, before any other core exists;
/// running from an identity address (the image is loaded where it is linked).
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
    l0.0[0] = (core::ptr::addr_of!(L1) as u64) | VALID | TABLE_OR_PAGE;
    // The intermediate physical address size: what the core supports, capped at 48 bits
    // (the encodings of PARange and TCR.IPS coincide up to that point).
    HW_DIRTY.store(cpu::has_hw_dirty(), core::sync::atomic::Ordering::Relaxed);
    let tcr = tcr_value();
    core::arch::asm!(
        "msr mair_el1, {mair}",
        "msr tcr_el1, {tcr}",
        "msr ttbr0_el1, {ttbr}",
        "tlbi vmalle1",
        "dsb ish",
        "isb",
        mair = in(reg) MAIR_VALUE, tcr = in(reg) tcr, ttbr = in(reg) core::ptr::addr_of!(L0) as u64,
        options(nostack),
    );
    enable_here();
}

/// Turn the MMU on for THIS core with the table the boot core built (`early_enable`):
/// a secondary core comes up with the MMU off. SCTLR: M (the MMU), C (the data caches), I
/// (the instruction cache); the RES1 bits are whatever the core came up with —
/// read-modify-write, never a constant.
pub unsafe fn enable_here() {
    // The same MAIR/TCR/TTBR as the boot core (a secondary's registers start undefined).
    let tcr = tcr_value();
    core::arch::asm!(
        "msr mair_el1, {mair}",
        "msr tcr_el1, {tcr}",
        "msr ttbr0_el1, {ttbr}",
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

/// The root a core coming up should use: `paging::init`'s table once it exists, the early one before.
fn root_for_new_core() -> u64 {
    let k = crate::paging::kernel_cr3();
    if k != 0 { k } else { core::ptr::addr_of!(L0) as u64 }
}
