//! The entries of an ext2 directory INSIDE ONE BLOCK: the search, the insertion,
//! the removal, "is there anything here besides `.` and `..`".
//!
//! Why a separate module. The layout of the records is the only place in the FS
//! where a mistake is not caught by any check on the machine: `rec_len` is both
//! the length of the record and the step of the walk, so an off-by-four turns a
//! directory into a different, still self-consistent directory — one where an
//! entry is simply not found any more, or where two entries overlap. e2fsck
//! notices it, the workload does not: it sees an ENOENT on a file it has just
//! created. The arithmetic lives here, without a block cache and without a
//! device, so it can be judged by tests rather than by a run in QEMU.
//!
//! What a record looks like (the same as in ext2/ext3/ext4):
//! ```text
//!   +0  u32 inode      0 = the record is free (a "hole")
//!   +4  u16 rec_len    the length of the record, a multiple of 4; the step to the next one
//!   +6  u8  name_len
//!   +7  u8  file_type
//!   +8  the name, without a terminating zero
//! ```
//! The last record of a block stretches its `rec_len` to the block's end, so a
//! walk always ends exactly at the boundary.

#[inline]
fn rd32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }
#[inline]
fn rd16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) }
#[inline]
fn wr32(b: &mut [u8], o: usize, v: u32) { b[o..o + 4].copy_from_slice(&v.to_le_bytes()); }
#[inline]
fn wr16(b: &mut [u8], o: usize, v: u16) { b[o..o + 2].copy_from_slice(&v.to_le_bytes()); }

/// How much room a record with a name of `n` bytes takes: the 8-byte header plus
/// the name, rounded up to 4 (`rec_len` is a multiple of four).
#[inline]
pub fn need(n: usize) -> usize { (8 + n + 3) & !3 }

/// One record as the walk sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ent {
    /// The record's offset in the block.
    pub off: usize,
    /// The inode; 0 means the record is free.
    pub ino: u32,
    /// `rec_len` — the step to the next record.
    pub rec: usize,
    /// The length of the name.
    pub nlen: usize,
    /// `file_type`.
    pub ft: u8,
}

/// The walk over a block's records. It stops at the first record that does not
/// fit the block or whose `rec_len` is less than the header: a broken directory
/// must end the walk rather than send it into foreign memory.
pub struct Walk<'a> {
    d: &'a [u8],
    o: usize,
}

impl<'a> Walk<'a> {
    pub fn new(d: &'a [u8]) -> Self { Walk { d, o: 0 } }
}

impl Iterator for Walk<'_> {
    type Item = Ent;
    fn next(&mut self) -> Option<Ent> {
        let (d, o) = (self.d, self.o);
        if o + 8 > d.len() { return None; }
        let rec = rd16(d, o + 4) as usize;
        if rec < 8 || o + rec > d.len() { return None; }
        let nlen = d[o + 6] as usize;
        self.o = o + rec;
        Some(Ent { off: o, ino: rd32(d, o), rec, nlen, ft: d[o + 7] })
    }
}

/// Does the record at `e` carry exactly this name? A name that does not fit into
/// its own record is a broken directory — such a record matches nothing.
#[inline]
fn name_is(d: &[u8], e: &Ent, name: &[u8]) -> bool {
    e.ino != 0 && e.nlen == name.len() && e.off + 8 + e.nlen <= e.off + e.rec && &d[e.off + 8..e.off + 8 + e.nlen] == name
}

/// Find a name in the block: `(the inode, the file_type)`.
pub fn find(d: &[u8], name: &[u8]) -> Option<(u32, u8)> {
    for e in Walk::new(d) {
        if name_is(d, &e, name) { return Some((e.ino, e.ft)); }
    }
    None
}

/// Put an entry into the block. `false` — there is no room in it (the caller
/// takes the next block or allocates one).
///
/// The room is looked for in two forms, as ext2 does: a free record (`inode == 0`)
/// is taken whole, while a live one is CUT — its `rec_len` is shrunk to what the
/// name really occupies, and the tail becomes the new record.
pub fn insert(d: &mut [u8], name: &[u8], ino: u32, ft: u8) -> bool {
    let want = need(name.len());
    let mut o = 0usize;
    while o + 8 <= d.len() {
        let eino = rd32(d, o);
        let rec = rd16(d, o + 4) as usize;
        let nlen = d[o + 6] as usize;
        if rec < 8 || o + rec > d.len() { break; }
        let used = if eino == 0 { 0 } else { need(nlen) };
        if rec >= used && rec - used >= want {
            let (no, nrec) = if eino == 0 {
                (o, rec)
            } else {
                wr16(d, o + 4, used as u16);
                (o + used, rec - used)
            };
            wr32(d, no, ino);
            wr16(d, no + 4, nrec as u16);
            d[no + 6] = name.len() as u8;
            d[no + 7] = ft;
            d[no + 8..no + 8 + name.len()].copy_from_slice(name);
            return true;
        }
        o += rec;
    }
    false
}

/// Remove a name from the block. `false` — it is not here.
///
/// The record is not erased: it is joined to the previous one (`rec_len` grows),
/// and a first record is marked free by a zero inode — that is exactly how a
/// linear directory keeps its walk unbroken.
pub fn remove(d: &mut [u8], name: &[u8]) -> bool {
    let mut prev: Option<usize> = None;
    let mut o = 0usize;
    while o + 8 <= d.len() {
        let rec = rd16(d, o + 4) as usize;
        if rec < 8 || o + rec > d.len() { break; }
        let e = Ent { off: o, ino: rd32(d, o), rec, nlen: d[o + 6] as usize, ft: d[o + 7] };
        if name_is(d, &e, name) {
            match prev {
                Some(p) => { let pr = rd16(d, p + 4) as usize; wr16(d, p + 4, (pr + rec) as u16); }
                None => wr32(d, o, 0),
            }
            return true;
        }
        prev = Some(o);
        o += rec;
    }
    false
}

/// Is there anything in the block besides `.` and `..` (a live record with any
/// other name)?
pub fn only_dots(d: &[u8]) -> bool {
    for e in Walk::new(d) {
        if e.ino == 0 { continue; }
        let dot = e.nlen == 1 && d[e.off + 8] == b'.';
        let dotdot = e.nlen == 2 && d[e.off + 8] == b'.' && d[e.off + 9] == b'.';
        if !dot && !dotdot { return false; }
    }
    true
}

/// A fresh block of a directory: one record for the whole block.
pub fn init(d: &mut [u8], name: &[u8], ino: u32, ft: u8) {
    for x in d.iter_mut() { *x = 0; }
    wr32(d, 0, ino);
    wr16(d, 4, d.len() as u16);
    d[6] = name.len() as u8;
    d[7] = ft;
    d[8..8 + name.len()].copy_from_slice(name);
}

#[cfg(test)]
mod tests {
    use super::*;

    const BS: usize = 1024;

    fn blk() -> [u8; BS] { [0u8; BS] }

    /// The records must tile the block exactly: every walk ends at the boundary,
    /// with no gaps and no overlap. A directory that fails this is the one where
    /// an entry "disappears" without any of the checks noticing.
    fn tiles(d: &[u8]) -> bool {
        let mut o = 0usize;
        for e in Walk::new(d) {
            if e.off != o { return false; }
            o += e.rec;
        }
        o == d.len()
    }

    #[test]
    fn need_is_aligned_and_fits_the_name() {
        for n in 0..=255usize {
            let k = need(n);
            assert_eq!(k % 4, 0, "rec_len must be a multiple of four (the name {})", n);
            assert!(k >= 8 + n, "a record of {} does not hold a name of {}", k, n);
            assert!(k < 8 + n + 4, "a record of {} wastes more than an alignment on a name of {}", k, n);
        }
    }

    #[test]
    fn init_gives_one_record_over_the_whole_block() {
        let mut d = blk();
        init(&mut d, b"hello.txt", 12, 1);
        assert!(tiles(&d));
        assert_eq!(find(&d, b"hello.txt"), Some((12, 1)));
        let e: Vec<Ent> = Walk::new(&d).collect();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].rec, BS);
    }

    #[test]
    fn insert_cuts_the_slack_of_a_live_record() {
        let mut d = blk();
        init(&mut d, b"a", 1, 1);           // one record over the whole block
        assert!(insert(&mut d, b"b", 2, 1));
        let e: Vec<Ent> = Walk::new(&d).collect();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].rec, need(1), "the donor keeps only what it really occupies");
        assert_eq!(e[1].rec, BS - need(1));
        assert_eq!(find(&d, b"a"), Some((1, 1)));
        assert_eq!(find(&d, b"b"), Some((2, 1)));
        assert!(tiles(&d));
    }

    #[test]
    fn insert_takes_a_hole_whole() {
        let mut d = blk();
        init(&mut d, b"a", 1, 1);
        assert!(insert(&mut d, b"bbbbbbbb", 2, 1));
        assert!(remove(&mut d, b"a"));      // the first record becomes a hole
        let before: Vec<Ent> = Walk::new(&d).collect();
        assert_eq!(before[0].ino, 0);
        assert!(insert(&mut d, b"c", 3, 2));
        let after: Vec<Ent> = Walk::new(&d).collect();
        assert_eq!(after[0].off, 0);
        assert_eq!(after[0].ino, 3, "the new entry took the hole");
        assert_eq!(after[0].rec, before[0].rec, "a hole is taken whole, it is not cut");
        assert_eq!(find(&d, b"c"), Some((3, 2)));
        assert_eq!(find(&d, b"bbbbbbbb"), Some((2, 1)));
        assert!(tiles(&d));
    }

    #[test]
    fn insert_refuses_when_the_block_is_full_and_spoils_nothing() {
        let mut d = blk();
        init(&mut d, b"first", 1, 1);
        let mut n = 1u32;
        // fill it up to the refusal
        while insert(&mut d, format!("name{:04}", n).as_bytes(), n + 1, 1) {
            n += 1;
            assert!(n < 200, "a block of 1 KB cannot hold that many");
        }
        assert!(tiles(&d), "a refusal must not leave a torn block");
        // everything that went in is still findable
        assert_eq!(find(&d, b"first"), Some((1, 1)));
        for i in 1..n {
            let nm = format!("name{:04}", i);
            assert_eq!(find(&d, nm.as_bytes()), Some((i + 1, 1)), "the entry {} was lost", nm);
        }
        // the free room really is less than the smallest record
        let free: usize = Walk::new(&d).map(|e| if e.ino == 0 { e.rec } else { e.rec - need(e.nlen) }).sum();
        assert!(free < need(1), "there is still room of {} — the refusal is premature", free);
    }

    #[test]
    fn remove_joins_the_record_to_the_previous_one() {
        let mut d = blk();
        init(&mut d, b"a", 1, 1);
        assert!(insert(&mut d, b"b", 2, 1));
        assert!(insert(&mut d, b"c", 3, 1));
        assert!(remove(&mut d, b"b"));
        assert_eq!(find(&d, b"b"), None);
        let e: Vec<Ent> = Walk::new(&d).collect();
        assert_eq!(e.len(), 2, "the removed record has been joined to its neighbour");
        assert_eq!(e[0].rec, need(1) * 2);
        assert_eq!(find(&d, b"a"), Some((1, 1)));
        assert_eq!(find(&d, b"c"), Some((3, 1)));
        assert!(tiles(&d));
    }

    #[test]
    fn remove_of_the_first_record_leaves_a_hole() {
        let mut d = blk();
        init(&mut d, b"a", 1, 1);
        assert!(insert(&mut d, b"b", 2, 1));
        assert!(remove(&mut d, b"a"));
        let e: Vec<Ent> = Walk::new(&d).collect();
        assert_eq!(e.len(), 2, "the first record stays in place as a hole");
        assert_eq!(e[0].ino, 0);
        assert_eq!(find(&d, b"a"), None);
        assert_eq!(find(&d, b"b"), Some((2, 1)));
        assert!(tiles(&d));
    }

    #[test]
    fn remove_of_a_missing_name_changes_nothing() {
        let mut d = blk();
        init(&mut d, b"a", 1, 1);
        assert!(insert(&mut d, b"b", 2, 1));
        let before = d;
        assert!(!remove(&mut d, b"zzz"));
        assert_eq!(&before[..], &d[..]);
    }

    #[test]
    fn a_name_that_is_a_prefix_of_another_is_a_different_name() {
        let mut d = blk();
        init(&mut d, b"file", 1, 1);
        assert!(insert(&mut d, b"file.txt", 2, 1));
        assert_eq!(find(&d, b"file"), Some((1, 1)));
        assert_eq!(find(&d, b"file.txt"), Some((2, 1)));
        assert_eq!(find(&d, b"fil"), None);
        assert_eq!(find(&d, b"file.tx"), None);
    }

    #[test]
    fn a_hole_is_not_found_by_the_name_that_lies_in_it() {
        let mut d = blk();
        init(&mut d, b"a", 1, 1);
        assert!(insert(&mut d, b"gone", 2, 1));
        assert!(insert(&mut d, b"c", 3, 1));
        assert!(remove(&mut d, b"gone"));
        // the bytes of the name are still in the block, but the record is not alive
        assert!(d.windows(4).any(|w| w == b"gone"), "the test itself is meaningless if the bytes are gone");
        assert_eq!(find(&d, b"gone"), None);
    }

    #[test]
    fn only_dots_sees_a_live_third_entry() {
        let mut d = blk();
        init(&mut d, b".", 2, 2);
        assert!(insert(&mut d, b"..", 2, 2));
        assert!(only_dots(&d));
        assert!(insert(&mut d, b"child", 7, 1));
        assert!(!only_dots(&d));
        assert!(remove(&mut d, b"child"));
        assert!(only_dots(&d), "a removed entry no longer holds the directory");
    }

    #[test]
    fn a_name_of_one_dot_is_not_a_dotdot() {
        let mut d = blk();
        init(&mut d, b".", 2, 2);
        assert!(insert(&mut d, b"..", 2, 2));
        assert!(insert(&mut d, b".x", 9, 1));
        assert!(!only_dots(&d), "'.x' is an ordinary name");
    }

    #[test]
    fn a_broken_rec_len_stops_the_walk_instead_of_leaving_the_block() {
        let mut d = blk();
        init(&mut d, b"a", 1, 1);
        assert!(insert(&mut d, b"b", 2, 1));
        // rec_len = 0: the step would be eternal
        wr16(&mut d, 4, 0);
        assert_eq!(Walk::new(&d).count(), 0);
        assert_eq!(find(&d, b"b"), None);
        assert!(!remove(&mut d, b"b"));
        assert!(only_dots(&d), "there is nothing to read — there is nothing to hold the directory");
        // rec_len beyond the block: the same
        let mut e = blk();
        init(&mut e, b"a", 1, 1);
        wr16(&mut e, 4, (BS + 4) as u16);
        assert_eq!(Walk::new(&e).count(), 0);
        assert!(!insert(&mut e, b"b", 2, 1));
    }

    #[test]
    fn a_name_longer_than_its_record_matches_nothing() {
        let mut d = blk();
        init(&mut d, b"ab", 1, 1);
        assert!(insert(&mut d, b"cd", 2, 1));
        // the first record says the name is 200 bytes while its rec_len is 12
        d[6] = 200;
        assert_eq!(find(&d, b"ab"), None);
        assert!(!remove(&mut d, &[b'x'; 200]));
    }

    #[test]
    fn the_records_stay_aligned_for_any_name_length() {
        for len in 1..=60usize {
            let name = vec![b'n'; len];
            let mut d = blk();
            init(&mut d, b"a", 1, 1);
            assert!(insert(&mut d, &name, 2, 1));
            for e in Walk::new(&d) {
                assert_eq!(e.off % 4, 0, "the record of the name of {} is not aligned", len);
                assert_eq!(e.rec % 4, 0);
            }
            assert_eq!(find(&d, &name), Some((2, 1)));
            assert!(tiles(&d));
        }
    }

    #[test]
    fn a_name_of_255_bytes_goes_in_and_comes_back() {
        let name = vec![b'z'; 255];
        let mut d = blk();
        init(&mut d, b"a", 1, 1);
        assert!(insert(&mut d, &name, 2, 1));
        assert_eq!(find(&d, &name), Some((2, 1)));
        assert!(tiles(&d));
    }

    /// Insertions and removals in a mixed order must not lose an entry and must
    /// not tear the block: that is exactly the load under which a directory of a
    /// database's data files lives.
    #[test]
    fn a_mixed_sequence_keeps_the_block_whole() {
        let mut d = blk();
        init(&mut d, b".", 2, 2);
        assert!(insert(&mut d, b"..", 2, 2));
        let mut live: Vec<(String, u32)> = Vec::new();
        let mut ino = 10u32;
        for step in 0..400u32 {
            if step % 3 != 2 {
                let nm = format!("f{:03}", step);
                if insert(&mut d, nm.as_bytes(), ino, 1) {
                    live.push((nm, ino));
                    ino += 1;
                }
            } else if !live.is_empty() {
                let (nm, _) = live.remove(live.len() / 2);
                assert!(remove(&mut d, nm.as_bytes()), "the entry {} has been lost", nm);
            }
            assert!(tiles(&d), "the block has been torn at the step {}", step);
        }
        for (nm, i) in &live {
            assert_eq!(find(&d, nm.as_bytes()), Some((*i, 1)), "{} is not found", nm);
        }
        assert_eq!(find(&d, b"."), Some((2, 2)));
        assert_eq!(find(&d, b".."), Some((2, 2)));
    }
}
