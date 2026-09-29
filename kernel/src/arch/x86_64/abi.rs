//! The Linux x86-64 syscall ABI as the dispatcher sees it. The kernel's INTERNAL syscall
//! numbers and argument order are x86-64's (`syscall.rs` grew up on this machine), so
//! here the decoding is the identity: every number is passed through — `dispatch` says
//! `ENOSYS` for what it does not implement — and no argument moves. The arm64 twin
//! (`arch::aarch64::abi`) maps the asm-generic numbers and swaps `clone`'s tls/ctid
//! (docs/ARM64-PLAN.md §4.4).

/// The internal number of an x86-64 syscall number: itself.
#[inline(always)]
pub fn to_internal(nr: u64) -> Option<u64> { Some(nr) }

/// Bring the six arguments into the internal (x86-64) order: nothing to do here.
#[inline(always)]
pub fn normalize(_nr: u64, _args: &mut [u64; 6]) {}
