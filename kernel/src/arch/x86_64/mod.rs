//! x86-64: the machine as the kernel has always seen it. Nothing in this
//! directory is new — it is the `asm!`-holding code of the shared modules,
//! moved behind the `arch::` surface (`docs/ARM64-PLAN.md` §4.1).

pub mod cpu;
