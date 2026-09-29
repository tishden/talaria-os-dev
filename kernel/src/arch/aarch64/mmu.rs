//! The early identity map (phase 1): one level-1 table of 1 GiB blocks, `TTBR0_EL1`
//! only (`TCR.EPD1 = 1`), the 4K granule, a 39-bit virtual address (`T0SZ = 25`) so
//! that the root IS the level-1 table — 512 blocks cover the first 512 GiB, which is
//! the whole of QEMU `virt` up to (not including) its high PCIe MMIO window at 512 GiB.
//! The design decisions are in docs/ARM64-PLAN.md §4.3: one table like x86, memory
//! attributes as MAIR indices rather than page-table flags.
//!
//! Block 0 (0..1 GiB) is Device memory: the GIC, the UART, the virtio-mmio windows and
//! the low ECAM live there on `virt`. Blocks 1.. are Normal write-back RAM. Nothing
//! is executable from EL0 (`UXN`), and Device memory is not executable at all (`PXN`).
//!
//! # Safety
//!
//! `early_enable` is called ONCE on the boot core, with the MMU off, from the entry
//! (`kmain`), before any other core exists. It rewrites the translation regime of the
//! core that calls it; the caller must be running from an identity address (it is —
//! the image is loaded where it is linked) and must not hold pointers into memory the
//! new map does not cover (it holds none: the map covers the image and the DTB).

use super::cpu;

#[repr(C, align(4096))]
struct Table([u64; 512]);
static mut L1: Table = Table([0; 512]);

/// The MAIR indices. Attr0 = Normal write-back (RAM), Attr1 = Device-nGnRE (MMIO),
/// Attr2 = Normal non-cacheable (the write-combining analogue: the ENA LLQ window).
pub const ATTR_NORMAL: u64 = 0;
pub const ATTR_DEVICE: u64 = 1;
pub const ATTR_NC: u64 = 2;
const MAIR_VALUE: u64 = 0xFF | (0x04 << 8) | (0x44 << 16);

const VALID: u64 = 1 << 0;          // bit 1 = 0: a block, not a table
const AF: u64 = 1 << 10;            // the access flag — set by us, there is no hardware walk for it
const SH_INNER: u64 = 0b11 << 8;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
#[inline] const fn attr(i: u64) -> u64 { i << 2 }

/// A 1 GiB Normal RAM block for the kernel: RW at EL1, no EL0 access (AP[1] = 0).
const fn normal_block(pa: u64) -> u64 { pa | VALID | attr(ATTR_NORMAL) | SH_INNER | AF | UXN }
/// A 1 GiB Device block: RW at EL1, never executed.
const fn device_block(pa: u64) -> u64 { pa | VALID | attr(ATTR_DEVICE) | AF | UXN | PXN }

/// Build the table and turn the MMU on. `ram_base`/`ram_size` from the DTB or, before
/// it is parsed, `virt`'s 1 GiB base and a generous size — mapping absent RAM as Normal
/// memory is harmless as long as nothing touches it.
pub unsafe fn early_enable(ram_base: u64, ram_size: u64) {
    let t = &mut *core::ptr::addr_of_mut!(L1);
    t.0[0] = device_block(0);
    let first = (ram_base >> 30) as usize;
    let last = ((ram_base + ram_size - 1) >> 30) as usize;
    for i in first.max(1)..=last.min(511) {
        t.0[i] = normal_block((i as u64) << 30);
    }
    // The intermediate physical address size: what the core supports, capped at 48 bits
    // (the encodings of PARange and TCR.IPS coincide up to that point).
    let parange = (cpu::id_aa64mmfr0() & 0xF).min(5);
    let tcr: u64 = 25                    // T0SZ: a 39-bit VA
        | (1 << 8) | (1 << 10)           // IRGN0/ORGN0: the walks are write-back cacheable
        | (0b11 << 12)                   // SH0: inner shareable
        | (1 << 23)                      // EPD1: no TTBR1 walks
        | (parange << 32)                // IPS
        | (1 << 36);                     // AS: 16-bit ASIDs
    core::arch::asm!(
        "msr mair_el1, {mair}",
        "msr tcr_el1, {tcr}",
        "msr ttbr0_el1, {ttbr}",
        "tlbi vmalle1",
        "dsb ish",
        "isb",
        mair = in(reg) MAIR_VALUE, tcr = in(reg) tcr, ttbr = in(reg) core::ptr::addr_of!(L1) as u64,
        options(nostack),
    );
    // SCTLR: M (the MMU), C (the data caches), I (the instruction cache). The RES1 bits
    // are whatever the core came up with — read-modify-write, never a constant.
    let sctlr = cpu::sctlr_el1() | (1 << 0) | (1 << 2) | (1 << 12);
    core::arch::asm!("msr sctlr_el1, {}", "isb", in(reg) sctlr, options(nostack));
}
