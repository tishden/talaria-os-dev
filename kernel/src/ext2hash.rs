//! Directory-index (htree) name hash, compatible with ext4 (`fs/ext4/hash.c`)
//! and e2fsprogs: legacy, half-MD4 and TEA, each with signed or unsigned
//! `char` semantics. Pure functions, unit-tested against `debugfs dx_hash`.
//!
//! The hash decides which leaf block of an indexed directory a name lives in,
//! so it has to match bit for bit what `mkfs.ext2`/`e2fsck` and Linux compute
//! for the same volume — a mismatch does not fail loudly, it makes existing
//! names unfindable.

/// Hash version from the superblock: `s_def_hash_version` corrected by
/// `s_flags` (EXT2_FLAGS_UNSIGNED_HASH = 2: the volume was created on a machine
/// with unsigned `char`, versions 0-2 become 3-5). `None` — a version this
/// code does not implement (e.g. siphash).
pub fn effective_version(def_hash_version: u8, s_flags: u32) -> Option<u32> {
    let mut v = def_hash_version as u32;
    if s_flags & 0x2 != 0 && v < 3 { v += 3; }
    if v <= 5 { Some(v) } else { None }
}

/// Hash of `name` for hash version `ver` (0..=5) with the volume's
/// `s_hash_seed`. As in Linux, the seed is used as a whole when any of its
/// words is non-zero, and the default MD4 constants are used otherwise. The
/// lowest bit is always 0 (it marks a collision chain in the index), and the
/// value 0xFFFF_FFFE is reserved (EXT2_HTREE_EOF).
pub fn name_hash(name: &[u8], ver: u32, seed: &[u32; 4]) -> u32 {
    let mut buf = if seed.iter().any(|&w| w != 0) { *seed } else { [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476] };
    let h = match ver {
        0 | 3 => legacy_hash(name, ver == 0),
        1 | 4 => {
            for chunk in chunks(name, 32) {
                let mut w = [0u32; 8];
                str2hashbuf(chunk, &mut w, 8, ver == 1);
                half_md4(&mut buf, &w);
            }
            buf[1]
        }
        _ => {
            for chunk in chunks(name, 16) {
                let mut w = [0u32; 8];
                str2hashbuf(chunk, &mut w, 4, ver == 2);
                tea(&mut buf, &w);
            }
            buf[0]
        }
    };
    let h = h & !1;
    if h == 0xFFFF_FFFE { 0xFFFF_FFFC } else { h }
}

/// The name in `n`-byte steps, each step seeing the REST of the name (the
/// kernel passes `name + i*n` with the remaining length; `str2hashbuf` pads
/// with the full remaining length). An empty name gets no round, as in Linux.
fn chunks(name: &[u8], n: usize) -> impl Iterator<Item = &[u8]> {
    let rounds = name.len().div_ceil(n);
    (0..rounds).map(move |i| &name[(i * n).min(name.len())..])
}

#[inline] fn md4_f(x: u32, y: u32, z: u32) -> u32 { z ^ (x & (y ^ z)) }
#[inline] fn md4_g(x: u32, y: u32, z: u32) -> u32 { (x & y).wrapping_add((x ^ y) & z) }
#[inline] fn md4_h(x: u32, y: u32, z: u32) -> u32 { x ^ y ^ z }

fn half_md4(buf: &mut [u32; 4], w: &[u32; 8]) {
    const K2: u32 = 0x5A82_7999;
    const K3: u32 = 0x6ED9_EBA1;
    let (mut a, mut b, mut c, mut d) = (buf[0], buf[1], buf[2], buf[3]);
    macro_rules! r { ($f:ident, $a:ident, $b:ident, $c:ident, $d:ident, $x:expr, $s:expr) => {
        $a = $a.wrapping_add($f($b, $c, $d)).wrapping_add($x).rotate_left($s);
    } }
    r!(md4_f, a, b, c, d, w[0], 3);  r!(md4_f, d, a, b, c, w[1], 7);
    r!(md4_f, c, d, a, b, w[2], 11); r!(md4_f, b, c, d, a, w[3], 19);
    r!(md4_f, a, b, c, d, w[4], 3);  r!(md4_f, d, a, b, c, w[5], 7);
    r!(md4_f, c, d, a, b, w[6], 11); r!(md4_f, b, c, d, a, w[7], 19);
    r!(md4_g, a, b, c, d, w[1].wrapping_add(K2), 3);  r!(md4_g, d, a, b, c, w[3].wrapping_add(K2), 5);
    r!(md4_g, c, d, a, b, w[5].wrapping_add(K2), 9);  r!(md4_g, b, c, d, a, w[7].wrapping_add(K2), 13);
    r!(md4_g, a, b, c, d, w[0].wrapping_add(K2), 3);  r!(md4_g, d, a, b, c, w[2].wrapping_add(K2), 5);
    r!(md4_g, c, d, a, b, w[4].wrapping_add(K2), 9);  r!(md4_g, b, c, d, a, w[6].wrapping_add(K2), 13);
    r!(md4_h, a, b, c, d, w[3].wrapping_add(K3), 3);  r!(md4_h, d, a, b, c, w[7].wrapping_add(K3), 9);
    r!(md4_h, c, d, a, b, w[2].wrapping_add(K3), 11); r!(md4_h, b, c, d, a, w[6].wrapping_add(K3), 15);
    r!(md4_h, a, b, c, d, w[1].wrapping_add(K3), 3);  r!(md4_h, d, a, b, c, w[5].wrapping_add(K3), 9);
    r!(md4_h, c, d, a, b, w[0].wrapping_add(K3), 11); r!(md4_h, b, c, d, a, w[4].wrapping_add(K3), 15);
    buf[0] = buf[0].wrapping_add(a); buf[1] = buf[1].wrapping_add(b);
    buf[2] = buf[2].wrapping_add(c); buf[3] = buf[3].wrapping_add(d);
}

fn tea(buf: &mut [u32; 4], w: &[u32; 8]) {
    const DELTA: u32 = 0x9E37_79B9;
    let (mut sum, mut b0, mut b1) = (0u32, buf[0], buf[1]);
    let (a, b, c, d) = (w[0], w[1], w[2], w[3]);
    for _ in 0..16 {
        sum = sum.wrapping_add(DELTA);
        b0 = b0.wrapping_add(((b1 << 4).wrapping_add(a)) ^ b1.wrapping_add(sum) ^ ((b1 >> 5).wrapping_add(b)));
        b1 = b1.wrapping_add(((b0 << 4).wrapping_add(c)) ^ b0.wrapping_add(sum) ^ ((b0 >> 5).wrapping_add(d)));
    }
    buf[0] = buf[0].wrapping_add(b0); buf[1] = buf[1].wrapping_add(b1);
}

/// Pack the name into `num` 32-bit words, padded with its length (str2hashbuf).
fn str2hashbuf(msg: &[u8], w: &mut [u32; 8], num: usize, signed: bool) {
    let l0 = msg.len() as u32;
    let mut pad = l0 | (l0 << 8); pad |= pad << 16;
    let mut val = pad;
    let len = msg.len().min(num * 4);
    let mut num = num as isize;
    let mut k = 0usize;
    for (i, &b) in msg[..len].iter().enumerate() {
        if i % 4 == 0 { val = pad; }
        let ch = if signed { (b as i8) as i32 as u32 } else { b as u32 };
        val = ch.wrapping_add(val << 8);
        if i % 4 == 3 { if k < 8 { w[k] = val; k += 1; } val = pad; num -= 1; }
    }
    num -= 1;
    if num >= 0 && k < 8 { w[k] = val; k += 1; }
    loop { num -= 1; if num < 0 { break; } if k < 8 { w[k] = pad; k += 1; } }
}

fn legacy_hash(name: &[u8], signed: bool) -> u32 {
    let (mut h0, mut h1) = (0x12a3_fe2du32, 0x37ab_e8f9u32);
    for &ch in name {
        let c = if signed { (ch as i8) as i32 as u32 } else { ch as u32 };
        let mut h = h1.wrapping_add(h0 ^ c.wrapping_mul(7152373));
        if h & 0x8000_0000 != 0 { h = h.wrapping_sub(0x7fff_ffff); }
        h1 = h0; h0 = h;
    }
    h0 << 1
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [&str; 3] = ["a", "üñïçødé", "a_very_long_file_name_that_is_longer_than_thirty_two_bytes.dat"];
    /// `debugfs -R "dx_hash -h <ver> <name>"` (e2fsprogs 1.46.5), default seed.
    const DEBUGFS: [[u32; 3]; 6] = [
        [0xe74b53e2, 0xe6fd7104, 0x8defa24a], // 0 legacy
        [0xd5fa7d7a, 0x3987d1fc, 0xe4243792], // 1 half_md4
        [0x6d0ea4c0, 0x0844839c, 0x0b227cc0], // 2 tea
        [0xe74b53e2, 0x54a7144a, 0x8defa24a], // 3 legacy, unsigned char
        [0xd5fa7d7a, 0x0d8d95b0, 0xe4243792], // 4 half_md4, unsigned char
        [0x6d0ea4c0, 0x37f274d2, 0x0b227cc0], // 5 tea, unsigned char
    ];

    #[test]
    fn matches_debugfs_for_every_version() {
        for (ver, want) in DEBUGFS.iter().enumerate() {
            for (name, &h) in NAMES.iter().zip(want) {
                assert_eq!(name_hash(name.as_bytes(), ver as u32, &[0; 4]), h, "ver {ver} name {name}");
            }
        }
    }

    #[test]
    fn signedness_only_matters_for_bytes_above_127() {
        for base in [0u32, 1, 2] {
            assert_eq!(name_hash(b"plain.txt", base, &[0; 4]), name_hash(b"plain.txt", base + 3, &[0; 4]));
            assert_ne!(name_hash("üñïçødé".as_bytes(), base, &[0; 4]), name_hash("üñïçødé".as_bytes(), base + 3, &[0; 4]));
        }
    }

    #[test]
    fn explicit_seed_matches_debugfs() {
        // debugfs -R "dx_hash -h 1 -s 01234567-89ab-cdef-0123-456789abcdef hello.txt" -> 0x42a85304
        let seed = [0x6745_2301, 0xefcd_ab89, 0x6745_2301, 0xefcd_ab89];
        assert_eq!(name_hash(b"hello.txt", 1, &seed), 0x42a8_5304);
    }

    #[test]
    fn seed_is_used_whole_when_any_word_is_nonzero() {
        // Linux copies the whole seed if any word is set; zero words stay zero.
        let partial = [0, 0x1111_1111, 0, 0];
        assert_ne!(name_hash(b"x", 1, &partial), name_hash(b"x", 1, &[0; 4]));
        assert_ne!(name_hash(b"x", 1, &partial), name_hash(b"x", 1, &[0x6745_2301, 0x1111_1111, 0x98ba_dcfe, 0x1032_5476]));
    }

    #[test]
    fn low_bit_is_clear_and_eof_value_is_reserved() {
        for ver in 0..6 {
            for i in 0..2000u32 {
                let h = name_hash(format!("f{i}").as_bytes(), ver, &[0; 4]);
                assert_eq!(h & 1, 0);
                assert_ne!(h, 0xFFFF_FFFE);
            }
        }
    }

    #[test]
    fn version_from_superblock() {
        assert_eq!(effective_version(1, 0), Some(1));
        assert_eq!(effective_version(1, 2), Some(4));   // unsigned-hash flag
        assert_eq!(effective_version(4, 2), Some(4));   // already unsigned
        assert_eq!(effective_version(6, 0), None);      // siphash: not implemented
    }

    #[test]
    fn empty_name_still_hashes() {
        for ver in 0..6 { let _ = name_hash(b"", ver, &[0; 4]); }
    }
}
