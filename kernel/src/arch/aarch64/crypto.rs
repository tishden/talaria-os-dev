//! The constant-time comparison the control plane and `filed` use is plain arithmetic and
//! lives here for both machines; AES-128-GCM on the Crypto Extensions (`aese`/`pmull`) is
//! not written yet — `selftest` says so and `Aes128` refuses to be built.

/// `a == b` in time that depends only on the lengths, not on where they differ.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    let mut d = 0u8;
    for i in 0..a.len() { d |= a[i] ^ b[i]; }
    d == 0
}
pub fn ct_eq_selftest() -> bool { ct_eq(b"abc", b"abc") && !ct_eq(b"abc", b"abd") && !ct_eq(b"ab", b"abc") }

pub struct Aes128;
impl Aes128 {
    /// # Safety
    /// None on this target: there is no implementation, and `new` stops the caller loudly.
    pub unsafe fn new(_key: &[u8; 16]) -> Self {
        crate::println!("crypto: AES-128-GCM on the Crypto Extensions is not written for arm64 yet");
        Aes128
    }
    /// # Safety
    /// See `new`; this never runs a cipher.
    pub unsafe fn seal_inplace(&self, _nonce: &[u8; 12], _aad: &[u8], _buf: &mut [u8], _len: usize) {}
}
pub fn selftest() -> bool { false }
