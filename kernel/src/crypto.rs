//! AES-128-GCM on AES-NI + PCLMULQDQ, encryption in place (a zero-copy TLS record).
//! A target_feature on every function with intrinsics — it works on the soft-float
//! target x86_64-unknown-none without a custom target JSON (the SSE code is in these
//! functions, the CPU support is guaranteed by the boot: CR4.OSFXSR + -cpu host).
#![allow(non_snake_case)]
use core::arch::x86_64::*;

const FEAT: &str = "sse2,aes,ssse3,pclmul";

pub struct Aes128 { rk: [__m128i; 11], h: __m128i }

#[target_feature(enable = "sse2,aes")]
unsafe fn key_expand(prev: __m128i, kga: __m128i) -> __m128i {
    let t = _mm_shuffle_epi32(kga, 0xff);
    let mut k = prev;
    k = _mm_xor_si128(k, _mm_slli_si128(k, 4));
    k = _mm_xor_si128(k, _mm_slli_si128(k, 4));
    k = _mm_xor_si128(k, _mm_slli_si128(k, 4));
    _mm_xor_si128(k, t)
}

impl Aes128 {
    /// Expand `key` into round keys and derive the GHASH key.
    ///
    /// # Safety
    /// The CPU must support AES-NI (`CPUID.1:ECX.AES`). The kernel requires it
    /// at boot; callers elsewhere must check it.
    #[target_feature(enable = "sse2,aes")]
    pub unsafe fn new(key: &[u8; 16]) -> Self {
        let mut rk = [_mm_setzero_si128(); 11];
        rk[0] = _mm_loadu_si128(key.as_ptr() as *const __m128i);
        macro_rules! step { ($i:expr, $rc:expr) => {
            rk[$i] = key_expand(rk[$i-1], _mm_aeskeygenassist_si128(rk[$i-1], $rc));
        };}
        step!(1,0x01); step!(2,0x02); step!(3,0x04); step!(4,0x08); step!(5,0x10);
        step!(6,0x20); step!(7,0x40); step!(8,0x80); step!(9,0x1b); step!(10,0x36);
        let mut a = Aes128 { rk, h: _mm_setzero_si128() };
        a.h = a.encrypt_block(_mm_setzero_si128());
        a
    }
    #[target_feature(enable = "sse2,aes")]
    unsafe fn encrypt_block(&self, b: __m128i) -> __m128i {
        let mut x = _mm_xor_si128(b, self.rk[0]);
        for i in 1..10 { x = _mm_aesenc_si128(x, self.rk[i]); }
        _mm_aesenclast_si128(x, self.rk[10])
    }

    /// Encrypt `buf[..len]` in place and write the 16-byte tag at `buf[len..len + 16]`.
    ///
    /// # Safety
    /// The CPU must support AES-NI, SSSE3 and PCLMULQDQ. Panics if `buf` has no
    /// room for the tag (the tag is stored with an unaligned 16-byte write).
    #[target_feature(enable = "sse2,aes,ssse3,pclmulqdq")]
    pub unsafe fn seal_inplace(&self, iv12: &[u8; 12], aad: &[u8], buf: &mut [u8], len: usize) {
        assert!(len.checked_add(16).is_some_and(|end| end <= buf.len()), "seal_inplace: no room for the tag");
        let bs = _mm_set_epi8(0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15);
        let mut j0 = [0u8; 16]; j0[..12].copy_from_slice(iv12); j0[15] = 1;
        let j0m = _mm_loadu_si128(j0.as_ptr() as *const __m128i);
        let ej0 = self.encrypt_block(j0m);
        let mut ctr = 2u32; let mut off = 0;
        while off < len {
            let mut cb = j0; cb[12..16].copy_from_slice(&ctr.to_be_bytes());
            let ks = self.encrypt_block(_mm_loadu_si128(cb.as_ptr() as *const __m128i));
            let n = len - off;
            if n >= 16 {
                let p = buf.as_mut_ptr().add(off) as *mut __m128i;
                _mm_storeu_si128(p, _mm_xor_si128(_mm_loadu_si128(p), ks));
            } else {
                let mut kb = [0u8;16]; _mm_storeu_si128(kb.as_mut_ptr() as *mut __m128i, ks);
                for i in 0..n { buf[off+i] ^= kb[i]; }
            }
            ctr += 1; off += 16;
        }
        let hb = _mm_shuffle_epi8(self.h, bs);
        let mut y = _mm_setzero_si128();
        let gh = |y: __m128i, blk: &[u8]| -> __m128i {
            let x = if blk.len() == 16 {
                _mm_shuffle_epi8(_mm_loadu_si128(blk.as_ptr() as *const __m128i), bs)
            } else {
                let mut b16 = [0u8;16]; b16[..blk.len()].copy_from_slice(blk);
                _mm_shuffle_epi8(_mm_loadu_si128(b16.as_ptr() as *const __m128i), bs)
            };
            gfmul(_mm_xor_si128(y, x), hb)
        };
        for c in aad.chunks(16) { y = gh(y, c); }
        for c in buf[..len].chunks(16) { y = gh(y, c); }
        let mut lens = [0u8;16];
        lens[..8].copy_from_slice(&((aad.len() as u64)*8).to_be_bytes());
        lens[8..].copy_from_slice(&((len as u64)*8).to_be_bytes());
        y = gh(y, &lens);
        let ghash_out = _mm_shuffle_epi8(y, bs);
        let tag = _mm_xor_si128(ghash_out, ej0);
        _mm_storeu_si128(buf[len..].as_mut_ptr() as *mut __m128i, tag);
    }
}

#[target_feature(enable = "sse2,pclmulqdq")]
unsafe fn gfmul(a: __m128i, b: __m128i) -> __m128i {
    let tmp3 = _mm_clmulepi64_si128(a, b, 0x00);
    let tmp4 = _mm_clmulepi64_si128(a, b, 0x10);
    let tmp5 = _mm_clmulepi64_si128(a, b, 0x01);
    let tmp6 = _mm_clmulepi64_si128(a, b, 0x11);
    let tmp4 = _mm_xor_si128(tmp4, tmp5);
    let tmp5 = _mm_slli_si128(tmp4, 8);
    let tmp4 = _mm_srli_si128(tmp4, 8);
    let tmp3 = _mm_xor_si128(tmp3, tmp5);
    let tmp6 = _mm_xor_si128(tmp6, tmp4);
    let tmp7 = _mm_srli_epi32(tmp3, 31);
    let tmp8 = _mm_srli_epi32(tmp6, 31);
    let tmp3 = _mm_slli_epi32(tmp3, 1);
    let tmp6 = _mm_slli_epi32(tmp6, 1);
    let tmp9 = _mm_srli_si128(tmp7, 12);
    let tmp8 = _mm_slli_si128(tmp8, 4);
    let tmp7 = _mm_slli_si128(tmp7, 4);
    let tmp3 = _mm_or_si128(tmp3, tmp7);
    let tmp6 = _mm_or_si128(tmp6, tmp8);
    let tmp6 = _mm_or_si128(tmp6, tmp9);
    let tmp7 = _mm_slli_epi32(tmp3, 31);
    let tmp8 = _mm_slli_epi32(tmp3, 30);
    let tmp9 = _mm_slli_epi32(tmp3, 25);
    let tmp7 = _mm_xor_si128(tmp7, tmp8);
    let tmp7 = _mm_xor_si128(tmp7, tmp9);
    let tmp8 = _mm_srli_si128(tmp7, 4);
    let tmp7 = _mm_slli_si128(tmp7, 12);
    let tmp3 = _mm_xor_si128(tmp3, tmp7);
    let tmp2 = _mm_srli_epi32(tmp3, 1);
    let tmp4 = _mm_srli_epi32(tmp3, 2);
    let tmp5 = _mm_srli_epi32(tmp3, 7);
    let tmp2 = _mm_xor_si128(tmp2, tmp4);
    let tmp2 = _mm_xor_si128(tmp2, tmp5);
    let tmp2 = _mm_xor_si128(tmp2, tmp8);
    let tmp3 = _mm_xor_si128(tmp3, tmp2);
    _mm_xor_si128(tmp6, tmp3)
}

/// A self-test against NIST AES-128-GCM (Test Case 2).
pub fn selftest() -> bool {
    if !ct_eq_selftest() { return false; }
    let _ = FEAT;
    unsafe {
        let aes = Aes128::new(&[0u8; 16]);
        let mut buf = [0u8; 32];
        aes.seal_inplace(&[0u8; 12], &[], &mut buf, 16);
        let ct = [0x03,0x88,0xda,0xce,0x60,0xb6,0xa3,0x92,0xf3,0x28,0xc2,0xb9,0x71,0xb2,0xfe,0x78];
        let tag = [0xab,0x6e,0x47,0xd4,0x2c,0xec,0x13,0xbd,0xf5,0x3a,0x67,0xb2,0x12,0x57,0xbd,0xdf];
        buf[..16] == ct && buf[16..] == tag
    }
}

/// A comparison in CONSTANT time: are `a` and `b` equal.
///
/// An ordinary `a == b` for slices stops at the first discrepancy, and the time of
/// the answer tells how many bytes of the secret have been guessed. For a password on a network port
/// that is an oracle: guessing stops being an enumeration over the whole space and
/// becomes an enumeration BYTE BY BYTE. At the latencies for whose sake Talaria
/// exists (single microseconds per round trip), the difference is measurable over the network.
///
/// Here all the bytes are walked, and the result is assembled through a `|`, so that an early
/// exit does not come back through the optimiser. The lengths are compared separately: it is no
/// secret anyway (it is visible from the request's size).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    core::hint::black_box(diff) == 0
}

/// The self-test of `ct_eq` — EQUALITY is checked (the time cannot be measured here, but
/// the correctness is bound to hold): the lengths, the prefixes, a discrepancy in the first and in
/// the last byte.
pub fn ct_eq_selftest() -> bool {
    let ok = ct_eq(b"", b"")
        && ct_eq(b"abc", b"abc")
        && !ct_eq(b"abc", b"abd")   // a discrepancy in the last one
        && !ct_eq(b"abc", b"bbc")   // a discrepancy in the first one
        && !ct_eq(b"abc", b"ab")    // the prefix is shorter
        && !ct_eq(b"ab", b"abc");   // the prefix is longer
    if !ok { crate::println!("ct_eq: FAILURE"); }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// Seal `pt` with `aad` and return ciphertext || tag.
    fn seal(key: &str, iv: &str, aad: &str, pt: &str) -> Vec<u8> {
        let k: [u8; 16] = hex(key).try_into().unwrap();
        let iv: [u8; 12] = hex(iv).try_into().unwrap();
        let p = hex(pt);
        let mut buf = p.clone();
        buf.resize(p.len() + 16, 0);
        unsafe { Aes128::new(&k).seal_inplace(&iv, &hex(aad), &mut buf, p.len()) };
        buf
    }

    // NIST GCM specification test vectors (McGrew & Viega), AES-128.
    const K3: &str = "feffe9928665731c6d6a8f9467308308";
    const IV3: &str = "cafebabefacedbaddecaf888";
    const P3: &str = "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255";
    const C3: &str = "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091473f5985";

    #[test]
    fn nist_case_1_empty_message() {
        assert_eq!(seal("00000000000000000000000000000000", "000000000000000000000000", "", ""),
                   hex("58e2fccefa7e3061367f1d57a4e7455a"));
    }

    #[test]
    fn nist_case_2_one_zero_block() {
        assert_eq!(seal("00000000000000000000000000000000", "000000000000000000000000", "", "00000000000000000000000000000000"),
                   hex("0388dace60b6a392f328c2b971b2fe78ab6e47d42cec13bdf53a67b21257bddf"));
    }

    #[test]
    fn nist_case_3_four_blocks() {
        assert_eq!(seal(K3, IV3, "", P3), hex(&format!("{C3}4d5c2af327cd64a62cf35abd2ba6fab4")));
    }

    #[test]
    fn nist_case_4_partial_block_with_aad() {
        let got = seal(K3, IV3, "feedfacedeadbeeffeedfacedeadbeefabaddad2", &P3[..120]);
        assert_eq!(got, hex(&format!("{}5bc94fbc3221a5db94fae95ae7121a47", &C3[..120])));
    }

    #[test]
    fn in_kernel_selftest_passes() {
        assert!(selftest());
    }

    #[test]
    #[should_panic(expected = "no room for the tag")]
    fn seal_refuses_a_buffer_without_room_for_the_tag() {
        let mut buf = [0u8; 20];
        unsafe { Aes128::new(&[0; 16]).seal_inplace(&[0; 12], &[], &mut buf, 8) };
    }

    #[test]
    fn ct_eq_semantics() {
        assert!(ct_eq(b"", b""));
        assert!(ct_eq(b"secret", b"secret"));
        assert!(!ct_eq(b"secret", b"secreT"));
        assert!(!ct_eq(b"secret", b"Secret"));
        assert!(!ct_eq(b"secret", b"secre"));
        assert!(!ct_eq(b"secre", b"secret"));
        let a = vec![7u8; 4096];
        let mut b = a.clone();
        assert!(ct_eq(&a, &b));
        b[2048] ^= 1;
        assert!(!ct_eq(&a, &b));
    }
}
