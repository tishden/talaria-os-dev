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

pub mod boot;
pub mod cpu;
pub mod dtb;
pub mod gic;
pub mod kmain;
pub mod mmu;
pub mod psci;
pub mod timer;
pub mod trap;
pub mod uart;
