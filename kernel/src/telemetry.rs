//! Telemetry of the datapath cell: pkts, p50/p99 latency (a histogram of
//! log2 buckets), gaps (a ring overflow), restarts (supervise), uptime.
//! Read by the console (`telem`) and printed by the cell periodically to the serial channel.
use core::sync::atomic::{AtomicU64, Ordering};

const NB: usize = 40; // log2 buckets by TSC cycles (up to ~2^40 cycles)
static PKTS: AtomicU64 = AtomicU64::new(0);
static LAT_MIN: AtomicU64 = AtomicU64::new(u64::MAX);
static LAT_MAX: AtomicU64 = AtomicU64::new(0);
static LAT_SUM: AtomicU64 = AtomicU64::new(0);
static HIST: [AtomicU64; NB] = {
    const Z: AtomicU64 = AtomicU64::new(0);
    [Z; NB]
};
static GAPS: AtomicU64 = AtomicU64::new(0);
static RESTARTS: AtomicU64 = AtomicU64::new(0);

#[inline] fn bucket(cycles: u64) -> usize {
    if cycles == 0 { return 0; }
    (63 - cycles.leading_zeros() as usize).min(NB - 1)
}

/// Account for the round-trip latency of one packet (in TSC cycles).
pub fn record_lat(cycles: u64) {
    PKTS.fetch_add(1, Ordering::Relaxed);
    LAT_SUM.fetch_add(cycles, Ordering::Relaxed);
    HIST[bucket(cycles)].fetch_add(1, Ordering::Relaxed);
    // min/max (without a CAS race on the critical path: there is one writer cell)
    if cycles < LAT_MIN.load(Ordering::Relaxed) { LAT_MIN.store(cycles, Ordering::Relaxed); }
    if cycles > LAT_MAX.load(Ordering::Relaxed) { LAT_MAX.store(cycles, Ordering::Relaxed); }
}

/// A skip/drop (the ring is full — the consumer did not keep up).
pub fn inc_gap() { GAPS.fetch_add(1, Ordering::Relaxed); }
/// A restart of the workload (supervise).
pub fn inc_restart() { RESTARTS.fetch_add(1, Ordering::Relaxed); }

pub fn pkts() -> u64 { PKTS.load(Ordering::Relaxed) }
pub fn gaps() -> u64 { GAPS.load(Ordering::Relaxed) }
pub fn restarts() -> u64 { RESTARTS.load(Ordering::Relaxed) }
pub fn lat_min() -> u64 { let m = LAT_MIN.load(Ordering::Relaxed); if m == u64::MAX { 0 } else { m } }
pub fn lat_max() -> u64 { LAT_MAX.load(Ordering::Relaxed) }

/// A latency percentile in TSC cycles (the bucket's lower bound — conservatively).
pub fn pctl(pct: u64) -> u64 {
    let total = PKTS.load(Ordering::Relaxed);
    if total == 0 { return 0; }
    let target = (total * pct).div_ceil(100); // ceil
    let mut acc = 0u64;
    for b in 0..NB {
        acc += HIST[b].load(Ordering::Relaxed);
        if acc >= target { return 1u64 << b; }
    }
    lat_max()
}

fn ns(cycles: u64, hz: u64) -> u64 { if hz > 0 { cycles * 1_000_000_000 / hz } else { 0 } }

/// A telemetry snapshot (the console's `telem` / the cell's periodic serial channel).
pub fn dump() {
    let hz = crate::tsc::calibrate_cached();
    let up = if hz > 0 { crate::tsc::rdtsc() / hz } else { 0 };
    let p50 = pctl(50); let p99 = pctl(99);
    let mn = lat_min(); let mx = lat_max();
    crate::println!(
        "telem: pkts={} p50={}c({}ns) p99={}c({}ns) min={}c max={}c gaps={} restarts={} uptime={}s",
        pkts(), p50, ns(p50, hz), p99, ns(p99, hz), mn, mx, gaps(), restarts(), up);
}
