//! The predicates of a wait on a word: "must the sleeper go on sleeping" and "may
//! the sleeper be declared woken".
//!
//! Why a separate module. A missed wakeup is the worst defect a synchronisation
//! primitive has: the thread does not crash, it stops for good, and the reason is
//! not in its own code. In our own case the price of the question is written down
//! in `sync.rs`: a waiter that lost its wakeup once took the whole core out of
//! rotation, and from the outside it looked like a hung workload.
//!
//! The predicate is asked from two sides, and they must agree. The SLEEPER asks
//! "do I keep waiting" (and re-asks it on every re-arm of the monitor, without
//! going back to user); the WAKER — a preemption, a signal, a partner's progress
//! — asks "has it been satisfied already" and, if so, marks the thread runnable.
//! Where the two disagree, a wakeup falls through the crack: the sleeper decides
//! to sleep on while the waker has already decided nobody needs telling.
//!
//! The arithmetic to get wrong here is the INDEX comparison (the upark
//! hysteresis and the doorbell): "has the producer's index grown up to my
//! target" holds for a ring counter that goes round 2^32, so it is a comparison
//! of a DIFFERENCE as a signed number, not of two numbers. Written as `cur >=
//! target` it works for months and then loses a wakeup at the wrap — which is
//! why both sides of it live here, with tests across the wrap.

/// What the sleeper is waiting for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `FUTEX_WAIT`: while the word still holds the value it went to sleep on.
    Word,
    /// The upark hysteresis: while the INDEX has not grown up to the target. The
    /// kernel re-arms the monitor itself instead of returning to user on every
    /// step of a neighbour.
    Index,
    /// A doorbell: while the door still holds its value AND the index has not
    /// grown up to the target. The partner rings the door ONCE at the threshold
    /// (the door lies on its own cache line), so the index is rechecked by the
    /// kernel — otherwise a partner that has already passed the threshold without
    /// ringing would leave the sleeper standing.
    Door,
}

/// The kind as the syscall passes it (`upark`'s `kind` argument).
pub fn kind_of(k: u64) -> Kind {
    match k {
        0 => Kind::Word,
        1 => Kind::Index,
        _ => Kind::Door,
    }
}

/// Has the index `cur` grown up to `target`? A comparison of the DIFFERENCE as a
/// signed number: the index is a ring counter, and at its wrap through 2^32 a
/// direct `cur >= target` answers "no" for half the ring.
#[inline]
pub fn reached(cur: u32, target: u32) -> bool { (cur.wrapping_sub(target) as i32) >= 0 }

/// The SLEEPER's side: must the wait go on? `word` is what lies in the word
/// waited on, `val` is what it went to sleep on. `idx` is the pair `(the
/// partner's index, my target)` for a doorbell — `None` when the sleeper passed
/// no index (then a doorbell waits for the door alone).
pub fn still_waiting(kind: Kind, word: u32, val: u32, idx: Option<(u32, u32)>) -> bool {
    match kind {
        Kind::Word => word == val,
        Kind::Index => !reached(word, val),
        Kind::Door => {
            if word != val { return false; }            // the door was rung
            match idx {
                None => true,
                Some((cur, target)) => !reached(cur, target),
            }
        }
    }
}

/// The WAKER's side: may the sleeper be declared woken already? It looks at the
/// word alone — the index of a doorbell it does not read (the door is what the
/// partner rings), so for a doorbell this answer is DELIBERATELY the eager one: a
/// wakeup too many costs a turn of the loop, a wakeup too few costs the thread.
pub fn satisfied(kind: Kind, word: u32, val: u32) -> bool {
    match kind {
        Kind::Index => reached(word, val),
        _ => word != val,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kind_comes_from_the_syscalls_argument() {
        assert_eq!(kind_of(0), Kind::Word);
        assert_eq!(kind_of(1), Kind::Index);
        assert_eq!(kind_of(2), Kind::Door);
        assert_eq!(kind_of(99), Kind::Door, "an unknown kind is a doorbell, as the kernel reads it");
    }

    #[test]
    fn a_word_wait_ends_at_any_change_of_the_word() {
        assert!(still_waiting(Kind::Word, 5, 5, None));
        assert!(!still_waiting(Kind::Word, 6, 5, None));
        assert!(!still_waiting(Kind::Word, 4, 5, None), "a word that went DOWN is a change too");
        assert!(!still_waiting(Kind::Word, 0, 5, None));
    }

    #[test]
    fn an_index_wait_ends_when_the_index_reaches_the_target() {
        assert!(still_waiting(Kind::Index, 7, 8, None));
        assert!(!still_waiting(Kind::Index, 8, 8, None), "the target has been reached exactly");
        assert!(!still_waiting(Kind::Index, 9, 8, None), "the partner ran past the target");
        assert!(reached(8, 8) && reached(9, 8) && !reached(7, 8));
    }

    /// The comparison that decides it all. A ring index goes round 2^32, and the
    /// target of a sleeper that fell asleep just before the wrap lies just after
    /// it: written as `cur >= target` the predicate keeps the sleeper standing for
    /// two billion steps of the partner.
    #[test]
    fn the_index_comparison_survives_the_wrap_of_the_counter() {
        let target = 5u32;                               // the partner was at 0xFFFF_FFFB and wrapped
        for back in 1..=8u32 {
            let cur = 0u32.wrapping_sub(back);           // 0xFFFF_FFFF .. 0xFFFF_FFF8
            assert!(still_waiting(Kind::Index, cur, target, None),
                    "the index {:#x} has not reached {} yet", cur, target);
            assert!(!satisfied(Kind::Index, cur, target));
        }
        for fwd in 0..=8u32 {
            let cur = target.wrapping_add(fwd);
            assert!(!still_waiting(Kind::Index, cur, target, None),
                    "the index {:#x} has passed {}", cur, target);
            assert!(satisfied(Kind::Index, cur, target));
        }
        // the same at the very wrap: the target is behind the boundary, the index is before it
        assert!(still_waiting(Kind::Index, 0xFFFF_FFFE, 2, None));
        assert!(!still_waiting(Kind::Index, 2, 0xFFFF_FFFE, None), "the index has wrapped and passed the target");
        // and a naive comparison of the two numbers answers the opposite in both cases
        let (before, after) = (0xFFFF_FFFEu32, 2u32);
        assert!(before >= after && !reached(before, after), "naively the index before the wrap has \"reached\" a target after it");
        assert!(after < before && reached(after, before), "and the one that really reached it looks as if it did not");
    }

    /// The half of the ring is the boundary of the answer, and it must be the same
    /// on both sides: `reached` is "the difference is not negative", so exactly
    /// half of the ring counts as "ahead".
    #[test]
    fn half_the_ring_ahead_is_reached_and_half_behind_is_not() {
        let target = 1000u32;
        assert!(reached(target.wrapping_add(0x7FFF_FFFF), target), "the last step of the near half");
        assert!(!reached(target.wrapping_add(0x8000_0000), target), "beyond the half it is the past");
        assert!(!reached(target.wrapping_sub(1), target));
        // the antipodal point is the one the ring cannot tell "ahead" from "behind" at:
        // it is counted as behind, and — what matters — the same way from both sides
        assert_eq!(target.wrapping_add(0x8000_0000), target.wrapping_sub(0x8000_0000));
        assert!(!reached(target.wrapping_sub(0x8000_0000), target));
    }

    #[test]
    fn a_doorbell_ends_at_a_ring_or_at_the_index() {
        // the door is unchanged, the index has not grown up: we wait
        assert!(still_waiting(Kind::Door, 3, 3, Some((7, 8))));
        // the door was rung: we go, whatever the index says
        assert!(!still_waiting(Kind::Door, 4, 3, Some((7, 8))));
        // the partner passed the threshold without ringing: the index releases us
        assert!(!still_waiting(Kind::Door, 3, 3, Some((8, 8))));
        assert!(!still_waiting(Kind::Door, 3, 3, Some((9, 8))));
        // no index was passed — the door alone holds us
        assert!(still_waiting(Kind::Door, 3, 3, None));
        assert!(!still_waiting(Kind::Door, 9, 3, None));
    }

    #[test]
    fn a_doorbells_index_is_compared_across_the_wrap_too() {
        assert!(still_waiting(Kind::Door, 1, 1, Some((0xFFFF_FFFF, 3))));
        assert!(!still_waiting(Kind::Door, 1, 1, Some((3, 0xFFFF_FFFF))));
    }

    /// The two sides must not disagree in the direction that costs a thread. For a
    /// word and for an index they are exact complements; for a doorbell the waker
    /// is allowed to be MORE eager (it does not read the index), and that is safe —
    /// the sleeper rechecks and goes back to sleep. The opposite would be a lost
    /// wakeup.
    #[test]
    fn the_waker_is_never_less_eager_than_the_sleeper() {
        let probes = [0u32, 1, 2, 5, 7, 8, 9, 1000, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFE, 0xFFFF_FFFF];
        for &word in &probes {
            for &val in &probes {
                for kind in [Kind::Word, Kind::Index, Kind::Door] {
                    let sleeps = still_waiting(kind, word, val, None);
                    let sat = satisfied(kind, word, val);
                    assert!(!(sleeps && sat) || kind == Kind::Door,
                            "{:?}: the sleeper waits on while the waker has written it off (word {:#x}, val {:#x})", kind, word, val);
                    if kind != Kind::Door {
                        assert_eq!(sleeps, !sat, "{:?}: the two sides disagree (word {:#x}, val {:#x})", kind, word, val);
                    } else {
                        // a doorbell: the waker may be the eager one, but it must not be
                        // the SLOWER one — that would be a lost wakeup
                        assert!(sat || sleeps, "a doorbell: nobody is going to wake it (word {:#x}, val {:#x})", word, val);
                    }
                }
            }
        }
        // with an index in hand a doorbell's sleeper may go on waiting while the
        // waker already says "satisfied" only when the door has been rung — and then
        // the sleeper does not wait
        for &(cur, target) in &[(0u32, 8u32), (7, 8), (8, 8), (0xFFFF_FFFF, 2)] {
            let door_rung = still_waiting(Kind::Door, 4, 3, Some((cur, target)));
            assert!(!door_rung, "a rung door releases the sleeper whatever the index is");
        }
    }

    #[test]
    fn a_sleeper_that_went_to_sleep_on_the_value_it_sees_never_waits_on_an_index() {
        // the entry into the wait: the caller checks the predicate first, and for an index
        // "already reached" must mean "do not sleep"
        for target in [0u32, 1, 0x8000_0000, 0xFFFF_FFFF] {
            assert!(!still_waiting(Kind::Index, target, target, None));
            assert!(satisfied(Kind::Index, target, target));
        }
    }
}
