//! Which pieces of the RAM above 4 GB become the main node's high areas (`mem::high_init`)
//! — the pure part, so that a machine QEMU cannot shape (holes between banks above 4 GB,
//! another node's area in the middle of the main node's RAM) is judged on the host
//! (`kernel-unit`).

/// How many separate high areas the main node keeps.
pub const MAX_HIGH: usize = 4;

/// The `MAX_HIGH` largest runs of RAM in [`floor`, `ceil`) that lie outside every `taken`
/// area (the other nodes') and whose first and last frames `own` accepts (the main node's
/// by the SRAT), the largest first; an empty slot is `(0, 0)`. `ram` is (base, length);
/// `taken` is (base, end) with `end <= base` meaning an empty slot. The bounds are rounded
/// inwards to `frame`.
pub fn pick_high(ram: &[(u64, u64)], taken: &[(u64, u64)], floor: u64, ceil: u64, frame: u64,
                 own: impl Fn(u64) -> bool) -> [(u64, u64); MAX_HIGH] {
    let mut runs = [(0u64, 0u64); MAX_HIGH];
    for &(rb, rl) in ram {
        // A region may straddle the floor (QEMU's `virt` puts RAM at 1 GB, so 4 GB of it
        // ends at 5): the part below belongs to the main allocator, the part above is ours.
        let mut b = (rb.max(floor) + frame - 1) & !(frame - 1);
        let e = (rb + rl).min(ceil) & !(frame - 1);
        // The runs between the other nodes' areas.
        while b < e {
            let mut run_end = e;
            let mut inside = 0u64;
            for &(nb, ne) in taken {
                if ne <= nb { continue; }
                if b >= nb && b < ne { inside = ne; break; }
                if nb > b && nb < run_end { run_end = nb; }
            }
            if inside != 0 { b = (inside + frame - 1) & !(frame - 1); continue; }
            if own(b) && own(run_end - 1) { keep(&mut runs, b, run_end); }
            b = run_end;
        }
    }
    runs
}

/// Insert (b, e) into the size-sorted `runs`, dropping the smallest when full.
fn keep(runs: &mut [(u64, u64); MAX_HIGH], b: u64, e: u64) {
    let mut i = MAX_HIGH;
    while i > 0 && e - b > runs[i - 1].1 - runs[i - 1].0 { i -= 1; }
    if i < MAX_HIGH {
        for j in (i + 1..MAX_HIGH).rev() { runs[j] = runs[j - 1]; }
        runs[i] = (b, e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const G: u64 = 1 << 30;
    const F: u64 = 4096;

    #[test]
    fn one_piece_straddling_the_floor() {
        // `virt` with 4 GB: RAM 1..5 GB -> the high area is 4..5 GB.
        let r = pick_high(&[(G, 4 * G)], &[], 4 * G, 192 * G, F, |_| true);
        assert_eq!(r[0], (4 * G, 5 * G));
        assert_eq!(r[1], (0, 0));
    }

    #[test]
    fn a_c7g_large() {
        // RAM at 1-2 GiB and from 16 GiB to 18.9 GiB.
        let top = 16 * G + 2976 * (1 << 20);
        let r = pick_high(&[(G, G), (16 * G, top - 16 * G)], &[], 4 * G, 192 * G, F, |_| true);
        assert_eq!(r[0], (16 * G, top));
        assert_eq!(r[1], (0, 0));
    }

    #[test]
    fn every_bank_above_4g_is_kept_largest_first() {
        // Three banks above 4 GB with holes between them: all three, sorted by size.
        let ram = [(0, 3 * G), (4 * G, G), (8 * G, 3 * G), (16 * G, 2 * G)];
        let r = pick_high(&ram, &[], 4 * G, 192 * G, F, |_| true);
        assert_eq!(r, [(8 * G, 11 * G), (16 * G, 18 * G), (4 * G, 5 * G), (0, 0)]);
    }

    #[test]
    fn another_nodes_area_splits_a_bank() {
        // One bank 4..12 GB with node 1's area at 6..8 GB: the two sides are both ours.
        let r = pick_high(&[(4 * G, 8 * G)], &[(6 * G, 8 * G), (0, 0)], 4 * G, 192 * G, F, |_| true);
        assert_eq!(r, [(8 * G, 12 * G), (4 * G, 6 * G), (0, 0), (0, 0)]);
    }

    #[test]
    fn a_foreign_run_and_the_ceiling() {
        // The SRAT gives 10..12 GB to another node; the ceiling cuts the last bank.
        let own = |a: u64| !(10 * G..12 * G).contains(&a);
        let r = pick_high(&[(4 * G, 2 * G), (10 * G, 2 * G), (190 * G, 4 * G)], &[], 4 * G, 192 * G, F, own);
        assert_eq!(r, [(4 * G, 6 * G), (190 * G, 192 * G), (0, 0), (0, 0)]);
    }

    #[test]
    fn more_banks_than_slots_keeps_the_largest() {
        let ram: [(u64, u64); 6] = core::array::from_fn(|i| ((8 + 4 * i as u64) * G, (i as u64 + 1) * (G / 4)));
        let r = pick_high(&ram, &[], 4 * G, 192 * G, F, |_| true);
        let sizes: [u64; MAX_HIGH] = core::array::from_fn(|i| (r[i].1 - r[i].0) / (G / 4));
        assert_eq!(sizes, [6, 5, 4, 3]);
    }

    #[test]
    fn unaligned_bounds_round_inwards() {
        let r = pick_high(&[(4 * G + 100, G)], &[], 4 * G, 192 * G, F, |_| true);
        assert_eq!(r[0], (4 * G + F, 5 * G + 100 - 100 % F));
        assert_eq!(r[0].1 % F, 0);
    }
}
