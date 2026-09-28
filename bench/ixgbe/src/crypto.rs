//! AES-128-GCM on AES-NI + PCLMULQDQ, encryption in place (a zero-copy TLS record).
//! The hot cost of TLS is the symmetric encryption of the record; the handshake is off the hot path.
#![allow(non_snake_case)]
use core::arch::x86_64::*;

pub struct Aes128 { rk: [__m128i; 11], h: __m128i }

#[inline]
unsafe fn key_expand(prev: __m128i, kga: __m128i) -> __m128i {
    let t = _mm_shuffle_epi32(kga, 0xff);
    let mut k = prev;
    k = _mm_xor_si128(k, _mm_slli_si128(k, 4));
    k = _mm_xor_si128(k, _mm_slli_si128(k, 4));
    k = _mm_xor_si128(k, _mm_slli_si128(k, 4));
    _mm_xor_si128(k, t)
}

impl Aes128 {
    pub fn new(key: &[u8; 16]) -> Self {
        unsafe {
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
    }
    #[inline]
    unsafe fn encrypt_block(&self, b: __m128i) -> __m128i {
        let mut x = _mm_xor_si128(b, self.rk[0]);
        for i in 1..10 { x = _mm_aesenc_si128(x, self.rk[i]); }
        _mm_aesenclast_si128(x, self.rk[10])
    }

    /// AES-128-GCM: encrypts buf[..len] in place, writes the tag to buf[len..len+16].
    /// iv12 is the 96-bit nonce. aad is the associated data (the record header).
    pub fn seal_inplace(&self, iv12: &[u8; 12], aad: &[u8], buf: &mut [u8], len: usize) {
        unsafe {
            let bs = _mm_set_epi8(0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15);
            // J0 = IV || 0^31 || 1
            let mut j0 = [0u8; 16]; j0[..12].copy_from_slice(iv12); j0[15] = 1;
            let j0m = _mm_loadu_si128(j0.as_ptr() as *const __m128i);
            let ej0 = self.encrypt_block(j0m);
            // CTR with J0+1: whole blocks are a SIMD XOR in place, the tail goes byte by byte
            let mut ctr = 2u32;
            let mut off = 0;
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
            // GHASH(AAD || C || lens)
            let hb = _mm_shuffle_epi8(self.h, bs);
            let mut y = _mm_setzero_si128();
            let ghash_block = |y: __m128i, blk: &[u8]| -> __m128i {
                let x = if blk.len() == 16 {
                    _mm_shuffle_epi8(_mm_loadu_si128(blk.as_ptr() as *const __m128i), bs)
                } else {
                    let mut b16 = [0u8;16]; b16[..blk.len()].copy_from_slice(blk);
                    _mm_shuffle_epi8(_mm_loadu_si128(b16.as_ptr() as *const __m128i), bs)
                };
                gfmul(_mm_xor_si128(y, x), hb)
            };
            for c in aad.chunks(16) { y = ghash_block(y, c); }
            for c in buf[..len].chunks(16) { y = ghash_block(y, c); }
            let mut lens = [0u8;16];
            lens[..8].copy_from_slice(&((aad.len() as u64)*8).to_be_bytes());
            lens[8..].copy_from_slice(&((len as u64)*8).to_be_bytes());
            y = ghash_block(y, &lens);
            // tag = bswap(GHASH) ^ E(J0)
            let ghash_out = _mm_shuffle_epi8(y, bs);
            let tag = _mm_xor_si128(ghash_out, ej0);
            _mm_storeu_si128(buf[len..].as_mut_ptr() as *mut __m128i, tag);
        }
    }
}

#[inline]
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
    let key = [0u8; 16];
    let iv = [0u8; 12];
    let aes = Aes128::new(&key);
    let mut buf = [0u8; 32];
    aes.seal_inplace(&iv, &[], &mut buf, 16);
    let ct = [0x03,0x88,0xda,0xce,0x60,0xb6,0xa3,0x92,0xf3,0x28,0xc2,0xb9,0x71,0xb2,0xfe,0x78];
    let tag = [0xab,0x6e,0x47,0xd4,0x2c,0xec,0x13,0xbd,0xf5,0x3a,0x67,0xb2,0x12,0x57,0xbd,0xdf];
    buf[..16] == ct && buf[16..] == tag
}
