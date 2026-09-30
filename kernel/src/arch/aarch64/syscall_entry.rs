//! The syscall entry on arm64 is the `svc` vector of `trap.rs`: the same frame every
//! entry saves (`trap::Frame` — `nr` in x8, `a1..a6` in x0..x5, `pc` = ELR, `flags` = SPSR,
//! `usp` = SP_EL0), handed to `rust_syscall` as `&mut Frame`; the result goes into the
//! frame's x0 slot and the stub's restore returns it. There are no MSRs to arm and no
//! second layout to keep in step with a stub: `init` is the vectors, done once per core.

pub use super::trap::Frame;

/// The vectors are this core's syscall entry (`VBAR_EL1`, set by `trap::init`).
pub fn init() { super::trap::init(); }
/// # Safety
/// The x86 name; nothing to arm here.
pub unsafe fn init_msrs() { super::trap::init(); }
