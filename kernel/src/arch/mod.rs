//! The architecture boundary. Everything below `arch/<name>/` is the code that
//! knows the machine: the instruction set, the privileged registers, the
//! interrupt controller, the entry points. Everything above it — the file
//! system, the network stack, the scheduler's policy, the process model — does
//! not, and reaches the machine ONLY through the surface re-exported here.
//!
//! The rule for what lives under `arch/`: a function moves here if and only if
//! it contains `asm!`, a hardware register layout or an ABI constant. The
//! surface is the same set of module names for every architecture
//! (`arch::cpu`, ...), fixed at compile time by `cfg(target_arch)` — no trait
//! objects, no generics over "the CPU": the hot paths stay inlined.
//!
//! The split exists for the arm64 port (`docs/ARM64-PLAN.md` §4.1). Phase 0 of
//! that plan MOVES the x86-64 code here without changing it, and is judged by
//! the regression count staying exactly what it was.

#[cfg(target_arch = "x86_64")]
#[macro_use]
pub mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use aarch64::*;
