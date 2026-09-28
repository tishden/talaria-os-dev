//! Observability: `top`, `ps`, `netstat`, `kstat`, `perf`.
//!
//! ## Why this is in the kernel rather than a utility in ring 3
//!
//! Talaria is made for predictable latency, and the main question about it is always
//! one: "where did the microseconds go". THE SYSTEM ITSELF must answer it, and exactly
//! at the moment when answering is hardest: the workload has degraded, a CPU core
//! is busy with the datapath, and there may be no spare process in ring 3 at all. So
//! observability lives where the operator console does — in the kernel on the BSP
//! (the control plane) — and depends on nothing but serial.
//!
//! ## What is different from Linux
//!
//! | Linux | Talaria | why |
//! |---|---|---|
//! | `/proc` as an FS | the tables straight from the kernel | there is no reader process, hence no syscalls to read |
//! | `%CPU` by ticks | by TSC cycles | on a DEDICATED core there is no tick at all — ticks lie with a zero |
//! | `perf_event_open` | `rdpmc` + an IPI on request | we have no perf subsystem syscalls |
//! | RSS from a counter | a table walk on demand | a counter would cost work on the fault path |
//!
//! ## The price of observing
//!
//! Everything collected CONSTANTLY costs one Relaxed add on an event that is
//! two orders of magnitude more expensive anyway (a context switch, a syscall, a fault), plus seven
//! `rdpmc` per scheduler tick. On the zero-syscall fast path it costs nothing: there
//! are no syscalls, no ticks and no switches there. Everything more expensive — a walk of the page
//! tables for RSS, a `perf poke` into a dedicated core — is done ONLY on the operator's explicit
//! request, and the command's help says so.

pub mod kstat;
pub mod netstat;
pub mod perf;
pub mod top;
pub mod unwind;

use core::fmt::Write;

// ── formatting ───────────────────────────────────────────────────────────────
/// A short string on the stack: `no_std` without an allocator, while the columns need text.
pub struct S<const N: usize> {
    b: [u8; N],
    n: usize,
}
impl<const N: usize> S<N> {
    pub const fn new() -> Self { S { b: [0; N], n: 0 } }
    pub fn as_str(&self) -> &str { core::str::from_utf8(&self.b[..self.n]).unwrap_or("?") }
}
impl<const N: usize> Write for S<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &c in s.as_bytes() {
            if self.n < N { self.b[self.n] = c; self.n += 1; }
        }
        Ok(())
    }
}
impl<const N: usize> core::fmt::Display for S<N> {
    // Exactly `pad` rather than `write_str`: the tables' columns are given through `{:<15}` /
    // `{:>6}`, and `write_str` silently ignores those directives — the table
    // falls apart into mush exactly when it is being read.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { f.pad(self.as_str()) }
}

/// Bytes for a human: 1023 -> `1023`, 1048576 -> `1.0M`. One significant fractional
/// digit — a `top` column is never wider than eight characters.
pub fn bytes(v: u64) -> S<12> {
    let mut s = S::new();
    const U: [(u64, &str); 4] = [(1 << 40, "T"), (1 << 30, "G"), (1 << 20, "M"), (1 << 10, "K")];
    for (d, n) in U {
        if v >= d {
            let w = v / d;
            let f = (v % d) * 10 / d;
            if w >= 10 { let _ = write!(s, "{}{}", w, n); } else { let _ = write!(s, "{}.{}{}", w, f, n); }
            return s;
        }
    }
    let _ = write!(s, "{}", v);
    s
}
/// A per-second number: 12345 -> `12.3k`. The arguments are the counter and the window in nanoseconds.
pub fn rate(delta: u64, win_ns: u64) -> S<12> {
    let mut s = S::new();
    if win_ns == 0 { let _ = write!(s, "-"); return s; }
    // u128: a byte counter per second reaches tens of gigabytes, and `delta * 1e9`
    // would overflow silently in u64 — the value would come out smaller than the real one.
    let per = (delta as u128 * 1_000_000_000 / win_ns as u128) as u64;
    const U: [(u64, &str); 3] = [(1_000_000_000, "G"), (1_000_000, "M"), (1_000, "k")];
    for (d, n) in U {
        if per >= d {
            let w = per / d;
            let f = (per % d) * 10 / d;
            if w >= 10 { let _ = write!(s, "{}{}", w, n); } else { let _ = write!(s, "{}.{}{}", w, f, n); }
            return s;
        }
    }
    let _ = write!(s, "{}", per);
    s
}
/// Bytes per second.
pub fn brate(delta_bytes: u64, win_ns: u64) -> S<12> {
    let mut s = S::new();
    if win_ns == 0 { let _ = write!(s, "-"); return s; }
    let per = (delta_bytes as u128 * 1_000_000_000 / win_ns as u128) as u64;
    let _ = write!(s, "{}", bytes(per));
    s
}
/// A share in hundredths of a percent -> `99.8%`.
pub fn pct(p100: u64) -> S<10> {
    let mut s = S::new();
    let _ = write!(s, "{}.{}%", p100 / 100, (p100 / 10) % 10);
    s
}
/// Nanoseconds -> `1d 02:03:04` / `02:03:04` / `3.21s` / `812ms`.
pub fn dur(ns: u64) -> S<16> {
    let mut s = S::new();
    let secs = ns / 1_000_000_000;
    if secs == 0 {
        if ns >= 1_000_000 { let _ = write!(s, "{}ms", ns / 1_000_000); }
        else if ns >= 1_000 { let _ = write!(s, "{}us", ns / 1_000); }
        else { let _ = write!(s, "{}ns", ns); }
        return s;
    }
    let (d, h, m, sec) = (secs / 86400, (secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    if d > 0 { let _ = write!(s, "{}d {:02}:{:02}:{:02}", d, h, m, sec); }
    else { let _ = write!(s, "{:02}:{:02}:{:02}", h, m, sec); }
    s
}

/// "How much missed" in one line. The share is printed only if the counter has a
/// DENOMINATOR (LLC accesses / total branches): on a machine where the PMU slots
/// sufficed only for the numerators, the share is not computed at all, and printing a zero
/// instead of it would be lying. MPKI (per thousand instructions) is always there.
pub fn miss(num: u64, mpki100: u64, has_ratio: bool, pct100: u64) -> S<40> {
    use core::fmt::Write;
    let mut s = S::new();
    if has_ratio { let _ = write!(s, "{} ({}, {}.{:02}/1000i)", num, pct(pct100), mpki100 / 100, mpki100 % 100); }
    else { let _ = write!(s, "{} ({}.{:02}/1000i)", num, mpki100 / 100, mpki100 % 100); }
    s
}
/// The same for LLC and branch-predictor misses from a snapshot.
pub fn miss_llc(s: &crate::pmu::Sample) -> S<40> { miss(s.llc_miss, s.llc_mpki100(), s.has_llc_ratio(), s.llc_miss_pct100()) }
pub fn miss_br(s: &crate::pmu::Sample) -> S<40> { miss(s.br_miss, s.br_mpki100(), s.has_br_ratio(), s.br_miss_pct100()) }

/// A thread's name as a printable string (a thread may not have named itself).
pub fn comm(t: usize) -> S<16> {
    let mut s = S::new();
    let c = crate::sched::comm(t);
    let l = crate::sched::comm_len(t);
    if l == 0 { let _ = write!(s, "-"); return s; }
    for &b in &c[..l] { let _ = write!(s, "{}", if b.is_ascii_graphic() { b as char } else { '?' }); }
    s
}

/// A task's state in one word (`ps`/`top`).
pub fn state(t: usize) -> &'static str {
    match crate::sched::state_of(t) {
        0 => "DEAD ",
        1 => "BUILD",
        2 => "RUN  ",
        4 => "WAIT ",
        _ => "READY",
    }
}

/// A core's role in one word.
pub fn role(cpu: usize) -> &'static str {
    match crate::affinity::role(cpu) { 0 => "off", 1 => "shared", _ => "dedic" }
}

/// The operator terminal's width (`talaria.cols=`), and whether it suffices for the wide tables.
pub fn wide() -> bool { crate::shell::cols() >= 100 }

// ── the periodic snapshot into the log ───────────────────────────────────────
//
// `talaria.top=<ms>` — print a `top` snapshot into serial every N milliseconds.
//
// WHY THIS EXISTS. The machine the OS is made for usually has no console:
// a `*.metal` beyond an external perimeter, no keyboard and no operator nearby. The only
// channel outwards is the same serial the log goes into, and a degradation that happened
// at night must stay IN THE LOG rather than wait for someone to type `top`.
// It is the same trick as `vmstat 5 > file` on a duty machine, only without a
// process, a file and an FS.
//
// THE PRICE. The snapshot is printed from THE SCHEDULER TICK, i.e. from an interrupt
// handler, and costs tens of lines into serial (~a millisecond at 115200 baud) plus
// a walk of the page tables for RSS. So: the interval is no shorter than 100 ms, the table
// walk is OFF by default (`talaria.top=1000,rss` turns it on), and the whole
// idea is on explicit request rather than by default.
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
static PERIOD_NS: AtomicU64 = AtomicU64::new(0);
static NEXT_NS: AtomicU64 = AtomicU64::new(0);
static PERIOD_RSS: AtomicBool = AtomicBool::new(false);
// `talaria.stat=<ms>` — the same for the COUNTER BLOCK `stat:`.
//
// Why a separate knob. The `stat:` block used to be printed in exactly one place — when
// ALL the tasks had FINISHED. For a workload that is stopped by a timeout (ClickHouse
// and the database servers live exactly as long as the runner gave them) that moment
// NEVER comes, and the counters are not visible at all: work steals, the skew,
// block cache misses, the tick's price. There was nothing to analyse such a workload's
// loss with — one had to trust a guess. A snapshot by time removes that
// limitation for ANY workload and any way of stopping it.
//
// The price is the same as `top`'s: tens of lines into serial from an interrupt handler.
// So the interval is no shorter than a second and only on explicit request.
static STAT_NS: AtomicU64 = AtomicU64::new(0);
static STAT_NEXT: AtomicU64 = AtomicU64::new(0);
/// Periodic printing into serial is allowed AT ALL (`talaria.stat=` is set).
/// Any line a driver prints "every N seconds" must ask
/// here: serial on Nitro is an exit to the hypervisor on EVERY byte with interrupts
/// disabled, and two ENA statistics lines every 30 s gave redis on EC2
/// "a max of 4.8-4.9 ms" at every point with a p99.99 of 70 µs.
pub fn periodic_print_on() -> bool { STAT_NS.load(Ordering::Relaxed) != 0 }

pub fn configure_from_cmdline(cmdline: &str) {
    if let Some(v) = crate::cmdline::value(cmdline, "talaria.stat") {
        match v.parse::<u64>() {
            Ok(ms) if ms > 0 => {
                let ms = ms.max(1000);
                STAT_NS.store(ms * 1_000_000, Ordering::SeqCst);
                STAT_NEXT.store(0, Ordering::SeqCst);
                crate::println!("talaria: the stat block into the log every {} ms (the workload may be stopped by a timeout); it goes onto the line in portions — if it cannot keep up, a snapshot is skipped and that is visible in the counter", ms);
            }
            _ => crate::println!("talaria: talaria.stat={} — not a number of milliseconds, the snapshot was not enabled", v),
        }
    }
    if crate::cmdline::flag(cmdline, "talaria.statidle") {
        stat_idle_on();
        crate::println!("talaria: the stat block goes onto the line only when the network is quiet (talaria.statidle=1)");
    }
    let Some(v) = crate::cmdline::value(cmdline, "talaria.top") else { return };
    let (ms, rss) = match v.split_once(',') { Some((a, b)) => (a, b.contains("rss")), None => (v, false) };
    let Ok(ms) = ms.parse::<u64>() else {
        crate::println!("talaria: talaria.top={} — not a number of milliseconds, the snapshot was not enabled", v);
        return;
    };
    if ms == 0 { return; }
    let ms = ms.max(100);
    PERIOD_NS.store(ms * 1_000_000, Ordering::SeqCst);
    PERIOD_RSS.store(rss, Ordering::SeqCst);
    NEXT_NS.store(0, Ordering::SeqCst);
    crate::println!("talaria: a top snapshot into the log every {} ms (the table walk for RSS is {}); it goes onto the line in portions — a short period cannot be carried by a polled UART, and the skips are in the counter",
        ms, if rss { "on — it costs milliseconds" } else { "off" });
}

/// Called from the BSP's tick. One Relaxed load when the mode is off.
#[inline]
pub fn periodic_tick() {
    // A portion of the assembled block comes before a new snapshot: the budget is 150 µs per tick
    // (the tick's price is 2.3 µs, so a snapshot costs like a dozen
    // ticks rather than like a hundred thousand).
    stat_flush(150_000);
    stat_tick();
    crate::console::tickcmd_tick();
    let p = PERIOD_NS.load(Ordering::Relaxed);
    if p == 0 { return; }
    let now = crate::tsc::mono_ns();
    if now < NEXT_NS.load(Ordering::Relaxed) { return; }
    NEXT_NS.store(now + p, Ordering::Relaxed);
    let rss = PERIOD_RSS.load(Ordering::Relaxed);
    if !deferred_print(|| top::cmd(if rss { "-t" } else { "-t -m" }, false)) {
        TOP_SKIPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Snapshots assembled into the buffer and skipped (the previous one is not poured out yet).
static STAT_TAKEN: AtomicU64 = AtomicU64::new(0);
static STAT_SKIPPED: AtomicU64 = AtomicU64::new(0);
static STAT_DRAIN_VISITS: AtomicU64 = AtomicU64::new(0);
/// `top` snapshots skipped: the previous block is not poured onto the line yet. It is printed in
/// the `stat:` block — otherwise "a snapshot every second" silently becomes "every
/// three", and the log does not show that.
pub static TOP_SKIPPED: AtomicU64 = AtomicU64::new(0);
/// The worst ASSEMBLY of a block into memory (a tick, IF=0) — it must be cheap too.
static STAT_BUILD_TSC: AtomicU64 = AtomicU64::new(0);

/// `talaria.stat=<ms>`: the counter block into the log by time rather than on the tasks'
/// completion. One Relaxed load when the mode is off.
///
/// PRINTING FROM THE TICK IS NOT ALLOWED — the instrument spoiled exactly what it measured. The block is
/// 25 KB over a polled UART: every byte waits for THRE and costs a VMEXIT, and all of
/// that with IF=0. A measurement: mongodb insert (gp3, SMP=4, 5000/s) with `talaria.stat=`
/// gave a maximum of 417-510 ms and a p99.999 of 353 ms, and without it a maximum of 16.7 ms.
/// For half a year that stall counted as "the WiredTiger journal's preallocation" and stood in
/// the TODO as the workload's tail. Now the tick only SCHEDULES a snapshot, while it is printed by
/// a core that has nothing to do (`stat_drain` in `sched::ap_loop`): there IF=1,
/// there is no task on the core, and the slow printing holds nobody up.
#[inline]
fn stat_tick() {
    let p = STAT_NS.load(Ordering::Relaxed);
    if p == 0 { return; }
    let now = crate::tsc::mono_ns();
    if now < STAT_NEXT.load(Ordering::Relaxed) { return; }
    STAT_NEXT.store(now + p, Ordering::Relaxed);
    // The snapshot is ASSEMBLED into memory (without a single VMEXIT), and it goes into the UART
    // in portions by a time budget — see `serial::capture_*`. If the previous block is not
    // poured out yet, we skip this one, naming the skip in the counter.
    if !crate::serial::capture_begin() { STAT_SKIPPED.fetch_add(1, Ordering::Relaxed); return; }
    STAT_TAKEN.fetch_add(1, Ordering::Relaxed);
    let t0 = crate::tsc::rdtsc();
    stat_block(now, now, "snapshot");
    crate::serial::capture_end();
    let d = crate::tsc::rdtsc().wrapping_sub(t0);
    if d > STAT_BUILD_TSC.load(Ordering::Relaxed) { STAT_BUILD_TSC.store(d, Ordering::Relaxed); }
}

/// Pour out the next portion of the assembled block.
///
/// THE INSTRUMENT TAKES NO MORE THAN A TENTH OF A CORE. A byte into a polled UART costs
/// 40-60 µs (measured on the stand: 58 KB in 2.5 s of clean time), and a 7 KB block
/// is 300 ms of CPU, however it is cut. So one must cut not only the
/// portion but the FREQUENCY: a dozen bytes every 5 ms is an event of half a millisecond
/// and a tenth of a core, and the block arrives in three seconds. A snapshot every 6-12 s
/// then fits entirely, and the workload's tail no longer sees it.
/// The portion is chosen BY ITSELF from the requested period: the block must get out
/// entirely before the next snapshot, while the portion stays small. Ask for a snapshot
/// every 12 s — six bytes at a time suffice; ask for one every second — the portion
/// will grow, and that is honest: it was asked for.
const FLUSH_EVERY_NS: u64 = 2_000_000;
const FLUSH_BYTES_MAX: usize = 32;
static FLUSH_LAST: AtomicU64 = AtomicU64::new(0);
/// The shortest of the requested printing periods: a block must get out before
/// the next one like it. Zero — no periodic printing was requested.
fn min_period_ns() -> u64 {
    let (a, b) = (STAT_NS.load(Ordering::Relaxed), PERIOD_NS.load(Ordering::Relaxed));
    match (a, b) { (0, x) | (x, 0) => x, (x, y) => x.min(y) }
}
fn flush_bytes() -> usize {
    let period = min_period_ns();
    let left = crate::serial::capture_left();
    if period == 0 || left == 0 { return FLUSH_BYTES_MAX; }
    // portions per period (with a twofold margin — some passes will find the port busy)
    let slots = (period / FLUSH_EVERY_NS).max(1) / 2;
    ((left / slots.max(1) as usize) + 1).clamp(8, FLUSH_BYTES_MAX)
}
pub fn stat_flush(budget_ns: u64) {
    if !crate::serial::capture_pending() { return; }
    let now = crate::tsc::rdtsc();
    if !net_quiet(now) { return; }
    let hz = crate::tsc::calibrate_cached().max(1);
    let gap = ((hz as u128 * FLUSH_EVERY_NS as u128) / 1_000_000_000u128) as u64;
    let last = FLUSH_LAST.load(Ordering::Relaxed);
    if now.wrapping_sub(last) < gap { return; }
    // WHILE A NETWORK WAITER POURS THE BLOCK, THE TICK DOES NOT (see `stat_flush_spin`).
    if now.wrapping_sub(FLUSH_SPIN_LAST.load(Ordering::Relaxed)) < gap * 10 { FLUSH_TICK_SKIP.fetch_add(1, Ordering::Relaxed); return; }
    FLUSH_LAST.store(now, Ordering::Relaxed);
    crate::serial::capture_flush(budget_ns, flush_bytes());
}
/// A portion of the block FROM A NETWORK WAITER'S SPIN — one byte at a time.
///
/// Analyses of slow deliveries under `talaria.stat=` ALL without exception
/// ran into a tick that entered before the frame arrived and wrote its decision
/// 120-200 µs later. A byte into the port on Nitro is an exit to the host, 12.6 µs; a portion
/// of 8 bytes from a tick is 100-170 µs with IF=0 on the core where a ready reader lies. With
/// a 15 KB block that is 4 s of standing, 2 ms out of every 30 — exactly the hundredth of the
/// requests, i.e. the instrument was drawing precisely the p99 it was looking for.
/// A network waiter in a spin does nothing and stands with IF=1 without the stack's lock: a byte
/// from there delays only its own next poll — by one exit
/// to the host rather than by a portion. With live spinners the tick does not pour at all.
static FLUSH_SPIN_LAST: AtomicU64 = AtomicU64::new(0);
/// `talaria.statidle=1` — THE BLOCK GOES ONTO THE LINE ONLY WHEN THE NETWORK IS QUIET (200 ms
/// without socket deliveries). Even a byte from a spin cost up to 240-657
/// µs (an exit to the host for a port cannot be interrupted), and the analyses of slow deliveries
/// showed the occupant as "in a poll for 200 µs" — i.e. the instrument again. Under this
/// knob not A SINGLE byte goes into the port while the workload runs; the last snapshot lives
/// in memory (the next one replaces it) and is poured out when the generator falls silent.
/// The counters are cumulative — only the analyses of a non-final window are lost.
static STAT_IDLE: AtomicBool = AtomicBool::new(false);
pub fn stat_idle_on() { STAT_IDLE.store(true, Ordering::Relaxed); }
/// Whether the block is being poured in portions UNDER LOAD (the analyses are not written then —
/// they would show the pouring portion). Under `statidle` nothing is poured under load.
pub fn flush_active() -> bool { crate::serial::capture_pending() && !STAT_IDLE.load(Ordering::Relaxed) }
#[inline]
fn net_quiet(now: u64) -> bool {
    if !STAT_IDLE.load(Ordering::Relaxed) { return true; }
    let last = crate::netsys::NET_LAST_DL.load(Ordering::Relaxed);
    now.wrapping_sub(last) >= crate::tsc::calibrate_cached() / 5
}
pub static FLUSH_SPIN_BYTES: AtomicU64 = AtomicU64::new(0);
pub static FLUSH_TICK_SKIP: AtomicU64 = AtomicU64::new(0);
#[inline]
pub fn stat_flush_spin() {
    if !crate::serial::capture_pending() { return; }
    if !net_quiet(crate::tsc::rdtsc()) { return; }
    let n = crate::serial::capture_flush(30_000, 1);
    if n != 0 {
        FLUSH_SPIN_LAST.store(crate::tsc::rdtsc(), Ordering::Relaxed);
        FLUSH_SPIN_BYTES.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// PRINT A LARGE BLOCK WITHOUT STOPPING THE MACHINE. Everything printed "every
/// N" from a tick or from the datapath must go through here: the block is assembled in memory
/// and goes onto the line in portions (`serial::capture_*`). Printing a dozen lines
/// directly over a polled UART costs hundreds of milliseconds with IF=0 — and three times per
/// session that turned out to be the very "workload tail" people were looking for with it.
/// Where there is nobody to shepherd the portions (there is no tick on this core — the echo server), the block
/// is finished in place: the old behaviour.
pub fn deferred_print<F: FnOnce()>(f: F) -> bool {
    if !crate::serial::capture_begin() { return false; }
    f();
    crate::serial::capture_end();
    if !crate::sched::cpu_timer_armed(crate::percpu::cpu_id()) { crate::serial::capture_drain_blocking(); }
    true
}

/// Pay the printing debt from the idle loop, if there is one (`sched::start_idle`).
pub fn stat_drain() {
    STAT_DRAIN_VISITS.fetch_add(1, Ordering::Relaxed);
    stat_flush(2_000_000);   // the core has nothing to do anyway (the frequency is the same — the rule is one)
}

fn stat_block(due_ns: u64, now_ns: u64, whence: &str) {
    let _ = now_ns;
    crate::println!("stat: --- {} at {} ms --- role {}", whence, due_ns / 1_000_000, crate::runmode::role_path());
    crate::metrics::dump();
    // The network datapath's profile goes here too: for a workload stopped by a
    // timeout there is nobody to ask `netstat`, while the datapath's stages are needed exactly
    // where a network tail is being analysed.
    if crate::netprof::on() { crate::netprof::dump(); }
    // A summary about the instrument itself: a snapshot "from the tick" costs the workload hundreds
    // of milliseconds, so its count must be visible in the same log.
    let (taken, skipped) = stat_counts();
    let (b, c) = (crate::serial::CAP_BYTES.load(Ordering::Relaxed), crate::serial::CAP_TSC.load(Ordering::Relaxed));
    let hz = crate::tsc::calibrate_cached().max(1);
    crate::println!("stat: snapshots assembled {}, skipped (the previous one not poured out) {}, bytes that did not fit {}; poured {} bytes in {} ms ({} ns/byte)",
        taken, skipped, crate::serial::CAP_LOST.load(Ordering::Relaxed),
        b, c / (hz / 1000).max(1), if b == 0 { 0 } else { crate::tsc::cycles_to_ns(c, hz) / b });
    if TOP_SKIPPED.load(Ordering::Relaxed) != 0 {
        crate::println!("stat: top snapshots skipped {} (the previous block is not poured out yet)", TOP_SKIPPED.load(Ordering::Relaxed));
    }
    crate::println!("stat: the worst assembly of a block into memory: {} µs",
        crate::tsc::cycles_to_ns(STAT_BUILD_TSC.load(Ordering::Relaxed), hz) / 1000);
    crate::println!("stat: the worst single pour: {} µs for {} bytes (the tick's budget is 150 µs); from network waiters' spins {} bytes, the tick yielded to a spin {} times",
        crate::tsc::cycles_to_ns(crate::serial::CAP_WORST_TSC.load(Ordering::Relaxed), hz) / 1000,
        crate::serial::CAP_WORST_N.load(Ordering::Relaxed),
        FLUSH_SPIN_BYTES.load(Ordering::Relaxed), FLUSH_TICK_SKIP.load(Ordering::Relaxed));
    crate::sched::tick_dur_dump();
}

/// (snapshots assembled, skipped) — for the gate and for analysis.
pub fn stat_counts() -> (u64, u64) {
    (STAT_TAKEN.load(Ordering::Relaxed), STAT_SKIPPED.load(Ordering::Relaxed))
}
