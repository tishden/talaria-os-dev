//! Whom this core takes next: the circular search over the threads assigned to it
//! and the policy "a request in progress keeps the core".
//!
//! Why a separate module. This is the decision the scheduler makes most often —
//! at every tick and at every yield, hundreds of thousands of times a second per
//! core — and it has no observable outcome of its own: a mistake shows up as a
//! latency somebody else measures. Two of its properties are worth more than
//! their size:
//!
//!   - the search is CIRCULAR from `cur + 1` and it is allowed to come back to
//!     `cur`: that is the whole of the rotation's fairness. Start it at 0 and the
//!     low-numbered threads get the core while the high-numbered ones wait for
//!     everybody; stop it one short of `cur` and a core with a single ready
//!     thread finds nobody and idles with work in hand.
//!   - the policy `talaria.reqfirst=`: a thread with a request in progress is not
//!     taken off the core in favour of a thread that is merely WAITING FOR THE
//!     NETWORK. It cost three quarters of the stops of a request mid-flight
//!     (session 43), and its danger is the mirror one — a thread that keeps the
//!     core forever. The ceiling on the protection lives in `sched.rs` (it needs a
//!     clock); here is the shape of the decision.

/// What the choice needs to know about the threads. The kernel answers from the
/// scheduler's tables, a test from an array.
pub trait Threads {
    /// How many threads there may be (the table's occupied prefix).
    fn n(&self) -> usize;
    /// The thread may be given a core right now (it is alive, it is not running
    /// elsewhere, it does not sleep).
    fn ready(&self, t: usize) -> bool;
    /// The core the thread is assigned to.
    fn cpu(&self, t: usize) -> usize;
    /// The thread is waiting for the network — the candidate the policy
    /// `talaria.reqfirst=` steps over.
    fn net_waiter(&self, t: usize) -> bool;
}

/// The circular search for this core's next ready thread, starting at `cur + 1`.
/// `skip_waiter` — pass over the network waiters (the policy `reqfirst`).
///
/// It may return `cur` itself: a core with one ready thread must keep running it
/// rather than idle.
pub fn scan(th: &impl Threads, cpu: usize, cur: usize, skip_waiter: bool) -> Option<usize> {
    let n = th.n();
    if n == 0 { return None; }
    let mut next = (cur + 1) % n;
    let mut tries = 0;
    while tries < n {
        if th.ready(next) && th.cpu(next) == cpu && !(skip_waiter && th.net_waiter(next)) { return Some(next); }
        next = (next + 1) % n;
        tries += 1;
    }
    None
}

/// The decision for a core that is taking the thread `cur` off.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pick {
    /// Run this thread.
    Take(usize),
    /// There IS a candidate, but it is a network waiter while `cur` has a request
    /// in flight — the core stays with `cur`. That is the policy's only action.
    Stay,
    /// This core has no ready thread of its own (the caller goes on to a steal).
    Nothing,
}

#[cfg(test)]
impl Pick {
    /// Does the decision leave the core with `cur`? To the caller `Take(cur)` and
    /// `Stay` are the same act — it returns to the same context; they differ in the
    /// diagnostic mark, and `Stay` is the policy's own decision, so it is counted.
    pub fn stays_with(self, cur: usize) -> bool { matches!(self, Pick::Stay) || self == Pick::Take(cur) }
}

/// `guarded` — the thread `cur` has a request in progress and the protection's
/// ceiling has not expired (`sched::req_guarded`: it needs a clock, so it is
/// decided there).
pub fn pick(th: &impl Threads, cpu: usize, cur: usize, guarded: bool) -> Pick {
    if !guarded {
        return match scan(th, cpu, cur, false) { Some(t) => Pick::Take(t), None => Pick::Nothing };
    }
    if let Some(t) = scan(th, cpu, cur, true) { return Pick::Take(t); }
    // There is a candidate, but it is a network waiter — the case the policy was
    // written for.
    if scan(th, cpu, cur, false).is_some() { return Pick::Stay; }
    Pick::Nothing
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A core's threads as a table: (ready, the core, it waits for the network).
    struct T(Vec<(bool, usize, bool)>);

    impl Threads for T {
        fn n(&self) -> usize { self.0.len() }
        fn ready(&self, t: usize) -> bool { self.0[t].0 }
        fn cpu(&self, t: usize) -> usize { self.0[t].1 }
        fn net_waiter(&self, t: usize) -> bool { self.0[t].2 }
    }

    /// `n` ready threads, all on core 0, none waiting for the network.
    fn ready(n: usize) -> T { T((0..n).map(|_| (true, 0usize, false)).collect()) }

    #[test]
    fn the_search_starts_after_the_current_thread() {
        let th = ready(4);
        assert_eq!(scan(&th, 0, 0, false), Some(1));
        assert_eq!(scan(&th, 0, 1, false), Some(2));
        assert_eq!(scan(&th, 0, 3, false), Some(0), "past the table's end it wraps to zero");
    }

    /// The rotation must give every thread the core in turn: with `k` ready threads
    /// the sequence of decisions is a cycle over all of them. A search that started
    /// at zero would give the core to the first one for ever.
    #[test]
    fn the_rotation_gives_every_thread_its_turn() {
        for k in 1..8usize {
            let th = ready(k);
            let mut cur = 0usize;
            let mut seen = vec![false; k];
            for _ in 0..k {
                cur = scan(&th, 0, cur, false).expect("a ready thread exists");
                assert!(!seen[cur], "the thread {} came round twice before the others of {}", cur, k);
                seen[cur] = true;
            }
            assert!(seen.iter().all(|&x| x), "with {} threads the rotation missed one", k);
            assert_eq!(cur, 0, "after a full circle it is back at the first");
        }
    }

    #[test]
    fn a_core_with_one_ready_thread_keeps_running_it() {
        let th = ready(1);
        assert_eq!(scan(&th, 0, 0, false), Some(0), "the search may come back to the current thread");
        // the same in a big table where everything else is not ours
        let mut big = ready(6);
        for t in 1..6 { big.0[t] = (true, 1, false); }
        assert_eq!(scan(&big, 0, 0, false), Some(0));
    }

    #[test]
    fn the_threads_of_another_core_are_not_taken() {
        let mut th = ready(5);
        th.0[1] = (true, 1, false);
        th.0[2] = (true, 2, false);
        assert_eq!(scan(&th, 0, 0, false), Some(3), "1 and 2 live on other cores");
        assert_eq!(scan(&th, 1, 0, false), Some(1));
        assert_eq!(scan(&th, 3, 0, false), None, "core 3 has nobody of its own");
    }

    #[test]
    fn a_thread_that_is_not_ready_is_stepped_over() {
        let mut th = ready(4);
        th.0[1].0 = false;
        th.0[2].0 = false;
        assert_eq!(scan(&th, 0, 0, false), Some(3));
        th.0[3].0 = false;
        assert_eq!(scan(&th, 0, 0, false), Some(0), "only the current one is left");
        th.0[0].0 = false;
        assert_eq!(scan(&th, 0, 0, false), None);
    }

    #[test]
    fn an_empty_table_decides_nothing() {
        let th = T(Vec::new());
        assert_eq!(scan(&th, 0, 0, false), None);
        assert_eq!(pick(&th, 0, 0, false), Pick::Nothing);
        assert_eq!(pick(&th, 0, 0, true), Pick::Nothing);
    }

    #[test]
    fn without_the_policy_a_network_waiter_is_an_ordinary_candidate() {
        let mut th = ready(3);
        th.0[1].2 = true;
        assert_eq!(scan(&th, 0, 0, false), Some(1));
        assert_eq!(pick(&th, 0, 0, false), Pick::Take(1), "the policy is off — the rotation is as usual");
    }

    #[test]
    fn a_request_in_flight_is_not_swapped_for_a_network_waiter() {
        let mut th = ready(2);
        th.0[1].2 = true;                             // the only candidate waits for the network
        assert_eq!(pick(&th, 0, 0, false), Pick::Take(1), "without the policy the waiter gets the core");
        let p = pick(&th, 0, 0, true);
        assert!(p.stays_with(0), "the core stays with the request, not {:?}", p);
        // ...but a candidate with work of its own is taken even so
        let mut th2 = ready(3);
        th2.0[1].2 = true;
        assert_eq!(pick(&th2, 0, 0, true), Pick::Take(2), "the waiter is stepped over, the next one is taken");
    }

    /// `Stay` proper — the decision the policy counts — appears when `cur` itself is
    /// not a candidate of the guarded search (it is a network waiter too, or it has
    /// stopped being ready) while a candidate does exist. The core is kept even so:
    /// giving it away is exactly what the request is protected from.
    #[test]
    fn the_policy_keeps_the_core_even_when_the_current_thread_is_not_a_candidate() {
        let mut th = ready(2);
        th.0[0].2 = true;                             // cur looks like a waiter as well
        th.0[1].2 = true;
        assert_eq!(pick(&th, 0, 0, true), Pick::Stay);
        assert_eq!(pick(&th, 0, 0, false), Pick::Take(1));
        let mut gone = ready(2);
        gone.0[0].0 = false;                          // cur is no longer ready
        gone.0[1].2 = true;
        assert_eq!(pick(&gone, 0, 0, true), Pick::Stay);
    }

    /// The invariant the policy exists for: while the request is guarded, the core
    /// never goes to a network waiter. Over every table of three threads.
    #[test]
    fn a_guarded_request_never_hands_the_core_to_a_waiter() {
        for bits in 0..64u32 {
            let mut th = ready(3);
            for t in 0..3 {
                th.0[t].0 = bits & (1 << t) != 0;
                th.0[t].2 = bits & (1 << (t + 3)) != 0;
            }
            for cur in 0..3usize {
                match pick(&th, 0, cur, true) {
                    Pick::Take(t) => assert!(!th.0[t].2, "bits {:#x}: the core went to the waiter {}", bits, t),
                    Pick::Stay => assert!(scan(&th, 0, cur, false).is_some(), "bits {:#x}: it stays with nobody to stay against", bits),
                    Pick::Nothing => assert!(scan(&th, 0, cur, false).is_none(), "bits {:#x}: there was a candidate", bits),
                }
                // and without the protection the decision is the plain rotation
                assert_eq!(pick(&th, 0, cur, false), match scan(&th, 0, cur, false) { Some(t) => Pick::Take(t), None => Pick::Nothing });
            }
        }
    }

    #[test]
    fn the_policy_does_not_invent_a_candidate_out_of_nothing() {
        let mut th = ready(3);
        th.0[1] = (false, 0, true);
        th.0[2] = (false, 0, true);
        assert_eq!(pick(&th, 0, 0, true), Pick::Take(0), "the current thread is the only ready one — it goes on");
        let mut none = ready(3);
        for t in 0..3 { none.0[t].0 = false; }
        assert_eq!(pick(&none, 0, 0, true), Pick::Nothing, "nobody is ready — the caller goes to a steal");
    }

    /// The protection must not turn into a hold: `Stay` is possible only while the
    /// caller says the request is guarded. The ceiling itself lives in `sched.rs`,
    /// and this is the property that makes its expiry enough.
    #[test]
    fn the_core_is_given_up_as_soon_as_the_protection_ends() {
        let mut th = ready(2);
        th.0[1].2 = true;
        assert!(pick(&th, 0, 0, true).stays_with(0));
        assert_eq!(pick(&th, 0, 0, false), Pick::Take(1), "the ceiling has expired — the waiter gets the core");
    }

    #[test]
    fn a_core_of_only_network_waiters_rotates_between_them() {
        let mut th = ready(3);
        for t in 0..3 { th.0[t].2 = true; }
        // no request in flight: the waiters replace one another as usual
        assert_eq!(pick(&th, 0, 0, false), Pick::Take(1));
        assert_eq!(pick(&th, 0, 1, false), Pick::Take(2));
        // with a request in flight the core stays with it
        assert!(pick(&th, 0, 0, true).stays_with(0));
    }
}
