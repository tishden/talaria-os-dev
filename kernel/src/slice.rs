//! The scheduler's tick as POLICY: the slice, the short slice under a network waiter, the
//! long tick on a lonely core, and the period a core wants right now. Numbers and rules
//! that were measured on x86-64 and hold for any core with a per-core timer; the MECHANISM
//! (programming the LAPIC or the generic timer) is `arch::smp`, which re-exports these names
//! so the callers keep saying `crate::smp::effective_slice_ns()`.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use crate::percpu::MAXCPU;

/// `talaria.slice=N` — an override of the timer's initial counter (0 — as it is).
pub static TIMER_SLICE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);


/// The scheduler's slice by default, nanoseconds.
///
/// Half a millisecond is A MEASUREMENT rather than a taste. A sweep by the slice on the full ClickHouse
/// suite (`services/clickhouse/sweep.sh`, 4 cores, the median):
/// 8 ms — 308 ms per suite, 2 ms — 246, 0.5 ms — 216, 0.2 ms — 227. Below
/// half a millisecond the gain is eaten by the preemption itself (a growth on the rows
/// `uniqExact` and `LIKE` is visible at once), above it a thread waits for a foreign quantum.
///
/// The former eight milliseconds were not a choice but an inheritance: exactly that came out
/// under QEMU from the hard-wired LAPIC counter when the slice was still set by a
/// counter rather than by time. They were held by one bug — on a short slice the gate
/// `tsync` stalled — and it is closed: a waiter whose condition is fulfilled is now visible
/// to the scheduler by itself (`sync::ready_now`).
pub const SLICE_NS: u32 = 500_000;

/// A SHORT SLICE ON A CORE WHERE SOMEBODY WAITS FOR THE NETWORK (nanoseconds; 0 — off).
/// The knob `talaria.netslice=<us>`.
///
/// Why a separate slice. There are no interrupts from the card in this build: the stack
/// is driven ONLY from the syscalls, that is, a frame is seen by the thread that waits
/// for it in a `recv`/`epoll_wait`. While the core is held by a COMPUTING neighbour, the frame will be
/// seen by nobody, and the delivery's latency is bounded from below by the slice. In the measurement that
/// looks unambiguous: mongod's p50 is 69 us (better than Linux's), while its p99 is 530, that
/// is, exactly the quantum.
///
/// Cutting the slice for the whole machine is not allowed — it was chosen by a measurement of the
/// throughput (see SLICE_NS). So it is cut ONLY on that core and only
/// for the time while somebody there really waits for the network (`netsys::poller_active`),
/// and it comes back by itself.
pub static NETSLICE_NS: AtomicU64 = AtomicU64::new(100_000);

pub fn netslice_from_cmdline(cmdline: &str) {
    if let Some(v) = crate::cmdline::value(cmdline, "talaria.netslice") {
        if let Ok(us) = v.parse::<u64>() {
            NETSLICE_NS.store(us * 1000, Ordering::SeqCst);
            crate::println!("smp: the slice on a core with a network waiter is {} us{}", us,
                if us == 0 { " (it is off: the slice is the common one)" } else { "" });
        }
    }
}

/// A LONG TICK PERIOD ON A CORE WHERE THERE IS NOBODY TO PREEMPT (nanoseconds; 0 — off).
/// The knob `talaria.idleslice=<us>`.
///
/// A tick is A PREEMPTION. When there is exactly one ready task on a core, there is nobody
/// to preempt, and the whole tick goes on the housekeeping: the acknowledgement of the interrupt, the clock,
/// the cleanup, the accounting. It is paid for two thousand times a second meanwhile and costs about
/// two microseconds — that is, on a latency-critical thread it is exactly that tail
/// which is visible in the measurement and which Linux does not have.
///
/// The measurement (`actors pingpong raw`, 100 thousand turnarounds, nanoseconds):
///   the slice 0.5 ms — p50 213, p99 248, **p99.9 3767**
///   the slice 4 ms   — p50 229, p99 316, **p99.9 463**   (with Linux 300-500)
/// That is, the whole tail is the tick, and it is removed by thinning rather than by cheapening.
///
/// A HYSTERESIS IS OBLIGATORY. The sign "there is nobody to preempt" with a ping-pong flickers at
/// every turnaround (the partner is now ready, now asleep), while every reconfiguration is
/// a write into the LAPIC. So the period is lengthened only after `SOLO_HOLD` ticks in
/// a row in solitude, and is shortened at the very first observation of a neighbour.
///
/// WHY 2 MS. The break because of which the default was first set at 700 us
/// (the bistable collapse of `actors fanout tsync spread`) turned out NOT to be a property
/// of the long tick: the long tick was the trigger, while what fired was WORK STEALING —
/// it took a task from a core with two onto a core with one, that is, it turned
/// the skew over instead of smoothing it (see `affinity::steal_candidate`).
/// After the steal was fixed there is no collapse either at 2000 or at 4000 (eight runs
/// in a row), while the tail at 2 ms is better than at 700: `pingpong raw` p99.9 268 ns
/// against 473-579.
pub static IDLESLICE_NS: AtomicU64 = AtomicU64::new(2_000_000);
const SOLO_HOLD: u32 = 4;
static SOLO_RUN: [AtomicU32; MAXCPU] = [const { AtomicU32::new(0) }; MAXCPU];
/// The core is on a long tick right now — a waker must ring it explicitly (see sync.rs).
static LONG_TICK: [AtomicU32; MAXCPU] = [const { AtomicU32::new(0) }; MAXCPU];
pub fn long_tick(cpu: usize) -> bool {
    cpu < MAXCPU && LONG_TICK[cpu].load(Ordering::Relaxed) != 0
}
/// The arch's `retune_timer` records whether the period it set is longer than the slice.
pub fn set_long_tick(cpu: usize, long: bool) {
    if cpu < MAXCPU { LONG_TICK[cpu].store(long as u32, Ordering::Relaxed); }
}

pub fn idleslice_from_cmdline(cmdline: &str) {
    if let Some(v) = crate::cmdline::value(cmdline, "talaria.idleslice") {
        if let Ok(us) = v.parse::<u64>() {
            IDLESLICE_NS.store(us * 1000, Ordering::SeqCst);
            crate::println!("smp: a tick on a core without neighbours goes once every {} us{}", us,
                if us == 0 { " (it is off: the period is the common one)" } else { "" });
        }
    }
}

/// The tick's period that this core WANTS right now.
/// `solo` means "there are no other ready tasks on this core".
pub fn want_period_ns(cpu: usize, solo: bool) -> u64 {
    let c = cpu.min(MAXCPU - 1);
    let eff = effective_slice_ns();
    let ns = NETSLICE_NS.load(Ordering::Relaxed);
    if ns != 0 && ns < eff && crate::netsys::poller_active(cpu) {
        SOLO_RUN[c].store(0, Ordering::Relaxed);
        return ns;
    }
    let idle = IDLESLICE_NS.load(Ordering::Relaxed);
    // The profile by the tick (`talaria.prof=`) samples EXACTLY by the tick: to thin it
    // means to coarsen the profile fourfold exactly when it has been switched on.
    if !solo || idle <= eff || crate::sched::PROF_ON.load(Ordering::Relaxed) {
        SOLO_RUN[c].store(0, Ordering::Relaxed);
        return eff;
    }
    if SOLO_RUN[c].fetch_add(1, Ordering::Relaxed) + 1 >= SOLO_HOLD { idle } else { eff }
}

/// The effective slice: the knob `talaria.slice=<ns>`, otherwise the default. The decisions
/// that depend on the quantum's length (for example "do not steal a cache-hot task")
/// must ask exactly it rather than a constant — otherwise the knob changes the scheduler's
/// behaviour only halfway.
pub fn effective_slice_ns() -> u64 {
    let over = TIMER_SLICE.load(Ordering::Relaxed);
    if over != 0 { over as u64 } else { SLICE_NS as u64 }
}

