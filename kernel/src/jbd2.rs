//! The JBD2 log's format: the header of a log block and the tags of a
//! descriptor.
//!
//! Why a separate module. The journal is read not only by us: `e2fsck` and
//! Linux's kernel replay ours, and we replay theirs — so the bytes are not ours
//! to choose. And the arithmetic over them decides durability: a transaction is
//! written only if it FITS into the journal, and how much it takes is counted
//! before the writing. When the count was made by the tags alone, forgetting
//! that the descriptor and its copies go to the device in one batch, a
//! transaction of 1018 blocks was estimated at 3 descriptors while it occupied
//! 17 — it crossed the journal's end onto its own descriptor, the fsync
//! returned, and after a crash there was nothing to replay.
//!
//! Everything here is big-endian (that is the JBD2 format) and needs neither a
//! device nor a cache: the layout is written and read back by the tests.
//!
//! A log block:
//! ```text
//!   0  u32 h_magic (0xc03b3998)   4 u32 h_blocktype   8 u32 h_sequence
//!   a descriptor then carries the tags, 8 bytes each:
//!   0  u32 t_blocknr (where to put the copy)   4 u16 t_checksum   6 u16 t_flags
//!   after the FIRST tag come 16 bytes of the UUID, unless it says SAME_UUID
//! ```

/// The magic of a JBD2 log block.
pub const MAGIC: u32 = 0xc03b_3998;
/// A descriptor: the tags of the blocks whose copies follow it.
pub const BT_DESCRIPTOR: u32 = 1;
/// A commit: the transaction before it is whole.
pub const BT_COMMIT: u32 = 2;
/// The journal's superblock, version 2.
pub const BT_SB_V2: u32 = 4;

/// A tag of a `journal_block_tag_t` without the 64BIT feature.
pub const TAG_SZ: usize = 8;
/// The header of a log block.
pub const HDR: usize = 12;
/// The UUID after the first tag.
pub const UUID: usize = 16;

/// The block's first word coincided with the journal's magic and was zeroed — a
/// replay puts it back.
pub const F_ESCAPE: u16 = 1;
/// The tag carries no UUID.
pub const F_SAME_UUID: u16 = 2;
/// The last tag of the descriptor.
pub const F_LAST_TAG: u16 = 8;

#[inline]
fn rdbe32(b: &[u8], o: usize) -> u32 { u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }
#[inline]
fn rdbe16(b: &[u8], o: usize) -> u16 { u16::from_be_bytes([b[o], b[o + 1]]) }
#[inline]
fn wrbe32(b: &mut [u8], o: usize, v: u32) { b[o..o + 4].copy_from_slice(&v.to_be_bytes()); }
#[inline]
fn wrbe16(b: &mut [u8], o: usize, v: u16) { b[o..o + 2].copy_from_slice(&v.to_be_bytes()); }

/// How many blocks ONE descriptor really describes: as many tags as fit into a
/// block (the first one also carries the UUID, and one tag is kept in reserve),
/// but no more than fits into the device's batch.
///
/// `reqs` is the batch's size in records, slot 0 of which goes to the descriptor
/// itself.
pub fn per_desc(bs: usize, reqs: usize) -> usize {
    ((bs - HDR - UUID) / TAG_SZ - 1).min(reqs - 1).max(1)
}

/// How many log blocks a transaction of `want` blocks takes: the descriptors +
/// the copies + the commit.
pub fn need(bs: usize, reqs: usize, want: usize) -> usize {
    let pd = per_desc(bs, reqs);
    want.div_ceil(pd) + want + 1
}

/// Read a log block's header: `(the type, the sequence)`. `None` — this is not a
/// log block (there is no magic, or the buffer is shorter than the header).
pub fn header(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < HDR || rdbe32(b, 0) != MAGIC { return None; }
    Some((rdbe32(b, 4), rdbe32(b, 8)))
}

/// Lay a header into a log block (the block is not zeroed — the caller does that).
pub fn put_header(b: &mut [u8], btype: u32, seq: u32) {
    wrbe32(b, 0, MAGIC);
    wrbe32(b, 4, btype);
    wrbe32(b, 8, seq);
}

/// Is there room in the descriptor for one more tag, if the tag being laid is
/// `first`? One tag is kept in reserve, as the writer does.
pub fn tag_room(o: usize, bs: usize, first: bool) -> bool {
    let extra = if first { UUID } else { 0 };
    o + TAG_SZ + extra + TAG_SZ <= bs
}

/// Lay a tag at the offset `o` and return the offset of the next one. After the
/// first tag 16 bytes of the UUID are left (zeroes), as JBD2 requires.
pub fn put_tag(d: &mut [u8], o: usize, block: u32, first: bool, escape: bool) -> usize {
    wrbe32(d, o, block);
    wrbe16(d, o + 4, 0);            // t_checksum: without the csum feature it is ignored
    wrbe16(d, o + 6, if first { 0 } else { F_SAME_UUID } | if escape { F_ESCAPE } else { 0 });
    let mut n = o + TAG_SZ;
    if first {
        for k in 0..UUID { d[n + k] = 0; }
        n += UUID;
    }
    n
}

/// Mark the tag at the offset `o` as the last one of the descriptor.
pub fn mark_last(d: &mut [u8], o: usize) {
    let f = rdbe16(d, o + 6);
    wrbe16(d, o + 6, f | F_LAST_TAG);
}

/// The tags of a descriptor: `(the target block, the flags)` in the order of the
/// copies that follow it. The walk ends at the last tag or at the block's end —
/// a descriptor that lies does not send the replay past the buffer.
pub struct Tags<'a> {
    d: &'a [u8],
    bs: usize,
    o: usize,
    first: bool,
    done: bool,
}

impl<'a> Tags<'a> {
    pub fn new(d: &'a [u8], bs: usize) -> Self {
        Tags { d, bs: bs.min(d.len()), o: HDR, first: true, done: false }
    }
}

impl Iterator for Tags<'_> {
    type Item = (u32, u16);
    fn next(&mut self) -> Option<(u32, u16)> {
        if self.done || self.o + TAG_SZ > self.bs { return None; }
        let tgt = rdbe32(self.d, self.o);
        let flags = rdbe16(self.d, self.o + 6);
        self.o += TAG_SZ;
        if self.first && flags & F_SAME_UUID == 0 { self.o += UUID; }
        self.first = false;
        if flags & F_LAST_TAG != 0 { self.done = true; }
        Some((tgt, flags))
    }
}

// ── the replay of a journal left by a crash ──────────────────────────────────
//
// Why it lives here and not in `ext2.rs`. A replay is the only code of the FS
// whose mistake is invisible until a power cut: it runs at a mounting, over a
// journal nobody has read since, and it writes STRAIGHT to the volume's blocks.
// Apply one transaction too many and the metadata go back to a state the data
// blocks no longer match; stop one transaction too early and an fsync that has
// already returned is lost. Both look like a clean mounting. On the machine it
// is checked by pulling the plug (`powercut-*.sh`) — a test that costs minutes
// and reproduces one ordering out of many; here the journal is a byte array, so
// a torn tail, a wrap around the journal's end and a stale sequence are written
// by hand.
//
// The algorithm is `jbd2_journal_recover`'s: the first pass walks the chain
// descriptor -> copies -> commit while the sequence is the expected one and
// finds where the journal stops being intact; the second walks the same way and
// writes the copies in place. A transaction without its commit block is not
// applied at all — that is what makes an fsync atomic.

/// The circular log: the block size and the window of log blocks the
/// transactions live in (the log block 0 is the journal's superblock).
#[derive(Clone, Copy)]
pub struct Geom {
    pub bs: usize,
    /// `s_first`: the first block a transaction may occupy.
    pub first: usize,
    /// `s_maxlen`: one past the last usable log block.
    pub len: usize,
}

impl Geom {
    /// The next log block, wrapping at the journal's end onto `first`.
    #[inline]
    pub fn next(&self, i: usize) -> usize { let n = i + 1; if n >= self.len { self.first } else { n } }
}

/// What a replay needs from the machine: the journal's own blocks and a write in
/// place. Nothing else — no cache, no allocator, no inode.
pub trait Media {
    /// Read the log block `i`. `false` — it could not be read; the replay stops
    /// there rather than applying what it has not seen.
    fn log_read(&mut self, i: usize, out: &mut [u8]) -> bool;
    /// Write a copy to its place in the filesystem (the block 0 is a lawful
    /// target: the FS's superblock lies in it).
    fn fs_write(&mut self, block: u32, data: &[u8]);
}

/// What a replay did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Replayed {
    /// The intact transactions found and applied.
    pub txns: u32,
    /// The sequence the next transaction must take.
    pub next_seq: u32,
    /// The copies written in place.
    pub blocks: u64,
}

/// One pass from the log block `start` with the expected sequence `seq`. `end`
/// absent — the SCAN pass: it writes nothing and only finds the sequence after
/// the last intact commit. `end` present — the REPLAY pass: it writes the copies
/// in place and stops when it reaches that sequence.
///
/// The bound is the whole point of the second pass. Behind the last commit
/// there may lie a descriptor of the transaction the crash interrupted: its
/// sequence is the expected one (the block did land), while its copies did not
/// necessarily land — the barrier that orders them stands before the COMMIT, and
/// there is no commit. Without the bound the replay wrote those copies in place,
/// that is, put the content of somebody else's former epoch over live metadata.
/// `jbd2_journal_recover` bounds `PASS_REPLAY` by `info->end_transaction` for
/// exactly this reason.
fn pass(m: &mut impl Media, g: &Geom, start: usize, seq: u32, end: Option<u32>, hdr: &mut [u8], cpy: &mut [u8]) -> (u32, u64) {
    let (mut i, mut want, mut applied) = (start, seq, 0u64);
    let apply = end.is_some();
    loop {
        if end == Some(want) { break; }                  // the last commit has been passed
        if !m.log_read(i, hdr) { break; }
        let Some((btype, bseq)) = header(&hdr[..g.bs]) else { break };
        if bseq != want { break; }                       // a stale tail: a block of a former epoch
        if btype == BT_COMMIT { want = want.wrapping_add(1); i = g.next(i); continue; }
        if btype != BT_DESCRIPTOR { break; }
        let mut data = g.next(i);
        for (tgt, flags) in Tags::new(&hdr[..g.bs], g.bs) {
            if apply && m.log_read(data, cpy) {
                // the escaping: a block whose first word coincided with the journal's
                // magic was written with it zeroed — we put it back
                if flags & F_ESCAPE != 0 { wrbe32(cpy, 0, MAGIC); }
                m.fs_write(tgt, &cpy[..g.bs]);
                applied += 1;
            }
            data = g.next(data);
        }
        i = data;
    }
    (want, applied)
}

/// Replay the journal from the log block `start`, the expected sequence `seq`.
/// `hdr` and `cpy` are two buffers of at least a block each (the kernel gives
/// its staging pages; there is no allocation here).
///
/// Two passes on purpose: the tail of a journal after a crash is arbitrary — a
/// descriptor with its copies but without a commit, a half-written commit, the
/// blocks of a former epoch under the same numbers. Applying as we walk would
/// write the copies of a transaction whose commit never came.
pub fn recover(m: &mut impl Media, g: &Geom, start: usize, seq: u32, hdr: &mut [u8], cpy: &mut [u8]) -> Replayed {
    let (want, _) = pass(m, g, start, seq, None, hdr, cpy);
    if want == seq { return Replayed { txns: 0, next_seq: seq, blocks: 0 }; }
    let (w2, applied) = pass(m, g, start, seq, Some(want), hdr, cpy);
    Replayed { txns: w2.wrapping_sub(seq), next_seq: w2, blocks: applied }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQS: usize = 64;

    /// Lay out `n` tags into a descriptor exactly as the writer does: it stops
    /// both at the block's end and at the device's batch (slot 0 of the batch
    /// goes to the descriptor itself). It returns how many really fitted.
    fn fill_reqs(d: &mut [u8], bs: usize, n: usize, seq: u32, reqs: usize) -> usize {
        for b in d[..bs].iter_mut() { *b = 0; }
        put_header(d, BT_DESCRIPTOR, seq);
        let (mut o, mut first, mut last, mut k) = (HDR, true, 0usize, 0usize);
        while k < n && k + 1 < reqs && tag_room(o, bs, first) {
            last = o;
            o = put_tag(d, o, 1000 + k as u32, first, false);
            first = false;
            k += 1;
        }
        if k > 0 { mark_last(d, last); }
        k
    }

    fn fill(d: &mut [u8], bs: usize, n: usize, seq: u32) -> usize {
        fill_reqs(d, bs, n, seq, REQS)
    }

    #[test]
    fn a_header_is_read_back_and_a_foreign_block_is_not_taken() {
        let mut b = [0u8; 1024];
        put_header(&mut b, BT_COMMIT, 42);
        assert_eq!(header(&b), Some((BT_COMMIT, 42)));
        b[0] ^= 0xFF;
        assert_eq!(header(&b), None, "without the magic it is not a log block");
        assert_eq!(header(&[0u8; 4]), None, "a buffer shorter than the header");
    }

    #[test]
    fn the_header_is_big_endian_as_jbd2_requires() {
        let mut b = [0u8; 64];
        put_header(&mut b, BT_DESCRIPTOR, 1);
        assert_eq!(&b[0..4], &[0xc0, 0x3b, 0x39, 0x98], "the magic goes big-endian");
        assert_eq!(&b[8..12], &[0, 0, 0, 1], "so does the sequence");
    }

    #[test]
    fn the_tags_come_back_in_the_order_they_were_laid() {
        let mut d = [0u8; 1024];
        let n = fill(&mut d, 1024, 5, 7);
        assert_eq!(n, 5);
        let got: Vec<(u32, u16)> = Tags::new(&d, 1024).collect();
        assert_eq!(got.len(), 5);
        for (k, (tgt, _)) in got.iter().enumerate() {
            assert_eq!(*tgt, 1000 + k as u32, "the tag {} points at a foreign block", k);
        }
        assert_eq!(got[0].1 & F_SAME_UUID, 0, "the first tag carries the UUID");
        assert!(got[1..].iter().all(|(_, f)| f & F_SAME_UUID != 0), "the rest do not");
        assert!(got.last().unwrap().1 & F_LAST_TAG != 0, "the last tag says it is the last");
        assert!(got[..4].iter().all(|(_, f)| f & F_LAST_TAG == 0));
    }

    #[test]
    fn a_single_tag_is_both_the_first_and_the_last() {
        let mut d = [0u8; 4096];
        assert_eq!(fill(&mut d, 4096, 1, 3), 1);
        let got: Vec<(u32, u16)> = Tags::new(&d, 4096).collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 1000);
        assert_eq!(got[0].1 & F_SAME_UUID, 0);
        assert!(got[0].1 & F_LAST_TAG != 0);
    }

    #[test]
    fn the_escape_flag_survives_the_round_trip() {
        let mut d = [0u8; 1024];
        for b in d.iter_mut() { *b = 0; }
        put_header(&mut d, BT_DESCRIPTOR, 1);
        let o = put_tag(&mut d, HDR, 77, true, true);
        let o2 = put_tag(&mut d, o, 78, false, false);
        mark_last(&mut d, o);
        let _ = o2;
        let got: Vec<(u32, u16)> = Tags::new(&d, 1024).collect();
        assert_eq!(got[0].0, 77);
        assert!(got[0].1 & F_ESCAPE != 0, "the block with the magic inside is marked");
        assert!(got.len() == 2 && got[1].1 & F_LAST_TAG != 0);
    }

    /// The count of the room and the laying out must agree: a descriptor holds
    /// exactly as many tags as `per_desc` promises. It is the disagreement of
    /// these two that once cost durability.
    #[test]
    fn a_descriptor_holds_exactly_as_many_tags_as_promised() {
        for &bs in &[1024usize, 2048, 4096] {
            for &reqs in &[8usize, 64, 1024] {
                let mut d = vec![0u8; bs];
                let laid = fill_reqs(&mut d, bs, 10_000, 1, reqs);
                assert_eq!(laid, per_desc(bs, reqs),
                           "a block of {} with a batch of {}: {} tags were laid, {} were counted",
                           bs, reqs, laid, per_desc(bs, reqs));
                assert_eq!(Tags::new(&d, bs).count(), laid, "the walk sees exactly the laid tags");
            }
        }
    }

    #[test]
    fn the_batch_limits_a_descriptor_no_less_than_the_block_does() {
        // a block of 4K holds 494 tags, but a batch of 64 records — 63
        assert_eq!(per_desc(4096, 64), 63);
        assert_eq!(per_desc(1024, 64), 63);
        // a small batch cuts it further; a batch of one record still leaves a tag
        assert_eq!(per_desc(4096, 8), 7);
        assert_eq!(per_desc(4096, 1), 1);
        // a block of 1 KB by itself holds (1024 - 12 - 16)/8 - 1 = 123
        assert_eq!(per_desc(1024, 1024), 123);
    }

    /// The very case of §3.2: a transaction of 1018 blocks in a journal of 1024.
    /// Counted by the tags alone it "fits"; counted by the batch it does not.
    #[test]
    fn a_transaction_of_a_thousand_blocks_is_counted_by_the_batch() {
        let pd = per_desc(4096, 64);
        assert_eq!(pd, 63);
        let n = need(4096, 64, 1018);
        assert_eq!(n, 1018usize.div_ceil(63) + 1018 + 1, "the descriptors + the copies + the commit");
        assert_eq!(n, 17 + 1018 + 1);
        assert!(n > 1024, "such a transaction does NOT fit into a journal of 1024 blocks");
        // counted by the tags of a 4K block alone it would look as if it fits
        let by_tags = 1018usize.div_ceil((4096 - HDR - UUID) / TAG_SZ - 1) + 1018 + 1;
        assert!(by_tags <= 1024, "that is exactly the count that used to deceive");
    }

    #[test]
    fn the_need_of_a_transaction_covers_its_pieces() {
        for &want in &[1usize, 2, 62, 63, 64, 125, 126, 1000] {
            let n = need(4096, 64, want);
            let descs = want.div_ceil(per_desc(4096, 64));
            assert_eq!(n, descs + want + 1);
            assert!(n >= want + 2, "a transaction is at least a descriptor, a copy and a commit");
        }
        assert_eq!(need(4096, 64, 0), 1, "an empty transaction is a commit alone");
    }

    #[test]
    fn a_descriptor_that_lies_does_not_send_the_walk_past_the_block() {
        let mut d = [0u8; 1024];
        let n = fill(&mut d, 1024, 4, 1);
        assert_eq!(n, 4);
        // the last tag's flag is lost: the walk must stop at the block's end
        let mut broken = d;
        for o in (HDR..1024 - TAG_SZ).step_by(TAG_SZ) { wrbe16(&mut broken, o + 6, F_SAME_UUID); }
        let got = Tags::new(&broken, 1024).count();
        assert!(got > 0 && got <= (1024 - HDR) / TAG_SZ, "the walk is bounded by the block");
        // a block shorter than the declared one: the walk is bounded by the buffer
        assert!(Tags::new(&d[..HDR + 4], 1024).count() == 0);
    }

    #[test]
    fn an_empty_descriptor_gives_no_tags() {
        let mut d = [0u8; 1024];
        for b in d.iter_mut() { *b = 0; }
        put_header(&mut d, BT_DESCRIPTOR, 5);
        // the first tag reads as a block 0 without flags — the walk gives it out but does not
        // go beyond the block; a real writer always marks the last tag, and the
        // replay of such a descriptor is bounded by the block's size
        assert!(Tags::new(&d, 1024).count() <= (1024 - HDR) / TAG_SZ);
        assert_eq!(header(&d), Some((BT_DESCRIPTOR, 5)));
    }
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    use std::collections::BTreeMap;

    const BS: usize = 1024;
    const REQS: usize = 8;                  // a small batch: a transaction takes several descriptors

    /// A journal and a volume in memory. The journal's blocks are addressed by the
    /// LOG index (0 is the journal's superblock), the volume's by the FS block.
    struct Img {
        bs: usize,
        log: Vec<Vec<u8>>,
        fs: BTreeMap<u32, Vec<u8>>,
        bad: Option<usize>,                 // this log block cannot be read (a media failure)
        reads: usize,
    }

    impl Media for Img {
        fn log_read(&mut self, i: usize, out: &mut [u8]) -> bool {
            self.reads += 1;
            if self.bad == Some(i) || i >= self.log.len() { return false; }
            out[..self.bs].copy_from_slice(&self.log[i]);
            true
        }
        fn fs_write(&mut self, block: u32, data: &[u8]) { self.fs.insert(block, data.to_vec()); }
    }

    impl Img {
        fn new(bs: usize, len: usize) -> Img {
            Img { bs, log: vec![vec![0u8; bs]; len], fs: BTreeMap::new(), bad: None, reads: 0 }
        }

        /// Write a transaction as the kernel's writer does: descriptors with the
        /// tags, the copies right after each descriptor, the commit at the end.
        /// `commit` off leaves the transaction without its commit block — that is
        /// what a crash in the middle of an fsync leaves behind.
        fn txn(&mut self, g: &Geom, mut pos: usize, seq: u32, blocks: &[(u32, Vec<u8>)], commit: bool) -> usize {
            let pd = per_desc(g.bs, REQS);
            let mut k = 0usize;
            while k < blocks.len() {
                let n = pd.min(blocks.len() - k);
                let dpos = pos;
                let mut d = vec![0u8; g.bs];
                put_header(&mut d, BT_DESCRIPTOR, seq);
                let (mut o, mut first, mut last) = (HDR, true, 0usize);
                pos = g.next(pos);
                for (tgt, payload) in &blocks[k..k + n] {
                    assert!(tag_room(o, g.bs, first), "the test laid out more tags than a descriptor holds");
                    let mut copy = payload.clone();
                    copy.resize(g.bs, 0);
                    let esc = u32::from_be_bytes([copy[0], copy[1], copy[2], copy[3]]) == MAGIC;
                    if esc { wrbe32(&mut copy, 0, 0); }   // the magic is zeroed in the journal
                    last = o;
                    o = put_tag(&mut d, o, *tgt, first, esc);
                    first = false;
                    self.log[pos] = copy;
                    pos = g.next(pos);
                }
                mark_last(&mut d, last);
                self.log[dpos] = d;
                k += n;
            }
            if commit {
                let mut c = vec![0u8; g.bs];
                put_header(&mut c, BT_COMMIT, seq);
                self.log[pos] = c;
                pos = g.next(pos);
            }
            pos
        }

        fn replay(&mut self, g: &Geom, start: usize, seq: u32) -> Replayed {
            let (mut hdr, mut cpy) = (vec![0u8; self.bs], vec![0u8; self.bs]);
            // the buffers are longer than a block, as the kernel's staging pages are
            hdr.resize(4096, 0); cpy.resize(4096, 0);
            recover(self, g, start, seq, &mut hdr, &mut cpy)
        }
    }

    fn geom(len: usize) -> Geom { Geom { bs: BS, first: 1, len } }
    fn payload(tag: u8) -> Vec<u8> { vec![tag; BS] }

    #[test]
    fn a_committed_transaction_is_applied_byte_for_byte() {
        let g = geom(64);
        let mut img = Img::new(BS, 64);
        let blocks = vec![(100u32, payload(0xA1)), (200, payload(0xA2)), (0, payload(0xA3))];
        img.txn(&g, 1, 7, &blocks, true);
        let r = img.replay(&g, 1, 7);
        assert_eq!((r.txns, r.next_seq, r.blocks), (1, 8, 3));
        for (b, p) in &blocks {
            assert_eq!(img.fs.get(b).map(|v| &v[..]), Some(&p[..]), "the block {} came back wrong", b);
        }
        assert_eq!(img.fs.len(), 3, "nothing else was written");
        assert!(img.fs.contains_key(&0), "the block 0 is a lawful target: the FS's superblock is in it");
    }

    #[test]
    fn a_transaction_without_a_commit_is_not_applied_at_all() {
        let g = geom(64);
        let mut img = Img::new(BS, 64);
        img.txn(&g, 1, 7, &[(100u32, payload(0xB1)), (200, payload(0xB2))], false);
        let r = img.replay(&g, 1, 7);
        assert_eq!((r.txns, r.next_seq, r.blocks), (0, 7, 0));
        assert!(img.fs.is_empty(), "a crash before the commit must leave the volume alone");
    }

    #[test]
    fn the_intact_prefix_is_applied_and_the_torn_tail_is_not() {
        let g = geom(64);
        let mut img = Img::new(BS, 64);
        let mut pos = 1usize;
        pos = img.txn(&g, pos, 7, &[(10u32, payload(1)), (11, payload(2))], true);
        pos = img.txn(&g, pos, 8, &[(12u32, payload(3))], true);
        img.txn(&g, pos, 9, &[(10u32, payload(0xFF)), (13, payload(0xFF))], false);   // it did not make it
        let r = img.replay(&g, 1, 7);
        assert_eq!((r.txns, r.next_seq, r.blocks), (2, 9, 3));
        assert_eq!(img.fs[&10][0], 1, "the block of the torn transaction must keep the OLD copy");
        assert_eq!(img.fs[&11][0], 2);
        assert_eq!(img.fs[&12][0], 3);
        assert!(!img.fs.contains_key(&13), "a block only the torn transaction touched");
    }

    /// The defect this module was written for. A crash between the batch
    /// "descriptor + copies" and the commit may leave the DESCRIPTOR on the disk
    /// while the copies behind it are whatever lay in those log blocks before.
    /// Its sequence is the expected one, so the replay walks into it — and if the
    /// applying pass is not bounded by the last commit, it writes that former
    /// content over live metadata.
    #[test]
    fn the_copies_of_an_uncommitted_descriptor_are_never_written_in_place() {
        let g = geom(64);
        let mut img = Img::new(BS, 64);
        let pos = img.txn(&g, 1, 7, &[(10u32, payload(1))], true);
        // the interrupted transaction: only its descriptor landed
        let end = img.txn(&g, pos, 8, &[(10u32, payload(0xEE)), (99, payload(0xEE))], false);
        for i in pos + 1..end { img.log[i] = vec![0xCD; BS]; }   // the copies did not land
        let r = img.replay(&g, 1, 7);
        assert_eq!((r.txns, r.next_seq, r.blocks), (1, 8, 1));
        assert_eq!(img.fs[&10][0], 1, "the live block must keep the committed copy");
        assert!(!img.fs.contains_key(&99), "a block named only by an uncommitted descriptor");
        assert!(img.fs.values().all(|v| v[0] != 0xCD), "the journal's former content went to the volume");
    }

    #[test]
    fn a_stale_tail_of_a_former_epoch_stops_the_replay() {
        let g = geom(64);
        let mut img = Img::new(BS, 64);
        let pos = img.txn(&g, 1, 7, &[(10u32, payload(1))], true);
        // the journal has been reused: at this place there lie the blocks of a transaction
        // with a FOREIGN sequence (a former epoch, or a later one after a checkpoint)
        img.txn(&g, pos, 12, &[(20u32, payload(9))], true);
        let r = img.replay(&g, 1, 7);
        assert_eq!((r.txns, r.next_seq), (1, 8));
        assert_eq!(img.fs.len(), 1);
        assert!(!img.fs.contains_key(&20), "a transaction with a foreign sequence is not ours");
    }

    #[test]
    fn a_block_that_starts_with_the_journals_magic_comes_back_whole() {
        let g = geom(64);
        let mut img = Img::new(BS, 64);
        let mut p = payload(0x5A);
        wrbe32(&mut p, 0, MAGIC);                       // the FS's block that looks like a log block
        img.txn(&g, 1, 3, &[(77u32, p.clone())], true);
        // in the journal the copy lies with the magic zeroed — otherwise the replay itself
        // would take it for a descriptor
        assert_ne!(u32::from_be_bytes([img.log[2][0], img.log[2][1], img.log[2][2], img.log[2][3]]), MAGIC);
        let r = img.replay(&g, 1, 3);
        assert_eq!(r.blocks, 1);
        assert_eq!(img.fs[&77], p, "the escaped word was not put back");
    }

    #[test]
    fn a_transaction_that_wraps_around_the_journals_end_is_replayed() {
        let len = 12usize;                              // a very short journal: 1..11
        let g = geom(len);
        let mut img = Img::new(BS, len);
        let blocks: Vec<(u32, Vec<u8>)> = (0..4u32).map(|i| (50 + i, payload(i as u8 + 1))).collect();
        let start = 9usize;                             // the transaction does not fit to the end
        let end = img.txn(&g, start, 5, &blocks, true);
        assert!(end < start, "the test is meaningless if the transaction did not wrap");
        let r = img.replay(&g, start, 5);
        assert_eq!((r.txns, r.blocks), (1, 4));
        for (i, (b, p)) in blocks.iter().enumerate() {
            assert_eq!(img.fs.get(b).map(|v| v[0]), Some(p[0]), "the block {} (the copy {}) was lost at the wrap", b, i);
        }
        assert!(img.log[0].iter().all(|&x| x == 0), "the journal's superblock must not be walked over");
    }

    #[test]
    fn a_transaction_of_several_descriptors_is_replayed_whole() {
        let g = geom(128);
        let mut img = Img::new(BS, 128);
        let pd = per_desc(BS, REQS);
        let n = pd * 3 + 2;                             // four descriptors
        let blocks: Vec<(u32, Vec<u8>)> = (0..n as u32).map(|i| (1000 + i, payload((i % 251) as u8 + 1))).collect();
        img.txn(&g, 1, 4, &blocks, true);
        let r = img.replay(&g, 1, 4);
        assert_eq!((r.txns, r.blocks), (1, n as u64));
        assert_eq!(img.fs.len(), n);
        for (b, p) in &blocks { assert_eq!(img.fs[b][0], p[0], "the block {}", b); }
    }

    #[test]
    fn a_replay_repeated_gives_the_same_volume() {
        let g = geom(64);
        let mut img = Img::new(BS, 64);
        let mut pos = 1usize;
        pos = img.txn(&g, pos, 7, &[(10u32, payload(1)), (11, payload(2))], true);
        img.txn(&g, pos, 8, &[(10u32, payload(3))], true);
        let first = img.replay(&g, 1, 7);
        let after = img.fs.clone();
        let second = img.replay(&g, 1, 7);
        assert_eq!(first, second, "a replay must be repeatable: a crash may happen during one");
        assert_eq!(after, img.fs);
        assert_eq!(img.fs[&10][0], 3, "the LAST copy of the block wins");
    }

    #[test]
    fn an_unreadable_log_block_stops_the_replay_instead_of_guessing() {
        let g = geom(64);
        let mut img = Img::new(BS, 64);
        let mut pos = 1usize;
        pos = img.txn(&g, pos, 7, &[(10u32, payload(1))], true);
        img.txn(&g, pos, 8, &[(20u32, payload(2))], true);
        img.bad = Some(pos);                            // the second transaction's descriptor is not read
        let r = img.replay(&g, 1, 7);
        assert_eq!((r.txns, r.next_seq), (1, 8));
        assert_eq!(img.fs.len(), 1);
        assert!(!img.fs.contains_key(&20));
    }

    #[test]
    fn a_journal_without_a_log_block_at_the_start_replays_nothing() {
        let g = geom(64);
        let mut img = Img::new(BS, 64);
        img.log[1] = vec![0x77; BS];                    // garbage instead of a descriptor
        let r = img.replay(&g, 1, 7);
        assert_eq!((r.txns, r.next_seq, r.blocks), (0, 7, 0));
        assert!(img.fs.is_empty());
        // and a block of the right shape but of a foreign type is not a transaction either
        let mut img2 = Img::new(BS, 64);
        put_header(&mut img2.log[1], BT_SB_V2, 7);
        assert_eq!(img2.replay(&g, 1, 7).txns, 0);
    }

    #[test]
    fn the_sequence_wraps_around_without_losing_a_transaction() {
        let g = geom(64);
        let mut img = Img::new(BS, 64);
        let seq = u32::MAX;                             // the next one is 0
        let mut pos = 1usize;
        pos = img.txn(&g, pos, seq, &[(10u32, payload(1))], true);
        img.txn(&g, pos, 0, &[(11u32, payload(2))], true);
        let r = img.replay(&g, 1, seq);
        assert_eq!((r.txns, r.next_seq, r.blocks), (2, 1, 2));
        assert_eq!(img.fs.len(), 2);
    }

    #[test]
    fn the_first_pass_writes_nothing() {
        let g = geom(64);
        let mut img = Img::new(BS, 64);
        img.txn(&g, 1, 7, &[(10u32, payload(1))], false);   // there is nothing to apply
        let before = img.fs.clone();
        let r = img.replay(&g, 1, 7);
        assert_eq!(r.txns, 0);
        assert_eq!(before, img.fs);
        assert!(img.reads > 0, "the journal was read");
    }

    #[test]
    fn the_wrap_of_the_log_never_touches_the_journals_superblock() {
        for len in 4..40usize {
            let g = geom(len);
            let mut i = g.first;
            for _ in 0..len * 3 {
                i = g.next(i);
                assert!(i >= g.first && i < g.len, "the log block {} is outside [{}, {})", i, g.first, g.len);
            }
        }
    }
}
