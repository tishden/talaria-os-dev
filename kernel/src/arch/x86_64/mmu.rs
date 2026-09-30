//! The page-table descriptor as the x86-64 MMU reads it, and the handful of
//! operations `paging.rs` needs from the hardware: the root register, the TLB, the
//! feature checks, the one-time enables. `paging.rs` keeps the tables in the
//! CANONICAL format — which IS the x86-64 format: present bit 0, RW bit 1, U/S bit 2,
//! PWT/PCD bits 3-4, the large-leaf bit 7, NX bit 63, the OS bits at 9-11 and 52-55 —
//! so `enc` and `dec` are the identity here and the compiler removes them. On the
//! other architecture they translate (`arch::aarch64::mmu`), and every write into a
//! table goes through them (`paging::pte_w`), which is what lets one page-table module
//! serve two MMUs (docs/ARM64-PLAN.md §4.3).

use super::cpu;

/// Canonical -> hardware. The identity on x86-64.
#[inline(always)] pub fn enc(canonical: u64) -> u64 { canonical }
/// Hardware -> canonical. The identity on x86-64.
#[inline(always)] pub fn dec(hardware: u64) -> u64 { hardware }

/// The physical address of the active root table (CR3 without its flag bits is the
/// caller's business — `paging` masks with `ADDR_MASK`).
#[inline(always)] pub fn root() -> u64 { unsafe { cpu::read_cr3() } }
/// Make `root` the active root table. Never `nomem`: it publishes the table writes before it.
#[inline(always)] pub unsafe fn set_root(root: u64) { cpu::write_cr3(root) }
/// Drop every translation this core has cached (a CR3 reload).
#[inline(always)] pub unsafe fn flush_all() { cpu::write_cr3(cpu::read_cr3()) }
/// Drop this core's translation of one page.
#[inline(always)] pub unsafe fn flush_page(vaddr: u64) { cpu::invlpg(vaddr) }
/// Does this core do 1 GiB leaves (CPUID 0x80000001 EDX bit 26)?
pub fn has_1g() -> bool { cpu::cpuid(0x8000_0001).3 & (1 << 26) != 0 }

/// The one-time enables before the first table is published, on the boot core:
/// EFER.NXE (the NX bit means something), CR0.WP (a kernel write into an RO page
/// faults — the COW protection of the kernel's own writes), SMEP where the CPU has
/// it (ring 0 never fetches from a user page). Returns whether SMEP is on.
///
/// # Safety
/// P5 of `paging.rs`: THIS core's control registers, before its tables are published.
pub unsafe fn init_hw() -> bool {
    const MSR_EFER: u32 = 0xC000_0080;
    cpu::wrmsr(MSR_EFER, cpu::rdmsr(MSR_EFER) | (1 << 11));
    cpu::write_cr0(cpu::read_cr0() | (1 << 16));
    // CPUID.(EAX=7,ECX=0):EBX bit 7 — SMEP. Ask only if leaf 7 exists at all.
    if cpu::cpuid(0).0 < 7 { return false; }
    if cpu::cpuid_count(7, 0).1 & (1 << 7) == 0 { return false; }
    cpu::write_cr4(cpu::read_cr4() | (1 << 20));
    true
}

/// What each of the four identity gigabytes is (`paging::init`): GB0 holds the kernel's
/// image and the user identity region (2M leaves, per-leaf U/S), GB1-2 are RAM, GB3 is
/// the MMIO hole below 4 GiB (uncached). `kernel_gb` is 0 here — the image is linked at
/// 2 MiB — and the parameter exists because on arm64 it is not.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GbKind { Device, User2M, Ram1G }
pub fn gb_kind(gb: usize, kernel_gb: usize) -> GbKind {
    if gb == 3 { GbKind::Device } else if gb == kernel_gb { GbKind::User2M } else { GbKind::Ram1G }
}

// ── The PAT: the memory type of a page by a slot index (write-combining for the ENA LLQ window) ──
// PWT|PCD|PAT index a slot of IA32_PAT. Linux keeps PA0..PA3 = WB/WT/UC-/UC and puts
// WC into PA4 — the ENA driver maps its LLQ BAR with `ioremap_wc`, so that is exactly
// where the card expects 64-byte bursts to come from. We change ONLY PA4: PA0..PA3
// stay as after a reset, so no existing mapping (they are all in PA0..PA3) changes
// its type. The slot index for a 4K page = PAT(bit 7)<<2 | PCD(bit 4)<<1 | PWT(bit 3),
// so a WC page is exactly "the PAT bit is raised, PCD/PWT are cleared".
const MSR_PAT: u32 = 0x277;
/// PA0..PA3 and PA5..PA7 are as after a reset, PA4 = 0x01 (write-combining).
pub const PAT_VALUE: u64 = 0x0007_0401_0007_0406;
/// For a 4K leaf the PAT bit is the seventh one (for a 2M/1G one it is the twelfth).
pub const PTE_PAT_4K: u64 = 1 << 7;

/// Program IA32_PAT. Call it on EVERY core (otherwise one and the same page will be
/// WC on one core and WB on another). The sequence is as in SDM 11.11.8:
/// CD=1, wbinvd, a TLB flush, the MSR write, wbinvd, a TLB flush, CD restored.
pub fn pat_init() {
    // SAFETY: P5 — the PAT MSR of THIS core plus a TLB flush — the slot's meaning changes for
    // translations already cached.
    unsafe {
        let flags = cpu::irq_flags();
        cpu::irq_disable();
        let cr0 = cpu::read_cr0();
        cpu::write_cr0((cr0 | (1 << 30)) & !(1u64 << 29)); // CD=1, NW=0
        cpu::wbinvd();
        flush_all();
        cpu::wrmsr(MSR_PAT, PAT_VALUE);
        cpu::wbinvd();
        flush_all();
        cpu::write_cr0(cr0);
        cpu::irq_restore(flags);
    }
}

/// Read IA32_PAT of this core (the self-check `talaria.test=wc` compares it with `PAT_VALUE`).
/// A 4K leaf re-typed as write-combining: the PAT slot (PAT=1, PCD=0, PWT=0) `pat_init` set.
pub fn with_wc(pte: u64) -> u64 { (pte & !((1 << 3) | (1 << 4))) | PTE_PAT_4K }

pub fn pat_read() -> u64 {
    // SAFETY: P5 — a read of this core's PAT MSR.
    unsafe { cpu::rdmsr(MSR_PAT) }
}
