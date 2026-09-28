//! The htree index of a directory: the array of entries of a root/node block and
//! the division of a full leaf between two blocks.
//!
//! Why a separate module. The index is a SECOND way to find a name, and it is
//! the one the workload uses: a directory of tens of thousands of files is read
//! by a descent, not by a walk. A mistake here therefore does not corrupt
//! anything visibly — it makes a name that lies in the directory unfindable.
//! The split is where that is easiest: the records of the leaf are laid out into
//! two blocks anew, and the index gets ONE number, the hash of the first record
//! of the new block. Take that number from the wrong record, or divide the
//! records at a place that disagrees with it, and every name from the boundary
//! goes missing — while the block still tiles, `e2fsck` is clean and the linear
//! walk (which the index makes unnecessary) still finds everything.
//!
//! The format is ext3/ext4's, since an index left by Linux is read by us and
//! ours by Linux:
//! ```text
//!   the root (the directory's block 0): "." and ".." (at ".." rec_len goes to
//!     the block's end, so a linear reader sees no index), then at 0x18 the
//!     dx_root_info {reserved_zero u32, hash_version u8, info_length u8,
//!     indirect_levels u8, unused u8} and from 0x18+info_length the array;
//!   a node: a dummy record of 8 bytes (inode 0, rec_len = the block) and the
//!     array from 0x08;
//!   an array entry (dx_entry, little-endian): {hash u32, block u32}, where
//!     `block` is a LOGICAL block of the directory. In the zeroth entry the hash
//!     field is occupied by the pair {limit u16, count u16} — the zeroth entry
//!     covers every hash up to the first one.
//! ```
//! Nothing here needs a device or a block cache: a block is a slice of bytes.

use crate::ext2dir;

/// The size of an array entry (`dx_entry`).
pub const ENT: usize = 8;
/// Where a node's array starts (right after the dummy record).
pub const NODE_ARRAY: usize = 8;
/// Where the `dx_root_info` lies in the root.
pub const ROOT_INFO: usize = 0x18;
/// The length of the `dx_root_info` we write (`info_length`).
pub const INFO_LEN: usize = 8;

#[inline]
fn rd32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }
#[inline]
fn rd16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) }
#[inline]
fn wr32(b: &mut [u8], o: usize, v: u32) { b[o..o + 4].copy_from_slice(&v.to_le_bytes()); }
#[inline]
fn wr16(b: &mut [u8], o: usize, v: u16) { b[o..o + 2].copy_from_slice(&v.to_le_bytes()); }

/// How many entries a node's array holds (`e2fsck` counts the limit the same
/// way: from the array's offset to the block's end).
#[inline]
pub fn node_limit(bs: usize) -> usize { (bs - NODE_ARRAY) / ENT }
/// The same for the root, whose array starts after the dots and the info.
#[inline]
pub fn root_limit(bs: usize) -> usize { (bs - (ROOT_INFO + INFO_LEN)) / ENT }

/// The root's header: `(the array's offset, indirect_levels)`. `None` — this is
/// not a root we can parse (a foreign `reserved_zero`, an info shorter than the
/// structure, an array that does not fit the block).
pub fn root_info(d: &[u8], bs: usize) -> Option<(usize, usize)> {
    if bs < ROOT_INFO + INFO_LEN + ENT || d.len() < bs { return None; }
    if rd32(d, ROOT_INFO) != 0 { return None; }          // reserved_zero
    let info_len = d[ROOT_INFO + 5] as usize;            // info_length
    let levels = d[ROOT_INFO + 6] as usize;              // indirect_levels
    if info_len < INFO_LEN || ROOT_INFO + info_len + ENT > bs { return None; }
    Some((ROOT_INFO + info_len, levels))
}

/// The array's `(count, limit)` at the offset `eoff`. `None` — the array is not
/// parsed: an empty one, a count above the limit, or a limit that does not fit
/// into the block. A descent must refuse such a block rather than read past it.
pub fn head(d: &[u8], eoff: usize, bs: usize) -> Option<(usize, usize)> {
    if eoff + ENT > bs || bs > d.len() { return None; }
    let limit = rd16(d, eoff) as usize;
    let count = rd16(d, eoff + 2) as usize;
    if count == 0 || count > limit || limit < 1 || eoff + limit * ENT > bs { return None; }
    Some((count, limit))
}

/// The hash of the entry `i` (for `i == 0` the field holds {limit, count}, so a
/// descent never asks for it).
#[inline]
pub fn hash_at(d: &[u8], eoff: usize, i: usize) -> u32 { rd32(d, eoff + i * ENT) }
/// The logical block the entry `i` leads to.
#[inline]
pub fn child_at(d: &[u8], eoff: usize, i: usize) -> u32 { rd32(d, eoff + i * ENT + 4) }
/// Write the array's `count`.
#[inline]
pub fn set_count(d: &mut [u8], eoff: usize, n: usize) { wr16(d, eoff + 2, n as u16); }
/// Write the block of the entry `i`.
#[inline]
pub fn set_child(d: &mut [u8], eoff: usize, i: usize, block: u32) { wr32(d, eoff + i * ENT + 4, block); }

/// The entry responsible for `hash`: the LAST one whose hash is not greater
/// (the zeroth one covers everything below the first). A binary search over an
/// array sorted by the hash — the same one `ext4_dx_probe` does.
pub fn descend(d: &[u8], eoff: usize, count: usize, hash: u32) -> usize {
    let (mut at, mut lo, mut hi) = (0usize, 1usize, count);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if hash_at(d, eoff, mid) <= hash { at = mid; lo = mid + 1; } else { hi = mid; }
    }
    at
}

/// Put `(hash, block)` into the array right after the entry `at` and grow the
/// count. `false` — the array is full (the caller grows the tree instead); it is
/// refused rather than written past the limit, otherwise the entry would land on
/// the records of the leaf or outside the block.
pub fn add_entry(d: &mut [u8], eoff: usize, count: usize, limit: usize, at: usize, hash: u32, block: u32) -> bool {
    if count >= limit || at >= count || eoff + (count + 1) * ENT > d.len() { return false; }
    let from = eoff + (at + 1) * ENT;
    let bytes = (count - at - 1) * ENT;
    d.copy_within(from..from + bytes, from + ENT);
    wr32(d, from, hash);
    wr32(d, from + 4, block);
    set_count(d, eoff, count + 1);
    true
}

/// A live record of a leaf as the split sees it.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Slot {
    /// The hash of the record's name.
    pub hash: u32,
    /// The record's offset in the source block.
    pub off: u16,
    /// The room the record really needs (`rec_len` without the slack).
    pub need: u16,
}

/// A zeroed slot: a static array of them is built at compile time.
pub const SLOT0: Slot = Slot { hash: 0, off: 0, need: 0 };

/// What a walk over a leaf found.
pub struct Collected {
    /// How many records went into `out`.
    pub n: usize,
    /// `out` ran out: NOT every record of the block is in the map. A split must
    /// not go on after that — the records left out would be lost by the repack.
    pub overflow: bool,
    /// The block does not tile (the walk ended before its end) or a record's
    /// name does not fit its own `rec_len`. Such a block must not be repacked:
    /// what the walk did not reach would be lost. A directory in the usual form
    /// never gives this.
    pub broken: bool,
    /// `.` — `(the inode, the file_type)`, when the block carries it.
    pub dot: Option<(u32, u8)>,
    /// `..` — the same.
    pub dotdot: Option<(u32, u8)>,
}

/// Walk a block of directory records and describe every live one: the hash of
/// its name, where it lies and how much room it needs. `skip_dots` leaves `.`
/// and `..` out of the map (they stay in the root and do not move into a leaf);
/// with it off every live record goes into the map, so a repack cannot drop one.
pub fn collect(src: &[u8], bs: usize, out: &mut [Slot], skip_dots: bool, hash: &mut impl FnMut(&[u8]) -> u32) -> Collected {
    let mut c = Collected { n: 0, overflow: false, broken: false, dot: None, dotdot: None };
    let mut end = 0usize;
    for e in ext2dir::Walk::new(&src[..bs]) {
        end = e.off + e.rec;
        if 8 + e.nlen > e.rec { c.broken = true; break; }
        if e.ino == 0 || e.nlen == 0 { continue; }
        let name = &src[e.off + 8..e.off + 8 + e.nlen];
        let is_dot = name == b".";
        let is_dotdot = name == b"..";
        if is_dot { c.dot = Some((e.ino, e.ft)); }
        if is_dotdot { c.dotdot = Some((e.ino, e.ft)); }
        if skip_dots && (is_dot || is_dotdot) { continue; }
        if c.n >= out.len() { c.overflow = true; break; }
        out[c.n] = Slot { hash: hash(name), off: e.off as u16, need: ext2dir::need(e.nlen) as u16 };
        c.n += 1;
    }
    if end != bs { c.broken = true; }
    c
}

/// Sort the map by the hash. The sort is STABLE on purpose: records with equal
/// hashes (a chain of collisions) keep the order they lie in the block in, so a
/// repack does not reshuffle a chain that the index cannot tell apart anyway.
pub fn sort(map: &mut [Slot]) {
    for i in 1..map.len() {
        let key = map[i];
        let mut j = i;
        while j > 0 && map[j - 1].hash > key.hash { map[j] = map[j - 1]; j -= 1; }
        map[j] = key;
    }
}

/// Where a full leaf is divided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Split {
    /// How many records stay in the old block; the rest go into the new one.
    pub keep: usize,
    /// The hash of the new block's first record — by it a descent chooses the
    /// half, so the division must be made at exactly this record.
    pub hash: u32,
    /// What goes into the index: the hash plus the low bit "the chain of
    /// collisions continues in the next block". The bit is free because a name's
    /// hash always has it zeroed.
    pub key: u32,
}

/// Choose the place of a division of a map SORTED by the hash: as in `do_split`
/// of ext4, by VOLUME rather than by the number of records — the halves must
/// each have room, and the records differ in length by a factor of thirty.
/// `None` — there is nothing to divide (fewer than two records).
pub fn split_point(map: &[Slot], bs: usize) -> Option<Split> {
    let n = map.len();
    if n < 2 { return None; }
    let (mut size, mut move_n, mut i) = (0usize, 0usize, n);
    while i > 0 {
        i -= 1;
        if size + (map[i].need as usize) / 2 > bs / 2 { break; }
        size += map[i].need as usize;
        move_n += 1;
    }
    let keep = (n - move_n).clamp(1, n - 1);
    let hash = map[keep].hash;
    let key = hash.wrapping_add(if hash == map[keep - 1].hash { 1 } else { 0 });
    Some(Split { keep, hash, key })
}

/// Does this set of records fit into one block? The halves are checked BEFORE
/// either block is rewritten: a repack that runs out of room in the middle would
/// lose the records it has not laid out yet.
pub fn fits(ents: &[Slot], bs: usize) -> bool {
    ents.iter().map(|e| e.need as usize).sum::<usize>() <= bs
}

/// Lay the chosen records out into the block `dst` anew, in the order of the
/// map, taking their names from `src`. The last record stretches its `rec_len`
/// to the block's end, so the walk ends exactly at the boundary. `false` — the
/// set does not fit (see `fits`); then `dst` is left alone.
pub fn pack(dst: &mut [u8], bs: usize, src: &[u8], ents: &[Slot]) -> bool {
    if !fits(ents, bs) { return false; }
    for x in dst[..bs].iter_mut() { *x = 0; }
    if ents.is_empty() { wr32(dst, 0, 0); wr16(dst, 4, bs as u16); return true; }
    let mut o = 0usize;
    for (k, e) in ents.iter().enumerate() {
        let (so, sz) = (e.off as usize, e.need as usize);
        let nl = src[so + 6] as usize;
        let rl = if k + 1 == ents.len() { bs - o } else { sz };
        wr32(dst, o, rd32(src, so));
        wr16(dst, o + 4, rl as u16);
        dst[o + 6] = nl as u8;
        dst[o + 7] = src[so + 7];
        dst[o + 8..o + 8 + nl].copy_from_slice(&src[so + 8..so + 8 + nl]);
        o += sz;
    }
    true
}

/// A fresh index node: a dummy record over the whole block (a linear reader sees
/// an empty block) and the array from the raw bytes `ents` — they are taken from
/// a copy of the block being split, so the entries themselves stay as they were.
pub fn node_write(d: &mut [u8], bs: usize, ents: &[u8]) {
    for x in d[..bs].iter_mut() { *x = 0; }
    wr32(d, 0, 0);
    wr16(d, 4, bs as u16);
    let n = ents.len() / ENT;
    d[NODE_ARRAY..NODE_ARRAY + n * ENT].copy_from_slice(&ents[..n * ENT]);
    wr16(d, NODE_ARRAY, node_limit(bs) as u16);
    wr16(d, NODE_ARRAY + 2, n as u16);
}

/// Lay out a `dx_root` in the directory's block 0: the records `.` and `..`
/// (the second one stretches to the block's end — that is what hides the index
/// from a linear reader), the `dx_root_info` and an array of one entry leading
/// to the leaf `leaf`.
pub fn root_write(d: &mut [u8], bs: usize, dir: u32, dot_ft: u8, dd_ino: u32, dd_ft: u8, hash_ver: u32, leaf: u32) {
    for x in d[..bs].iter_mut() { *x = 0; }
    wr32(d, 0, dir); wr16(d, 4, 12); d[6] = 1; d[7] = dot_ft; d[8] = b'.';
    wr32(d, 12, dd_ino); wr16(d, 16, (bs - 12) as u16); d[18] = 2; d[19] = dd_ft;
    d[20] = b'.'; d[21] = b'.';
    wr32(d, ROOT_INFO, 0);                               // reserved_zero
    d[ROOT_INFO + 4] = hash_ver as u8;
    d[ROOT_INFO + 5] = INFO_LEN as u8;
    d[ROOT_INFO + 6] = 0;                                // indirect_levels
    d[ROOT_INFO + 7] = 0;
    let arr = ROOT_INFO + INFO_LEN;
    wr16(d, arr, root_limit(bs) as u16);
    wr16(d, arr + 2, 1);
    wr32(d, arr + 4, leaf);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic hash: half-MD4 with a volume seed, as `mkfs.ext2` leaves it.
    const SEED: [u32; 4] = [0x1234_5678, 0x9abc_def0, 0x0f1e_2d3c, 0x4b5a_6978];
    fn h(name: &[u8]) -> u32 { crate::ext2hash::name_hash(name, 1, &SEED) }
    /// The cruellest hash there is: every name collides with every other. A real
    /// volume gives such chains on generated names (a hash of a hash).
    fn one(_name: &[u8]) -> u32 { 0x1000 }

    /// Do the records tile the block exactly? A block that fails this is one
    /// where a name is lost without any check noticing.
    fn tiles(d: &[u8]) -> bool {
        let mut o = 0usize;
        for e in ext2dir::Walk::new(d) {
            if e.off != o { return false; }
            o += e.rec;
        }
        o == d.len()
    }

    fn names_in(d: &[u8]) -> Vec<Vec<u8>> {
        ext2dir::Walk::new(d).filter(|e| e.ino != 0 && e.nlen != 0)
            .map(|e| d[e.off + 8..e.off + 8 + e.nlen].to_vec()).collect()
    }

    // ── an indexed directory in memory: the root and the leaves ──────────────
    //
    // The glue (the descent, the call of a split, the retry) repeats what
    // `ext2.rs` does around this module; under test is the arithmetic below.

    struct Dir {
        bs: usize,
        blk: Vec<Vec<u8>>,           // the directory's logical blocks
        hash: fn(&[u8]) -> u32,
        splits: usize,
        full: bool,                  // the root's array has run out (the tree would have to grow)
    }

    impl Dir {
        fn new(bs: usize, hash: fn(&[u8]) -> u32) -> Dir {
            let mut root = vec![0u8; bs];
            root_write(&mut root, bs, 2, 2, 2, 2, 1, 1);
            let mut leaf = vec![0u8; bs];
            ext2dir::init(&mut leaf, b"", 0, 0);   // an empty leaf: one free record
            Dir { bs, blk: vec![root, leaf], hash, splits: 0, full: false }
        }
        fn arr(&self) -> usize { root_info(&self.blk[0], self.bs).expect("the root is parsed").0 }
        fn head(&self) -> (usize, usize) { head(&self.blk[0], self.arr(), self.bs).expect("the array is parsed") }

        /// The leaf responsible for the hash, and its place in the root's array.
        fn probe(&self, hash: u32) -> (usize, usize) {
            let (arr, (count, _)) = (self.arr(), self.head());
            let at = descend(&self.blk[0], arr, count, hash);
            (at, child_at(&self.blk[0], arr, at) as usize)
        }

        fn insert(&mut self, name: &[u8], ino: u32) -> bool {
            let hash = (self.hash)(name);
            let (at, lb) = self.probe(hash);
            if ext2dir::insert(&mut self.blk[lb], name, ino, 1) { return true; }
            // ── the leaf is full: the split ──
            let src = self.blk[lb].clone();
            let mut map = [Slot::default(); 512];
            let c = collect(&src, self.bs, &mut map, false, &mut |n| (self.hash)(n));
            assert!(!c.broken && !c.overflow, "a leaf of a directory in the usual form");
            sort(&mut map[..c.n]);
            let s = match split_point(&map[..c.n], self.bs) { Some(s) => s, None => return false };
            assert!(fits(&map[..s.keep], self.bs) && fits(&map[s.keep..c.n], self.bs));
            let (arr, (count, limit)) = (self.arr(), self.head());
            let nb = self.blk.len();
            if !add_entry(&mut self.blk[0], arr, count, limit, at, s.key, nb as u32) {
                self.full = true;                 // the root is full — the kernel grows the tree here
                return false;
            }
            let mut fresh = vec![0u8; self.bs];
            assert!(pack(&mut fresh, self.bs, &src, &map[s.keep..c.n]));
            assert!(pack(&mut self.blk[lb], self.bs, &src, &map[..s.keep]));
            self.blk.push(fresh);
            self.splits += 1;
            let target = if hash >= s.hash { nb } else { lb };
            ext2dir::insert(&mut self.blk[target], name, ino, 1)
        }

        /// A search by the index, with the chain of collisions followed (that is
        /// `dx_next_leaf`: while the next entry carries the same hash, the name
        /// may lie in its block too).
        fn find(&self, name: &[u8]) -> Option<(u32, u8)> {
            let hash = (self.hash)(name);
            let (arr, (count, _)) = (self.arr(), self.head());
            let mut at = descend(&self.blk[0], arr, count, hash);
            loop {
                let lb = child_at(&self.blk[0], arr, at) as usize;
                if let Some(r) = ext2dir::find(&self.blk[lb], name) { return Some(r); }
                if at + 1 >= count || hash_at(&self.blk[0], arr, at + 1) & !1 != hash { return None; }
                at += 1;
            }
        }

        fn check(&self) {
            assert!(tiles(&self.blk[0]), "the root does not tile");
            for (i, b) in self.blk.iter().enumerate().skip(1) {
                assert!(tiles(b), "the leaf {} does not tile", i);
            }
            let (arr, (count, limit)) = (self.arr(), self.head());
            assert!(count <= limit);
            for i in 2..count {                   // the zeroth entry's hash field is {limit, count}
                assert!(hash_at(&self.blk[0], arr, i - 1) <= hash_at(&self.blk[0], arr, i),
                        "the root's array is not sorted at {}", i);
            }
        }
    }

    #[test]
    fn descend_picks_the_last_entry_not_greater_than_the_hash() {
        let bs = 1024usize;
        let mut d = vec![0u8; bs];
        let arr = 32usize;
        let hashes = [0u32, 100, 100, 500, 1000, 1000, 1000, 4000];
        wr16(&mut d, arr, node_limit(bs) as u16);
        wr16(&mut d, arr + 2, hashes.len() as u16);
        for (i, &x) in hashes.iter().enumerate() {
            if i > 0 { wr32(&mut d, arr + i * ENT, x); }
            wr32(&mut d, arr + i * ENT + 4, 100 + i as u32);
        }
        for probe in 0..5000u32 {
            // the linear answer: the last entry with hash <= probe (the zeroth covers everything below)
            let mut want = 0usize;
            for i in 1..hashes.len() { if hashes[i] <= probe { want = i; } }
            assert_eq!(descend(&d, arr, hashes.len(), probe), want, "the hash {}", probe);
        }
    }

    #[test]
    fn add_entry_keeps_the_array_sorted_and_refuses_a_full_one() {
        let bs = 1024usize;
        let mut d = vec![0u8; bs];
        node_write(&mut d, bs, &[]);
        let limit = node_limit(bs);
        // the zeroth entry: the block for the hashes from zero
        set_child(&mut d, NODE_ARRAY, 0, 7);
        set_count(&mut d, NODE_ARRAY, 1);
        let mut n = 1usize;
        let mut k = 1u32;
        while n < limit {
            let at = descend(&d, NODE_ARRAY, n, k * 10);
            assert!(add_entry(&mut d, NODE_ARRAY, n, limit, at, k * 10, 100 + k), "the entry {} did not go in", k);
            n += 1; k += 1;
            let (count, _) = head(&d, NODE_ARRAY, bs).unwrap();
            assert_eq!(count, n);
            for i in 2..n { assert!(hash_at(&d, NODE_ARRAY, i - 1) <= hash_at(&d, NODE_ARRAY, i)); }
        }
        let before = d.clone();
        assert!(!add_entry(&mut d, NODE_ARRAY, n, limit, n - 1, 99999, 999), "a full array must refuse");
        assert_eq!(before, d, "a refusal must not touch the block");
    }

    #[test]
    fn add_entry_puts_the_entry_in_its_place_not_at_the_end() {
        let bs = 1024usize;
        let mut d = vec![0u8; bs];
        node_write(&mut d, bs, &[]);
        set_child(&mut d, NODE_ARRAY, 0, 1);
        set_count(&mut d, NODE_ARRAY, 1);
        assert!(add_entry(&mut d, NODE_ARRAY, 1, node_limit(bs), 0, 300, 3));
        assert!(add_entry(&mut d, NODE_ARRAY, 2, node_limit(bs), 0, 100, 2));
        // the order must be 0(1), 100(2), 300(3)
        assert_eq!(head(&d, NODE_ARRAY, bs).unwrap().0, 3);
        assert_eq!((hash_at(&d, NODE_ARRAY, 1), child_at(&d, NODE_ARRAY, 1)), (100, 2));
        assert_eq!((hash_at(&d, NODE_ARRAY, 2), child_at(&d, NODE_ARRAY, 2)), (300, 3));
        assert_eq!(child_at(&d, NODE_ARRAY, 0), 1, "the zeroth entry stayed in place");
    }

    #[test]
    fn head_refuses_an_array_that_lies() {
        let bs = 1024usize;
        let mut d = vec![0u8; bs];
        node_write(&mut d, bs, &[]);
        set_count(&mut d, NODE_ARRAY, 4);
        assert_eq!(head(&d, NODE_ARRAY, bs), Some((4, node_limit(bs))));
        set_count(&mut d, NODE_ARRAY, 0);
        assert_eq!(head(&d, NODE_ARRAY, bs), None, "an empty array is not parsed");
        set_count(&mut d, NODE_ARRAY, node_limit(bs) + 1);
        assert_eq!(head(&d, NODE_ARRAY, bs), None, "a count above the limit");
        wr16(&mut d, NODE_ARRAY, (bs / ENT) as u16);   // a limit that does not fit the block
        set_count(&mut d, NODE_ARRAY, 1);
        assert_eq!(head(&d, NODE_ARRAY, bs), None);
        assert_eq!(head(&d, bs - 4, bs), None, "an array beyond the block");
    }

    #[test]
    fn root_write_hides_the_index_from_a_linear_reader() {
        let bs = 1024usize;
        let mut d = vec![0u8; bs];
        root_write(&mut d, bs, 12, 2, 2, 2, 1, 1);
        assert!(tiles(&d), "a linear walk must end at the block's end");
        assert_eq!(ext2dir::find(&d, b"."), Some((12, 2)));
        assert_eq!(ext2dir::find(&d, b".."), Some((2, 2)));
        assert!(ext2dir::only_dots(&d), "a linear reader sees a directory of two records");
        assert_eq!(names_in(&d).len(), 2);
        let (arr, levels) = root_info(&d, bs).expect("our own root is parsed");
        assert_eq!((arr, levels), (ROOT_INFO + INFO_LEN, 0));
        assert_eq!(head(&d, arr, bs), Some((1, root_limit(bs))));
        assert_eq!(child_at(&d, arr, 0), 1, "the only entry leads to the leaf");
    }

    #[test]
    fn root_info_refuses_a_foreign_root() {
        let bs = 1024usize;
        let mut d = vec![0u8; bs];
        root_write(&mut d, bs, 12, 2, 2, 2, 1, 1);
        let mut a = d.clone();
        wr32(&mut a, ROOT_INFO, 1);                      // reserved_zero is not zero
        assert_eq!(root_info(&a, bs), None);
        let mut b = d.clone();
        b[ROOT_INFO + 5] = 4;                            // info_length shorter than the structure
        assert_eq!(root_info(&b, bs), None);
        let mut c = d.clone();
        c[ROOT_INFO + 5] = 255;                          // a longer info is lawful: the array moves after it
        assert_eq!(root_info(&c, bs), Some((ROOT_INFO + 255, 0)));
        assert_eq!(root_info(&c, ROOT_INFO + 255 + 4), None, "an array that does not fit the block");
        let mut e = d.clone();
        e[ROOT_INFO + 6] = 1;                            // a level was added — that is lawful
        assert_eq!(root_info(&e, bs), Some((ROOT_INFO + INFO_LEN, 1)));
    }

    #[test]
    fn node_write_gives_a_block_a_linear_reader_sees_as_empty() {
        let bs = 4096usize;
        let mut ents = Vec::new();
        for i in 0..5u32 { ents.extend_from_slice(&(i * 10).to_le_bytes()); ents.extend_from_slice(&(i + 1).to_le_bytes()); }
        let mut d = vec![0u8; bs];
        node_write(&mut d, bs, &ents);
        assert!(tiles(&d));
        assert_eq!(names_in(&d).len(), 0, "a node carries no names");
        assert!(ext2dir::only_dots(&d), "nothing holds such a block as a directory");
        assert_eq!(head(&d, NODE_ARRAY, bs), Some((5, node_limit(bs))));
        for i in 1..5usize { assert_eq!(hash_at(&d, NODE_ARRAY, i), i as u32 * 10); }
        for i in 0..5usize { assert_eq!(child_at(&d, NODE_ARRAY, i), i as u32 + 1); }
    }

    #[test]
    fn pack_tiles_the_block_and_keeps_every_name() {
        let bs = 1024usize;
        let mut src = vec![0u8; bs];
        ext2dir::init(&mut src, b"first", 1, 1);
        let mut n = 1u32;
        while ext2dir::insert(&mut src, format!("name{:03}", n).as_bytes(), n + 1, 2) { n += 1; }
        let mut map = [Slot::default(); 512];
        let c = collect(&src, bs, &mut map, false, &mut h);
        assert!(!c.broken && !c.overflow);
        assert_eq!(c.n, n as usize, "every live record is in the map");
        sort(&mut map[..c.n]);
        let mut dst = vec![0u8; bs];
        assert!(pack(&mut dst, bs, &src, &map[..c.n]));
        assert!(tiles(&dst));
        let mut before = names_in(&src); before.sort();
        let mut after = names_in(&dst); after.sort();
        assert_eq!(before, after, "the repack lost or invented a name");
        for e in ext2dir::Walk::new(&dst) {
            if e.ino == 0 { continue; }
            let nm = dst[e.off + 8..e.off + 8 + e.nlen].to_vec();
            assert_eq!(ext2dir::find(&src, &nm), ext2dir::find(&dst, &nm), "{:?} changed its inode", nm);
        }
        // the records are laid out in the order of the hash — that is what the index promises
        let got: Vec<u32> = names_in(&dst).iter().map(|nm| h(nm)).collect();
        assert!(got.windows(2).all(|w| w[0] <= w[1]), "the block is not laid out by the hash");
    }

    #[test]
    fn pack_refuses_a_set_that_does_not_fit_instead_of_writing_past_the_block() {
        let bs = 1024usize;
        let mut src = vec![0u8; bs];
        ext2dir::init(&mut src, b"a", 1, 1);
        let mut map = [Slot::default(); 4];
        let c = collect(&src, bs, &mut map, false, &mut h);
        assert_eq!(c.n, 1);
        // one and the same record taken many times: the volume exceeds the block
        let many = [map[0]; 4];
        let mut big = Vec::new();
        for _ in 0..200 { big.push(map[0]); }
        assert!(fits(&many, bs));
        assert!(!fits(&big, bs));
        let mut dst = vec![0u8; bs];
        assert!(!pack(&mut dst, bs, &src, &big));
        assert!(dst.iter().all(|&b| b == 0), "a refusal must not touch the block");
    }

    #[test]
    fn collect_reports_a_torn_block_instead_of_repacking_it() {
        let bs = 1024usize;
        let mut good = vec![0u8; bs];
        ext2dir::init(&mut good, b"a", 1, 1);
        assert!(ext2dir::insert(&mut good, b"b", 2, 1));
        let mut map = [Slot::default(); 16];
        assert!(!collect(&good, bs, &mut map, false, &mut h).broken);
        // rec_len is short by four: the walk stops before the block's end
        let mut torn = good.clone();
        wr16(&mut torn, 4, 8);
        assert!(collect(&torn, bs, &mut map, false, &mut h).broken, "a block that does not tile");
        // a name longer than its own record
        let mut liar = good.clone();
        liar[6] = 200;
        assert!(collect(&liar, bs, &mut map, false, &mut h).broken, "a name that does not fit its record");
        // the map is out of room: the rest of the records would be lost by a repack
        let mut full = vec![0u8; bs];
        ext2dir::init(&mut full, b"n000", 1, 1);
        for i in 1..20u32 { assert!(ext2dir::insert(&mut full, format!("n{:03}", i).as_bytes(), i + 1, 1)); }
        let mut small = [Slot::default(); 4];
        let c = collect(&full, bs, &mut small, false, &mut h);
        assert!(c.overflow && c.n == 4);
    }

    #[test]
    fn collect_leaves_the_dots_out_only_when_asked() {
        let bs = 1024usize;
        let mut d = vec![0u8; bs];
        ext2dir::init(&mut d, b".", 2, 2);
        assert!(ext2dir::insert(&mut d, b"..", 2, 2));
        assert!(ext2dir::insert(&mut d, b"file", 9, 1));
        let mut map = [Slot::default(); 16];
        let all = collect(&d, bs, &mut map, false, &mut h);
        assert_eq!(all.n, 3);
        assert_eq!((all.dot, all.dotdot), (Some((2, 2)), Some((2, 2))));
        let without = collect(&d, bs, &mut map, true, &mut h);
        assert_eq!(without.n, 1, "only the ordinary name moves into a leaf");
        assert_eq!(map[0].hash, h(b"file"));
        assert_eq!((without.dot, without.dotdot), (Some((2, 2)), Some((2, 2))));
        // ".x" is an ordinary name and it is not a dot
        assert!(ext2dir::insert(&mut d, b".x", 10, 1));
        assert_eq!(collect(&d, bs, &mut map, true, &mut h).n, 2);
    }

    #[test]
    fn sort_is_stable_so_a_chain_of_collisions_keeps_its_order() {
        let mut map = [
            Slot { hash: 8, off: 10, need: 12 },
            Slot { hash: 4, off: 20, need: 12 },
            Slot { hash: 8, off: 30, need: 12 },
            Slot { hash: 4, off: 40, need: 12 },
            Slot { hash: 8, off: 50, need: 12 },
        ];
        sort(&mut map);
        assert_eq!(map.iter().map(|e| (e.hash, e.off)).collect::<Vec<_>>(),
                   vec![(4, 20), (4, 40), (8, 10), (8, 30), (8, 50)]);
    }

    #[test]
    fn split_point_divides_by_volume_and_leaves_both_halves_alive() {
        let bs = 1024usize;
        // long names first, short ones after: a division by the count would leave
        // the halves differing in volume by a factor of ten
        let mut map = Vec::new();
        for i in 0..4 { map.push(Slot { hash: i * 10, off: 0, need: 200 }); }
        for i in 4..20 { map.push(Slot { hash: i * 10, off: 0, need: 12 }); }
        let s = split_point(&map, bs).unwrap();
        let lo: usize = map[..s.keep].iter().map(|e| e.need as usize).sum();
        let hi: usize = map[s.keep..].iter().map(|e| e.need as usize).sum();
        assert!(s.keep >= 1 && s.keep < map.len());
        assert!(lo <= bs && hi <= bs, "a half must fit into a block");
        assert!(lo.abs_diff(hi) * 4 <= lo + hi, "the halves differ in volume by more than a quarter: {} vs {}", lo, hi);
        assert_eq!(s.hash, map[s.keep].hash);
        assert_eq!(s.key, s.hash, "without a collision the index carries the hash as it is");
    }

    #[test]
    fn split_point_of_two_records_leaves_one_on_each_side() {
        let bs = 1024usize;
        let map = [Slot { hash: 4, off: 0, need: 500 }, Slot { hash: 8, off: 0, need: 500 }];
        let s = split_point(&map, bs).unwrap();
        assert_eq!((s.keep, s.hash), (1, 8));
        assert_eq!(split_point(&map[..1], bs), None, "one record has nothing to divide");
        assert_eq!(split_point(&[], bs), None);
    }

    #[test]
    fn split_point_marks_a_division_inside_a_chain_of_collisions() {
        let bs = 1024usize;
        let map: Vec<Slot> = (0..20).map(|_| Slot { hash: 0x1000, off: 0, need: 60 }).collect();
        let s = split_point(&map, bs).unwrap();
        assert!(s.keep >= 1 && s.keep < map.len());
        assert_eq!(s.hash, 0x1000);
        assert_eq!(s.key, 0x1001, "the index must say the chain continues in the next block");
        assert_eq!(s.key & !1, s.hash, "a reader takes the hash back by dropping the bit");
    }

    #[test]
    fn a_split_leaf_keeps_every_name_findable_by_the_index() {
        for &bs in &[1024usize, 4096] {
            let mut dir = Dir::new(bs, h);
            let mut names = Vec::new();
            for i in 0..900u32 {
                let nm = format!("file-{:05}.dat", i).into_bytes();
                if !dir.insert(&nm, i + 10) { break; }
                names.push((nm, i + 10));
                dir.check();
            }
            assert!(dir.splits > 2, "a block of {} must have been split more than twice", bs);
            assert!(names.len() > 100);
            for (nm, ino) in &names {
                assert_eq!(dir.find(nm), Some((*ino, 1)), "the name {:?} is not found by the index", nm);
            }
            // a name that was not put in must not be found either
            assert_eq!(dir.find(b"absent-name"), None);
        }
    }

    #[test]
    fn a_chain_of_collisions_survives_the_splits() {
        let bs = 1024usize;
        let mut dir = Dir::new(bs, one);
        let mut names = Vec::new();
        for i in 0..300u32 {
            let nm = format!("collide{:04}", i).into_bytes();
            if !dir.insert(&nm, i + 10) { break; }
            names.push((nm, i + 10));
            dir.check();
        }
        assert!(dir.splits >= 2, "the leaves were split");
        assert!(names.len() > 50);
        let (arr, (count, _)) = (dir.arr(), dir.head());
        for i in 1..count {
            assert_eq!(hash_at(&dir.blk[0], arr, i) & !1, 0x1000);
            assert_eq!(hash_at(&dir.blk[0], arr, i) & 1, 1, "every division of the chain is marked");
        }
        for (nm, ino) in &names {
            assert_eq!(dir.find(nm), Some((*ino, 1)), "the name {:?} was lost in the chain", nm);
        }
    }

    #[test]
    fn names_of_any_length_go_in_and_come_back() {
        let bs = 1024usize;
        let mut dir = Dir::new(bs, h);
        let mut names = Vec::new();
        for i in 0..200u32 {
            let len = 1 + (i as usize * 7) % 200;
            let mut nm = format!("{:04}", i).into_bytes();
            nm.resize(len.max(4), b'x');
            if !dir.insert(&nm, i + 10) { break; }
            names.push((nm, i + 10));
            dir.check();
        }
        assert!(dir.splits > 1);
        for (nm, ino) in &names { assert_eq!(dir.find(nm), Some((*ino, 1)), "{:?}", nm); }
    }

    /// `dx_grow`, the node branch: a full node gives half of its entries away,
    /// and the parent indexes the new node by the hash of the entry the division
    /// was made at. A mistake of one entry here sends a whole range of hashes to
    /// the wrong node — every name in it disappears.
    #[test]
    fn a_node_split_sends_every_hash_to_the_block_it_went_to_before() {
        let bs = 1024usize;
        let (nl, count) = (node_limit(bs), node_limit(bs));
        let mut node = vec![0u8; bs];
        let mut ents = Vec::new();
        for i in 0..count as u32 { ents.extend_from_slice(&(i * 16).to_le_bytes()); ents.extend_from_slice(&(1000 + i).to_le_bytes()); }
        node_write(&mut node, bs, &ents);
        assert_eq!(head(&node, NODE_ARRAY, bs), Some((count, nl)));
        // the reference: where every hash led before the split
        let probes: Vec<u32> = (0..count as u32 * 16 + 32).collect();
        let before: Vec<u32> = probes.iter().map(|&p| child_at(&node, NODE_ARRAY, descend(&node, NODE_ARRAY, count, p))).collect();
        // the split: the entries [k, count) move away, the parent gets the hash of the k-th
        let k = count / 2;
        let src = node.clone();
        let hash2 = hash_at(&src, NODE_ARRAY, k);
        let mut fresh = vec![0u8; bs];
        node_write(&mut fresh, bs, &src[NODE_ARRAY + k * ENT..NODE_ARRAY + count * ENT]);
        set_count(&mut node, NODE_ARRAY, k);
        let mut root = vec![0u8; bs];
        root_write(&mut root, bs, 2, 2, 2, 2, 1, 1);   // one entry: the node, the logical block 1
        let arr = root_info(&root, bs).unwrap().0;
        root[ROOT_INFO + 6] = 1;                        // indirect_levels
        assert!(add_entry(&mut root, arr, 1, root_limit(bs), 0, hash2, 2));
        // the descent through the two levels must give the same answers as before
        for (i, &p) in probes.iter().enumerate() {
            let at = descend(&root, arr, 2, p);
            let child = child_at(&root, arr, at);
            let n = if child == 1 { &node } else { &fresh };
            let (c, _) = head(n, NODE_ARRAY, bs).unwrap();
            let got = child_at(n, NODE_ARRAY, descend(n, NODE_ARRAY, c, p));
            assert_eq!(got, before[i], "the hash {} used to lead to {} and now leads to {}", p, before[i], got);
        }
    }

    /// `dx_grow`, the level branch: the root's whole array moves into a node, and
    /// the root keeps one entry pointing at it. The descent must answer as it did
    /// before the level appeared.
    #[test]
    fn a_new_level_answers_as_the_root_did_before_it() {
        let bs = 1024usize;
        let mut root = vec![0u8; bs];
        root_write(&mut root, bs, 2, 2, 2, 2, 1, 1);
        let arr = root_info(&root, bs).unwrap().0;
        let limit = root_limit(bs);
        let mut count = 1usize;
        while count < limit {
            let at = descend(&root, arr, count, count as u32 * 16);
            assert!(add_entry(&mut root, arr, count, limit, at, count as u32 * 16, 100 + count as u32));
            count += 1;
        }
        let probes: Vec<u32> = (0..count as u32 * 16 + 32).collect();
        let before: Vec<u32> = probes.iter().map(|&p| child_at(&root, arr, descend(&root, arr, count, p))).collect();
        // the level: the array -> a node in a new block (the logical one 9), the root keeps one entry
        let src = root.clone();
        let mut node = vec![0u8; bs];
        node_write(&mut node, bs, &src[arr..arr + count * ENT]);
        set_count(&mut root, arr, 1);
        set_child(&mut root, arr, 0, 9);
        root[ROOT_INFO + 6] = 1;
        assert_eq!(root_info(&root, bs), Some((arr, 1)));
        for (i, &p) in probes.iter().enumerate() {
            assert_eq!(descend(&root, arr, 1, p), 0, "the root of one entry sends everything into the node");
            let (c, _) = head(&node, NODE_ARRAY, bs).unwrap();
            assert_eq!(c, count, "the node received the whole array");
            let got = child_at(&node, NODE_ARRAY, descend(&node, NODE_ARRAY, c, p));
            assert_eq!(got, before[i], "the hash {} changed its block", p);
        }
    }

    /// A node born of a level is born FULL (it received the root's whole array),
    /// so the very next insertion needs a split of the node as well — the place
    /// where a directory used to lose its index after growing exactly one level.
    #[test]
    fn a_node_born_of_a_level_is_full_at_once() {
        let bs = 1024usize;
        let root_ents = root_limit(bs);
        let mut node = vec![0u8; bs];
        let ents = vec![0u8; root_ents * ENT];
        node_write(&mut node, bs, &ents);
        let (count, limit) = head(&node, NODE_ARRAY, bs).unwrap();
        assert_eq!(count, root_ents);
        assert!(limit > count, "a node holds more than a root: its array starts earlier");
        // with a block of 1 KB the root holds 124 entries and a node 127 — three
        // insertions later the node is full too
        assert!(!add_entry(&mut node, NODE_ARRAY, limit, limit, limit - 1, 1, 1));
    }
}
