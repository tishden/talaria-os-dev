//! Thread placement policy: which core a new thread goes to, which core is
//! given out as dedicated, where a process's home NUMA node is, and when the
//! shared pool is unbalanced enough to move a task.
//!
//! Every decision here is a pure function of a `World` snapshot, so the policy
//! is tested without a machine — by the host unit tests below and by the
//! in-kernel gate `talaria.test=affinity`. `affinity.rs` takes the snapshot from
//! the live scheduler and applies the decisions.

/// Cores per machine the policy works with: the width of the affinity mask
/// (`sched_getaffinity` returns a u64). Must equal `percpu::MAXCPU`.
pub const MAXCPU: usize = 64;
/// "Not in a named pool": the shared pool.
pub const POOL_NONE: u8 = 255;
/// How much busier a process's home node may be than the best core in the
/// machine before a thread leaves it: 1 = tolerate one extra task for locality.
pub const NUMA_SLACK: usize = 1;
/// Demand of an always-runnable task, in units of the running average (a task
/// sleeping on a futex weighs about zero).
pub const W_FULL: u32 = 1024;
/// Imbalance at which the balancer moves a task, in the same units. It lies
/// STRICTLY between one and two always-runnable tasks: at a difference of one,
/// two cores would ping-pong a task every pass; exactly two cannot be required,
/// because the running average approaches its limit from below — a thread that
/// just started or just moved weighs less than `W_FULL` for its first few
/// dozen updates.
pub const IMBALANCE_W: u32 = 3 * W_FULL / 2;
const _: () = assert!(W_FULL < IMBALANCE_W && IMBALANCE_W < 2 * W_FULL);
/// Time constant of the demand filter: `w -= w >> S; w += W_FULL >> S if runnable`.
/// A quarter, not an eighth: within one decision window the filter must REACH
/// the threshold, not creep towards it.
pub const DEMAND_SHIFT: u32 = 2;

/// Snapshot of the placement the policy decides over.
#[derive(Clone, Copy)]
pub struct World {
    /// Scheduler cores (one bit per core).
    pub pool: u64,
    /// Tasks per core, by head count. PLACEMENT of a new thread is measured by
    /// this: it has no demand yet, and "like everyone else" is the only honest
    /// assumption.
    pub load: [u8; MAXCPU],
    /// Summed DEMAND of the core's tasks (`W_FULL` per always-runnable task).
    /// BALANCING is measured by this: a core with five sleeping threads and one
    /// running is less loaded than a core with two running ones.
    pub weight: [u32; MAXCPU],
    /// Of those, tasks placed by a RULE (they cannot be moved).
    pub pinned: [u8; MAXCPU],
    /// The core is dedicated (give it to nobody else).
    pub ded: [bool; MAXCPU],
    /// NUMA node of the core.
    pub node: [u8; MAXCPU],
    /// Named pool of the core (`POOL_NONE` — shared).
    pub cpool: [u8; MAXCPU],
}

impl World {
    pub const fn empty() -> Self {
        World { pool: 1, load: [0; MAXCPU], weight: [0; MAXCPU], pinned: [0; MAXCPU], ded: [false; MAXCPU], node: [0; MAXCPU], cpool: [POOL_NONE; MAXCPU] }
    }
    pub fn has(&self, c: usize) -> bool { c < MAXCPU && self.pool & (1 << c) != 0 }
    /// Free to dedicate: in the pool, not dedicated, no tasks, not the control
    /// plane (core 0 hosts the OS), and not in a NAMED pool — the operator has
    /// already said whom those cores belong to.
    fn free_for_dedicate(&self, c: usize) -> bool {
        self.has(c) && c != 0 && !self.ded[c] && self.load[c] == 0 && self.cpool[c] == POOL_NONE
    }
    /// Can be FREED for dedication: no task on it was placed by a rule, so
    /// everything there may move.
    fn evictable_for_dedicate(&self, c: usize) -> bool {
        self.has(c) && c != 0 && !self.ded[c] && self.pinned[c] == 0 && self.cpool[c] == POOL_NONE
    }
    fn shared(&self, c: usize) -> bool { self.has(c) && !self.ded[c] }
    /// The core is shared AND belongs to pool `pool` (`POOL_NONE` — the shared pool).
    pub fn in_pool(&self, c: usize, pool: u8) -> bool { self.shared(c) && self.cpool[c] == pool }
}

/// Demand of an always-runnable thread after `n` filter updates.
pub fn demand_after(n: u32) -> u32 {
    let mut w = 0u32;
    for _ in 0..n { w = w - (w >> DEMAND_SHIFT) + (W_FULL >> DEMAND_SHIFT); }
    w
}

/// Demand of a thread that fell asleep at the limit, after `n` updates.
pub fn demand_decay(n: u32) -> u32 {
    let mut w = W_FULL;
    for _ in 0..n { w -= w >> DEMAND_SHIFT; }
    w
}

/// Core of the SHARED pool for one more thread: fewest tasks, lowest number on
/// a tie (deterministic). `home` — the process's home node (255 — any): the
/// home node yields only to an imbalance larger than `NUMA_SLACK`.
pub fn pick_shared_in(w: &World, home: u8) -> usize { pick_in_pool(w, home, POOL_NONE) }

/// The same inside a NAMED pool. If the pool is empty (all its cores went
/// away), the thread goes to the shared pool: the promise "these cores are
/// yours" is already broken by the machine, and losing the thread on top of
/// that helps nobody.
pub fn pick_in_pool(w: &World, home: u8, pool: u8) -> usize {
    let mut best = usize::MAX; let mut best_load = usize::MAX;
    let mut best_home = usize::MAX; let mut best_home_load = usize::MAX;
    for c in 0..MAXCPU {
        if !w.in_pool(c, pool) { continue; }
        let l = w.load[c] as usize;
        if l < best_load { best_load = l; best = c; }
        if home != 255 && w.node[c] == home && l < best_home_load { best_home_load = l; best_home = c; }
    }
    if best == usize::MAX {
        // Empty pool: a named one falls back to the shared pool, the shared
        // one to the control plane (it always exists).
        return if pool != POOL_NONE { pick_in_pool(w, home, POOL_NONE) } else { 0 };
    }
    if best_home != usize::MAX && best_home_load <= best_load + NUMA_SLACK { return best_home; }
    best
}

/// Core to dedicate. First a truly free one; failing that, a core that holds
/// only UNpinned threads — they can be moved away, the fewer the cheaper.
///
/// A thread sets its name only AFTER `clone`, and spreading places threads
/// immediately, so by the time of `prctl` there usually is no free core left:
/// without eviction `pin=...:dedicated` would never be honoured. A rule is a
/// requirement of the workload; spreading is only a guess of the OS.
pub fn pick_dedicated_in(w: &World, home: u8) -> Option<usize> {
    if home != 255 {
        for c in 0..MAXCPU { if w.free_for_dedicate(c) && w.node[c] == home { return Some(c); } }
    }
    for c in 0..MAXCPU { if w.free_for_dedicate(c) { return Some(c); } }
    // Evictable: fewest movers, then the home node, then the lowest number.
    let mut best = usize::MAX; let mut best_load = usize::MAX; let mut best_home = false;
    for c in 0..MAXCPU {
        if !w.evictable_for_dedicate(c) { continue; }
        let l = w.load[c] as usize;
        let h = home != 255 && w.node[c] == home;
        if l < best_load || (l == best_load && h && !best_home) { best_load = l; best = c; best_home = h; }
    }
    if best == usize::MAX { None } else { Some(best) }
}

/// Does the machine have node `nd` (a scheduler core on it)?
pub fn node_exists(w: &World, nd: u8) -> bool {
    (0..MAXCPU).any(|c| w.has(c) && w.node[c] == nd)
}

/// Home node for a process that has none yet: the node with most free cores
/// (lowest number on a tie). 255 — no scheduler core at all.
pub fn pick_node_in(w: &World) -> u8 {
    let mut best = 255u8; let mut best_free = -1i32;
    for nd in 0..64u8 {
        let mut free = 0i32; let mut present = false;
        for c in 0..MAXCPU {
            if !w.has(c) || w.node[c] != nd { continue; }
            present = true;
            if !w.ded[c] && w.load[c] == 0 { free += 1; }
        }
        if present && free > best_free { best_free = free; best = nd; }
    }
    best
}

/// Whom to move where to balance the shared pool: (donor core, receiver core).
/// `None` — the imbalance is below the threshold, nothing to move (the normal
/// state, not a failure).
///
/// Measured by DEMAND (`weight`), not by head count, and the donor must hold at
/// least two tasks: taking the only one is not balancing but moving, and it
/// creates the opposite imbalance by itself.
pub fn imbalance_in(w: &World) -> Option<(usize, usize)> { imbalance_in_pool(w, POOL_NONE) }

/// The same within one pool: balancing happens INSIDE a pool only — an
/// imbalance between pools is not an imbalance but the operator's decision.
pub fn imbalance_in_pool(w: &World, pool: u8) -> Option<(usize, usize)> {
    let mut mx = usize::MAX; let mut mx_w = 0u32;
    let mut mn = usize::MAX; let mut mn_w = u32::MAX;
    for c in 0..MAXCPU {
        if !w.in_pool(c, pool) { continue; }
        let x = w.weight[c];
        if w.load[c] >= 2 && (mx == usize::MAX || x > mx_w) { mx_w = x; mx = c; }
        if x < mn_w { mn_w = x; mn = c; }
    }
    if mx == usize::MAX || mn == usize::MAX || mx == mn { return None; }
    if mx_w < mn_w.saturating_add(IMBALANCE_W) { return None; }
    Some((mx, mn))
}

// ── work stealing: may this steal be made ────────────────────────────────────
//
// A steal is the largest decision the scheduler makes silently, and its mistake
// does not end: one wrong steal seats two active threads on one core FOREVER —
// the balancer does not reach the skew's threshold, while the thief no longer
// looks for work, it "has" some. It was caught as a bistable collapse of message
// passing (25 us per message instead of 1.5), and the rule that came out of it is
// the one below: after a steal the thief must have NO MORE than what is left at
// the donor.
//
// WHAT to count that with is a separate question, and two of the three answers
// are wrong in a way no counter shows: by the ASSIGNED tasks a thread pool's core
// with five sleeping threads looks busier than a core with two computing ones
// (235 refusals against 2 steals, a query on three cores out of four); by the
// INSTANTANEOUS readiness a pair passing messages flickers. The knob
// `talaria.stealbal=` exists to measure the variants against each other rather
// than to argue about them; the analysis of each is at `STEALBAL_RULE` in
// `affinity.rs`.

/// What a steal measures the balance by (`talaria.stealbal=`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StealBy {
    /// The ASSIGNED tasks of the core — the default, and the count the rule was
    /// born with.
    Assigned,
    /// The READY ones right now.
    Ready,
    /// The DEMAND (the running average of the readiness), with the balancer's own
    /// skew threshold.
    Demand,
}

/// The knob's number -> what to count by. The thresholds are exactly the kernel's
/// (`>= 4` the demand, `>= 2` the ready ones, otherwise the assigned ones), so a
/// variant cannot mean one thing in the policy and another in the measurement.
pub fn steal_by(rule: u8) -> StealBy {
    if rule >= 4 { StealBy::Demand } else if rule >= 2 { StealBy::Ready } else { StealBy::Assigned }
}

/// Would carrying ONE task from `donor` onto `thief` improve the balance?
/// `false` — the steal backs off.
///
/// By the head count the condition is "the thief plus two is not more than the
/// donor": after the move the thief has `thief+1` and the donor `donor-1`, so a
/// difference of two is the smallest one a move does not merely mirror.
/// By the demand the threshold is `IMBALANCE_W` rather than two full tasks —
/// the running average approaches `W_FULL` FROM BELOW, so with two always-ready
/// threads the donor's sum is always slightly less than `2 * W_FULL` and a
/// condition written with it is never satisfied (measured: 1-3 steals per run
/// instead of forty).
pub fn steal_improves(by: StealBy, thief: u32, donor: u32) -> bool {
    let need = match by { StealBy::Demand => IMBALANCE_W, _ => 2 };
    // the DIFFERENCE, not a sum against a bound: a sum of demands saturates at the
    // top, and a saturated sum compares as "equal" — that is, as a permission
    donor.saturating_sub(thief) >= need
}

/// Is the core empty BY DEMAND? That is what tells a thread sleeping microseconds
/// between messages (it keeps its demand) from a pool's thread sleeping for
/// seconds (it does not) — the instantaneous "there is nobody ready" says the
/// same thing about both.
pub fn demand_idle(w: u32) -> bool { w < W_FULL / 4 }

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` cores, all shared, node 0, empty.
    fn world(n: usize) -> World {
        let mut w = World::empty();
        w.pool = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
        w
    }

    #[test]
    fn new_thread_goes_to_least_loaded_core_lowest_number_on_tie() {
        let mut w = world(4);
        w.load = [2, 1, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(pick_shared_in(&w, 255), 1);
    }

    #[test]
    fn eight_threads_on_four_cores_spread_two_each() {
        let mut w = world(4);
        for _ in 0..8 { let c = pick_shared_in(&w, 255); w.load[c] += 1; }
        assert_eq!(&w.load[..4], &[2, 2, 2, 2]);
    }

    #[test]
    fn dedicated_cores_are_skipped_by_shared_placement() {
        let mut w = world(4);
        w.ded[1] = true;
        w.load[0] = 1; w.load[2] = 1;
        assert_eq!(pick_shared_in(&w, 255), 3);
    }

    #[test]
    fn home_node_wins_within_numa_slack_and_loses_beyond_it() {
        let mut w = world(4);
        w.node[2] = 1; w.node[3] = 1;
        w.load[0] = 1; w.load[1] = 1; w.load[2] = 0; w.load[3] = 0;
        assert_eq!(pick_shared_in(&w, 0), 0, "home is one task busier: stay");
        w.load[0] = 2; w.load[1] = 2;
        assert_eq!(pick_shared_in(&w, 0), 2, "home is two tasks busier: leave");
    }

    #[test]
    fn named_pool_threads_stay_in_the_pool_and_others_avoid_it() {
        let mut w = world(6);
        w.cpool[4] = 0; w.cpool[5] = 0;
        w.load[4] = 3; w.load[5] = 2;
        assert_eq!(pick_in_pool(&w, 255, 0), 5);
        assert_eq!(pick_in_pool(&w, 255, POOL_NONE), 0);
    }

    #[test]
    fn empty_named_pool_falls_back_to_shared_and_empty_shared_to_core_0() {
        let w = world(3);
        assert_eq!(pick_in_pool(&w, 255, 3), 0);
        let mut all_ded = world(2);
        all_ded.ded = [true; MAXCPU];
        assert_eq!(pick_shared_in(&all_ded, 255), 0);
    }

    #[test]
    fn dedicate_prefers_free_core_on_home_node_never_core_0() {
        let mut w = world(4);
        w.node[1] = 0; w.node[2] = 1; w.node[3] = 1;
        assert_eq!(pick_dedicated_in(&w, 1), Some(2));
        assert_eq!(pick_dedicated_in(&w, 255), Some(1));
        let one = world(1);
        assert_eq!(pick_dedicated_in(&one, 255), None, "core 0 is never dedicated");
    }

    #[test]
    fn dedicate_evicts_unpinned_threads_but_not_pinned_or_pool_cores() {
        let mut w = world(4);
        w.load = [0; MAXCPU];
        w.load[1] = 2; w.load[2] = 1; w.load[3] = 1;
        w.pinned[2] = 1;          // a rule put a thread here
        w.cpool[3] = 0;           // operator's named pool
        assert_eq!(pick_dedicated_in(&w, 255), Some(1));
        w.pinned[1] = 1;
        assert_eq!(pick_dedicated_in(&w, 255), None);
    }

    #[test]
    fn home_node_is_the_one_with_most_free_cores() {
        let mut w = world(6);
        for c in 3..6 { w.node[c] = 1; }
        w.load[0] = 1; w.load[1] = 1;
        assert_eq!(pick_node_in(&w), 1);
        assert!(node_exists(&w, 1) && !node_exists(&w, 2));
        let mut none = World::empty(); none.pool = 0;
        assert_eq!(pick_node_in(&none), 255);
    }

    #[test]
    fn balance_needs_more_than_one_task_of_difference() {
        let mut w = world(2);
        w.load[0] = 2; w.weight[0] = 2 * W_FULL;
        w.load[1] = 1; w.weight[1] = W_FULL;
        assert_eq!(imbalance_in(&w), None, "a difference of one task would ping-pong");
        w.load[0] = 3; w.weight[0] = 3 * W_FULL; w.load[1] = 0; w.weight[1] = 0;
        assert_eq!(imbalance_in(&w), Some((0, 1)));
    }

    #[test]
    fn balance_uses_demand_not_head_count() {
        let mut w = world(2);
        w.load[0] = 6; w.weight[0] = W_FULL;         // five sleepers, one runner
        w.load[1] = 2; w.weight[1] = 2 * W_FULL;     // two runners
        assert_eq!(imbalance_in(&w), None);
    }

    #[test]
    fn donor_with_a_single_task_is_not_a_donor() {
        let mut w = world(2);
        w.load[0] = 1; w.weight[0] = 3 * W_FULL; // impossible weight, still one task
        assert_eq!(imbalance_in(&w), None);
    }

    #[test]
    fn balancing_stays_inside_a_named_pool() {
        let mut w = world(6);
        w.cpool[4] = 0; w.cpool[5] = 0;
        w.load[4] = 4; w.weight[4] = 4 * W_FULL;
        assert_eq!(imbalance_in_pool(&w, 0), Some((4, 5)));
        assert_eq!(imbalance_in(&w), None, "the shared pool is balanced");
    }

    #[test]
    fn demand_filter_reaches_the_threshold_within_one_decision_window() {
        // 32 updates per balancer pass (DEMAND_EVERY_NS vs BALANCE_EVERY_NS)
        let two = 2 * demand_after(32);
        assert!(two > IMBALANCE_W, "two runners {two} must exceed the threshold {IMBALANCE_W}");
        assert!(demand_after(8) < W_FULL, "a new thread weighs less than a settled one");
        assert_eq!(demand_after(1000), W_FULL, "integer filter settles exactly at the limit");
        assert!(demand_decay(32) < W_FULL / 100, "a sleeper stops counting within a window");
    }

    // ── work stealing ────────────────────────────────────────────────────────

    #[test]
    fn the_knobs_number_picks_the_count_the_kernel_uses() {
        assert_eq!(steal_by(0), StealBy::Assigned);
        assert_eq!(steal_by(1), StealBy::Assigned, "the default is the count the rule was born with");
        assert_eq!(steal_by(2), StealBy::Ready);
        assert_eq!(steal_by(3), StealBy::Ready);
        assert_eq!(steal_by(4), StealBy::Demand);
        assert_eq!(steal_by(5), StealBy::Demand);
    }

    /// The collapse of session 20 in one line: a core with two ready threads and a
    /// core with one must NOT exchange a thread — the skew would simply change
    /// sides, and nobody spreads it apart after that.
    #[test]
    fn a_steal_that_only_mirrors_the_skew_is_refused() {
        for by in [StealBy::Assigned, StealBy::Ready] {
            assert!(!steal_improves(by, 1, 2), "{:?}: 2/1 must not become 1/2", by);
            assert!(!steal_improves(by, 2, 2), "{:?}: an equal load is not a skew", by);
            assert!(!steal_improves(by, 2, 3), "{:?}: 3/2 becomes 2/3 — the same skew", by);
            assert!(!steal_improves(by, 0, 1), "{:?}: the donor's only task is not extra", by);
            assert!(steal_improves(by, 0, 2), "{:?}: 2/0 -> 1/1 is exactly what a steal is for", by);
            assert!(steal_improves(by, 1, 3), "{:?}: 3/1 -> 2/2", by);
        }
    }

    /// The rule must limit itself: a pool of eight threads landed on one core
    /// spreads out and STOPS. A rule that does not stop is the same collapse from
    /// the other side — the threads roam and lose their caches.
    #[test]
    fn a_pool_on_one_core_spreads_to_halves_and_stops() {
        for by in [StealBy::Assigned, StealBy::Ready] {
            let mut core = [8u32, 0];
            let mut steps = 0;
            loop {
                // an empty core comes to steal; it is the only thief, as in the kernel
                // (a thief enters a steal only with no ready tasks of its own)
                let (thief, donor) = if core[0] < core[1] { (0usize, 1usize) } else { (1, 0) };
                if !steal_improves(by, core[thief], core[donor]) { break; }
                core[thief] += 1; core[donor] -= 1;
                steps += 1;
                assert!(steps < 100, "{:?}: the rule does not converge — the threads roam", by);
            }
            assert_eq!(core, [4, 4], "{:?}: it stopped at {:?}", by, core);
        }
    }

    /// The trap the demand threshold was measured into: the filter converges to
    /// `W_FULL` from below, so two always-ready threads NEVER sum up to
    /// `2 * W_FULL`, and a condition written with that number refuses every steal.
    #[test]
    fn the_demand_threshold_lets_two_ready_threads_through() {
        // the filter reaches the limit only in the limit: a thread that started or
        // moved a couple of dozen decisions ago weighs less, and it is that case the
        // threshold is written for
        let fresh = demand_after(8);
        assert!(fresh < W_FULL, "the running average approaches its limit from below");
        let donor = 2 * fresh;                        // two such threads on one core
        assert!(donor < 2 * W_FULL, "that is exactly why two full tasks cannot be required");
        assert!(steal_improves(StealBy::Demand, 0, donor), "an empty core must be allowed to take one of the two");
        assert!(donor < 2 * W_FULL, "the condition written with two full tasks refuses this very steal");
        // one ready thread is not given away, however long it has been ready
        assert!(!steal_improves(StealBy::Demand, 0, fresh));
        assert!(!steal_improves(StealBy::Demand, 0, demand_after(1000)));
    }

    #[test]
    fn the_demand_rule_converges_too() {
        let one = W_FULL;
        let mut core = [4 * one, 0];
        let mut steps = 0;
        while steal_improves(StealBy::Demand, core[1], core[0]) {
            core[0] -= one; core[1] += one;
            steps += 1;
            assert!(steps < 100, "the demand rule does not converge");
        }
        assert_eq!(core[0] / one, 2, "it stopped at a half");
        assert_eq!(core[1] / one, 2);
    }

    #[test]
    fn the_demand_threshold_never_overflows() {
        assert!(!steal_improves(StealBy::Demand, u32::MAX, 0), "a saturating sum must not wrap into a permission");
        assert!(!steal_improves(StealBy::Demand, u32::MAX, u32::MAX));
    }

    /// "The core is empty" by the demand is the only way to tell a thread that
    /// sleeps microseconds between messages from a pool's thread that sleeps for
    /// seconds: the instantaneous "nobody is ready" is true of both.
    #[test]
    fn an_idle_core_by_demand_is_not_a_core_between_two_messages() {
        let busy = demand_after(1000);
        assert!(!demand_idle(busy), "an always-ready thread does not look idle");
        // a pause of a couple of decisions — the demand is still there
        assert!(!demand_idle(demand_decay(2)), "a thread between two messages keeps its demand");
        // a long sleep — the demand has decayed
        assert!(demand_idle(demand_decay(20)), "a thread that has slept a long time does not hold the core");
        assert!(demand_idle(0));
    }
}
