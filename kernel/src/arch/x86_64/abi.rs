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

/// The structures a call bounces through the kernel on a machine whose layouts differ
/// from the internal ones (`arch::aarch64::abi`): nothing here — the internal layouts ARE
/// x86-64's.
pub struct Bounce;
impl Bounce {
    #[inline(always)]
    pub fn uninit() -> Self { Bounce }
}
/// # Safety
/// Nothing is touched.
#[inline(always)]
pub unsafe fn enter(_nr: u64, _args: &mut [u64; 6], _b: &mut Bounce) {}
/// # Safety
/// Nothing is touched.
#[inline(always)]
pub unsafe fn leave(_nr: u64, _user: &[u64; 6], _b: &mut Bounce, r: u64) -> u64 { r }

/// `utsname.machine`.
pub const UTS_MACHINE: &[u8] = b"x86_64";
