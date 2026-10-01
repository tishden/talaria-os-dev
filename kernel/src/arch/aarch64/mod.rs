//! AArch64: the machine for the arm64 port (`docs/ARM64-PLAN.md`). Phase 1 of the plan
//! lives here — the boot entry, the early identity map, the exception vectors that
//! print a diagnosis, the UART — and nothing above `arch::` is compiled for this
//! target yet: every shared module of the kernel is gated `cfg(target_arch = "x86_64")`
//! in `main.rs` and comes over one at a time as its arch surface gets an
//! implementation here. The count of those gates is the port's progress meter.
//!
//! `dead_code` is allowed for the directory while that is so: the surface
//! (`cpu::irq_*`, the barriers, the fault registers) is written to the same
//! vocabulary as `arch::x86_64` ahead of its callers, and a wrapper that nobody
//! calls YET is the expected state, not a mistake. The allow goes when the first
//! shared module is compiled for this target.
#![allow(dead_code)]

#[macro_use]
pub mod uart;    // the PL011; `print!`/`println!` — first, so the macros are in scope for the rest
pub use uart as serial; // the x86 name the shared modules use
pub mod abi;
pub mod boot;
pub mod cat;     // no cache allocation on arm64: the surface answers "no"
pub mod cpu;
pub mod crypto;  // ct_eq; AES on the Crypto Extensions is not written yet
pub mod dtb;
pub mod gdb;     // the stub is phase 8: the surface says so
pub mod gdt;     // no tables: the way into EL0 and this core's kernel stack
pub mod gic;
pub mod icache;
pub mod idt;     // the INTIDs under the x86 vector names, the handler table, ExcFrame
pub mod ioapic;  // a device line to a core = a GIC SPI route
pub mod its;     // MSI-X: the ITS's tables and commands, LPIs
pub mod msi;     // what a device's MSI/MSI-X entry holds on this machine
pub mod kmain;
pub mod mmu;
pub mod percpu;
pub mod pmu;     // PMUv3 is phase 8: available() == false
pub mod probes;
pub mod psci;
pub mod setjmp;  // tal_setjmp/tal_longjmp: x19..x28, fp, lr, sp, d8..d15
pub mod signal;  // the rt_sigframe layout, the fault decode, sigreturn as a frame rewrite
pub mod smp;     // the cores and the tick under the LAPIC's names (GIC + generic timer + PSCI)
pub mod switch;  // the FP/SIMD state, the saved-context API, entering a frame, the soft yield
pub mod syscall_entry; // the svc entry is the trap frame; the neutral names live there
pub mod timer;
pub mod trap;
pub mod tsc;     // the counter under the TSC's names; the wall clock is the shared wallclock.rs
pub mod unwind;  // the guarded load a profiler makes on a foreign stack
pub mod vga;     // no text window: the surface answers "no"

/// The virtio-mmio slots a driver may probe: the device tree's `virtio,mmio` windows, as
/// `kmain` recorded them (QEMU `virt`: 32 windows of 0x200 from 0x0a00_0000, in the device
/// gigabyte below the kernel's — mapped as such by `paging::init`).
pub const VIRTIO_MMIO_SLOTS: usize = dtb::MAX_VIRTIO;
pub fn virtio_mmio_slot(i: usize) -> Option<(usize, usize)> {
    if i >= VIRTIO_MMIO_SLOTS { return None; }
    let base = kmain::VIRTIO_WINDOWS[i].load(core::sync::atomic::Ordering::Relaxed);
    if base == 0 { None } else { Some((base as usize, 0x200)) }
}
/// The "GSI" (the SPI's number: INTID - 32) of the virtio-mmio window in slot `i` — the
/// device tree names it for every window (`virt`: SPIs 16..47), nothing is derived.
pub fn virtio_mmio_gsi(i: usize) -> Option<u32> {
    if i >= VIRTIO_MMIO_SLOTS { return None; }
    let intid = kmain::VIRTIO_INTIDS[i].load(core::sync::atomic::Ordering::Relaxed);
    if intid < 32 { None } else { Some(intid as u32 - 32) }
}

/// The ELF `e_machine` this kernel runs, the `AT_HWCAP` word it hands a process and the
/// `AT_PLATFORM` string. The HWCAP bits are the ones a Graviton3 and QEMU `max` both have —
/// FP, ASIMD, EVTSTRM, AES, PMULL, SHA1, SHA2, CRC32, ATOMICS, FPHP, ASIMDHP, CPUID, ASIMDRDM
/// (bits 0..12), LRCPC (15), DCPOP (16), ASIMDDP (20); the rest are read from `ID_AA64*`
/// when a workload asks for them (phase 7).
pub const ELF_MACHINE: u16 = 0xB7;
pub const AT_HWCAP: u64 = 0x1FFF | (1 << 15) | (1 << 16) | (1 << 20);
pub const PLATFORM: [u8; 8] = *b"aarch64\0";

/// The vDSO symbol a libc looks for on this architecture; `vdso::init` refuses an image without it.
pub const VDSO_CLOCK_SYMBOL: &[u8] = b"__kernel_clock_gettime";

/// `talaria.test=its`: MSI-X through the interrupt translation service (arm64).
pub fn msi_selftest() -> ! { its::selftest() }
