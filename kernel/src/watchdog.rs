//! A workload health watchdog: it watches the datapath's progress (pkts).
//! No progress for STALL_LIMIT checks in a row while the watchdog is armed -> declare
//! a stall and record a restart (telemetry + the flight recorder). tick() is called
//! periodically (a timer/health loop) with the current progress counter.
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const STALL_LIMIT: u64 = 3; // this many "no progress" in a row = a hang

static ARMED: AtomicBool = AtomicBool::new(false);
static LAST: AtomicU64 = AtomicU64::new(0);
static STALL: AtomicU64 = AtomicU64::new(0);
static STALLS_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Arm the watchdog with the current progress value as the baseline.
pub fn arm(progress: u64) {
    LAST.store(progress, Ordering::SeqCst);
    STALL.store(0, Ordering::SeqCst);
    ARMED.store(true, Ordering::SeqCst);
}
pub fn stalls_total() -> u64 { STALLS_TOTAL.load(Ordering::Relaxed) }

/// The health check. Returns true if a stall->restart was recorded on this tick.
pub fn tick(progress: u64) -> bool {
    if !ARMED.load(Ordering::SeqCst) { return false; }
    let last = LAST.load(Ordering::SeqCst);
    if progress == last {
        let n = STALL.fetch_add(1, Ordering::SeqCst) + 1;
        if n >= STALL_LIMIT {
            // the workload hung: record a restart
            STALLS_TOTAL.fetch_add(1, Ordering::Relaxed);
            crate::telemetry::inc_restart();
            crate::fr::record(crate::fr::RESTART, progress);
            crate::println!("watchdog: STALL — the workload made no progress for {} checks (pkts={}) -> RESTART",
                n, progress);
            STALL.store(0, Ordering::SeqCst); // reset after reacting (the workload was restarted)
            return true;
        }
    } else {
        LAST.store(progress, Ordering::SeqCst);
        STALL.store(0, Ordering::SeqCst);
    }
    false
}
