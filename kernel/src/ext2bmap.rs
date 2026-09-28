//! The bitmaps of ext2: a bit per block (or per inode) of a group, plus the
//! arithmetic of a group's size.
//!
//! Why a separate module. A mistake here does not crash and does not show up in
//! any counter: a bit taken twice means two files sharing a block, a bit lost
//! means space that never comes back. Both are found by an `e2fsck` after the
//! run — that is, long after the write that caused them. The scan and the
//! marking need neither a cache nor a device, so they are judged here by tests.
//!
//! The scan is a WRAP-AROUND one, from a cursor: the allocator keeps, per group,
//! the place where the previous search ended, and the next one starts there and
//! goes round. Without the cursor every block of a long file rescans the group
//! from its start — the 5000th block walks 5000 bits (§2.4).

/// Is the bit `i` set?
#[inline]
pub fn get(bm: &[u8], i: usize) -> bool { bm[i / 8] & (1 << (i % 8)) != 0 }

/// Set or clear the bit `i`.
#[inline]
pub fn set(bm: &mut [u8], i: usize, v: bool) {
    if v { bm[i / 8] |= 1 << (i % 8); } else { bm[i / 8] &= !(1 << (i % 8)); }
}

/// The first free bit, starting at `hint` and wrapping round to it. `None` —
/// there is no free bit among the first `nbits`.
///
/// A hint beyond the bitmap is not an error: the allocator keeps it per group
/// modulo the number of the groups, so a collision merely costs an extra scan.
pub fn find_free(bm: &[u8], nbits: usize, hint: usize) -> Option<usize> {
    let hint = hint.min(nbits);
    for (lo, hi) in [(hint, nbits), (0, hint)] {
        for i in lo..hi {
            if !get(bm, i) { return Some(i); }
        }
    }
    None
}

/// Mark as taken up to `want` free bits in a row starting at `start`. It returns
/// how many have been taken: the run is cut short by the first bit that is
/// already taken and by the end of the group.
pub fn take_run(bm: &mut [u8], nbits: usize, start: usize, want: u32) -> u32 {
    let mut n = 0u32;
    while n < want && start + (n as usize) < nbits && !get(bm, start + n as usize) {
        set(bm, start + n as usize, true);
        n += 1;
    }
    n
}

/// How many bits (blocks) the group `g` holds: every group has `per_group` of
/// them except the last one, which is cut by the volume's end.
///
/// `total` is the number of the volume's blocks, `first_data` is the first block
/// covered by the maps (1 with a block of 1 KB, 0 otherwise). A group beyond the
/// volume has no bits at all — that is the answer, not an underflow.
pub fn group_bits(total: u32, first_data: u32, per_group: u32, g: usize) -> usize {
    let covered = total.saturating_sub(first_data);
    let start = (g as u32).saturating_mul(per_group);
    covered.saturating_sub(start).min(per_group) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bitmap of `nbits` free bits (the tail of the last byte is zero too).
    fn empty(nbits: usize) -> Vec<u8> { vec![0u8; nbits.div_ceil(8)] }

    #[test]
    fn a_bit_reads_back_as_it_was_written() {
        let mut bm = empty(64);
        for i in 0..64 {
            assert!(!get(&bm, i));
            set(&mut bm, i, true);
            assert!(get(&bm, i));
        }
        for i in 0..64 {
            set(&mut bm, i, false);
            assert!(!get(&bm, i));
        }
        assert!(bm.iter().all(|&b| b == 0));
    }

    #[test]
    fn setting_a_bit_touches_no_neighbour() {
        let mut bm = empty(32);
        set(&mut bm, 9, true);
        for i in 0..32 {
            assert_eq!(get(&bm, i), i == 9, "the bit {} has changed along with the 9th", i);
        }
        set(&mut bm, 9, false);
        assert!(bm.iter().all(|&b| b == 0));
    }

    #[test]
    fn the_bit_order_inside_a_byte_is_the_one_ext2_uses() {
        // ext2 numbers the bits from the low one: the block 0 is 0x01, the block 7 is 0x80.
        let mut bm = empty(16);
        set(&mut bm, 0, true);
        assert_eq!(bm[0], 0x01);
        set(&mut bm, 7, true);
        assert_eq!(bm[0], 0x81);
        set(&mut bm, 8, true);
        assert_eq!(bm[1], 0x01);
    }

    #[test]
    fn the_scan_starts_at_the_cursor_and_goes_round() {
        let mut bm = empty(64);
        for i in 0..20 { set(&mut bm, i, true); }
        assert_eq!(find_free(&bm, 64, 0), Some(20));
        assert_eq!(find_free(&bm, 64, 40), Some(40), "the cursor's own place is free");
        // everything from the cursor to the end is taken — the search goes round
        for i in 40..64 { set(&mut bm, i, true); }
        assert_eq!(find_free(&bm, 64, 40), Some(20));
    }

    #[test]
    fn a_cursor_beyond_the_bitmap_is_not_an_error() {
        let mut bm = empty(64);
        for i in 0..30 { set(&mut bm, i, true); }
        assert_eq!(find_free(&bm, 64, 10_000), Some(30), "a hint out of range = a scan from the start");
        assert_eq!(find_free(&bm, 64, 64), Some(30));
    }

    #[test]
    fn a_full_group_gives_no_room_however_it_is_scanned() {
        let mut bm = empty(64);
        for i in 0..64 { set(&mut bm, i, true); }
        for hint in [0usize, 1, 31, 63, 64, 999] {
            assert_eq!(find_free(&bm, 64, hint), None, "the hint {}", hint);
        }
    }

    #[test]
    fn the_bits_beyond_the_group_are_not_looked_at() {
        let mut bm = empty(64);
        for i in 0..40 { set(&mut bm, i, true); }
        // the group is shorter than the bitmap's byte: the free bits 40..64 are outside it
        assert_eq!(find_free(&bm, 40, 0), None);
        assert_eq!(find_free(&bm, 41, 0), Some(40));
    }

    #[test]
    fn a_run_is_taken_in_full_when_there_is_room() {
        let mut bm = empty(64);
        assert_eq!(take_run(&mut bm, 64, 8, 16), 16);
        for i in 0..64 {
            assert_eq!(get(&bm, i), (8..24).contains(&i), "the bit {}", i);
        }
    }

    #[test]
    fn a_run_is_cut_by_a_taken_bit_and_by_the_end_of_the_group() {
        let mut bm = empty(64);
        set(&mut bm, 12, true);
        assert_eq!(take_run(&mut bm, 64, 8, 16), 4, "the run stops before the taken bit");
        assert!(get(&bm, 11) && get(&bm, 12) && !get(&bm, 13));

        let mut bm2 = empty(64);
        assert_eq!(take_run(&mut bm2, 40, 36, 16), 4, "the run stops at the end of the group");
        assert!(!get(&bm2, 40), "a bit beyond the group must not be taken");
    }

    #[test]
    fn a_run_on_a_taken_bit_takes_nothing() {
        let mut bm = empty(64);
        set(&mut bm, 8, true);
        let before = bm.clone();
        assert_eq!(take_run(&mut bm, 64, 8, 16), 0);
        assert_eq!(before, bm, "a refusal must not change the map");
    }

    #[test]
    fn a_zero_run_and_a_start_beyond_the_group_take_nothing() {
        let mut bm = empty(64);
        assert_eq!(take_run(&mut bm, 64, 8, 0), 0);
        assert_eq!(take_run(&mut bm, 64, 64, 8), 0);
        assert_eq!(take_run(&mut bm, 64, 999, 8), 0);
        assert!(bm.iter().all(|&b| b == 0));
    }

    /// The allocator's cycle: scan -> take a run -> move the cursor. Whatever the
    /// order of the requests, a block must not be handed out twice, and what has
    /// been freed must come back.
    #[test]
    fn the_scan_and_the_run_hand_out_every_block_exactly_once() {
        const N: usize = 200;
        let mut bm = empty(N);
        let mut hint = 0usize;
        let mut given: Vec<usize> = Vec::new();
        let mut want = 1u32;
        while let Some(start) = find_free(&bm, N, hint) {
            let n = take_run(&mut bm, N, start, want);
            assert!(n > 0, "the scan found a free bit but the run took nothing");
            for k in 0..n as usize { given.push(start + k); }
            hint = start + n as usize;
            want = (want * 2).min(16);
        }
        given.sort_unstable();
        assert_eq!(given.len(), N, "some blocks have not been handed out");
        assert!(given.windows(2).all(|w| w[0] + 1 == w[1]), "a block has been handed out twice");
        // free every third one — it must become findable again
        for i in (0..N).step_by(3) { set(&mut bm, i, false); }
        assert_eq!(find_free(&bm, N, 0), Some(0));
        assert_eq!(find_free(&bm, N, 1), Some(3));
    }

    #[test]
    fn the_last_group_is_cut_by_the_end_of_the_volume() {
        // 8192 blocks of the volume, 8192 per group, the first one is the block 1 (a block of 1 KB)
        assert_eq!(group_bits(8192, 1, 8192, 0), 8191, "the group's bits are those it really covers");
        assert_eq!(group_bits(8192, 1, 8192, 1), 0, "there is no second group here");
        // 20000 blocks, 8192 per group: 8192 + 8192 + the remainder
        assert_eq!(group_bits(20000, 0, 8192, 0), 8192);
        assert_eq!(group_bits(20000, 0, 8192, 1), 8192);
        assert_eq!(group_bits(20000, 0, 8192, 2), 20000 - 2 * 8192);
    }

    #[test]
    fn a_group_beyond_the_volume_has_no_bits() {
        assert_eq!(group_bits(20000, 0, 8192, 3), 0);
        assert_eq!(group_bits(20000, 0, 8192, 1000), 0, "and no underflow either");
        assert_eq!(group_bits(0, 1, 8192, 0), 0);
    }
}
