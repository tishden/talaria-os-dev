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
pub mod gic;
pub mod icache;
pub mod idt;     // the INTIDs under the x86 vector names, the handler table, ExcFrame
pub mod ioapic;  // a device line to a core = a GIC SPI route
pub mod kmain;
pub mod mmu;
pub mod pending;
pub mod percpu;
pub mod pmu;     // PMUv3 is phase 8: available() == false
pub mod probes;
pub mod psci;
pub mod smp;     // the cores and the tick under the LAPIC's names (GIC + generic timer + PSCI)
pub mod timer;
pub mod trap;
pub mod tsc;     // the counter under the TSC's names; the wall clock is the shared wallclock.rs
pub mod vga;     // no text window: the surface answers "no"

/// The vDSO symbol a libc looks for on this architecture; `vdso::init` refuses an image without it.
pub const VDSO_CLOCK_SYMBOL: &[u8] = b"__kernel_clock_gettime";
