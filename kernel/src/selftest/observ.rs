//! The self-tests of observability (`talaria.test=obs`, `talaria.test=obslive`).
//!
//! Observability breaks the most quietly of all: the command still prints the table,
//! it is just that the numbers in it become untrue. A zero in the `CPU%` column looks
//! exactly like "the thread does nothing" rather than "the accounting of time is broken", and one cannot
//! notice the difference without a gate. So what is checked here is not "the commands do not crash"
//! but the NUMBERS:
//!
//! * `obs` — the arithmetic without a workload: the formatting of quantities (a mistake here
//!   lies to the operator by the scale — "1.0M" instead of "1.0K"), the walk over the page tables for
//!   the RSS, the difference of PMU snapshots, and the fact that every command runs to the end;
//! * `obslive` — the same on a LIVE workload: the time on the processor, the syscalls per
//!   thread, the switches and the profile's samples are bound to be NON-ZERO after
//!   a multi-threaded workload has run.

use core::fmt::Write;

// Three frames before the fault: the chain in the stack is bound to be visible both in the text and in
// the core dump. `inline(never)` — otherwise the compiler would collapse them into one.
#[inline(never)]
fn core_fault_l1() { unsafe { core::ptr::write_volatile(0x0000_7EFF_DEAD_0000u64 as *mut u64, 1); } }
#[inline(never)]
fn core_fault_l2() { core_fault_l1(); core::hint::black_box(()); }
#[inline(never)]
fn core_fault_l3() { core_fault_l2(); core::hint::black_box(()); }

fn check(ok: &mut bool, cond: bool, what: &str) {
    if !cond { println!("obs: FAIL {}", what); *ok = false; }
}

/// The arithmetic and the formatting — without a workload and without the scheduler.
fn statics() -> bool {
    let mut ok = true;
    // The scale. A mistake by an order of magnitude does not crash here, it lies: "free 1.0M"
    // instead of "1.0G" reads as a disaster where everything is fine.
    check(&mut ok, crate::obs::bytes(0).as_str() == "0", "bytes(0)");
    check(&mut ok, crate::obs::bytes(1023).as_str() == "1023", "bytes(1023) without a unit");
    check(&mut ok, crate::obs::bytes(1024).as_str() == "1.0K", "bytes(1K)");
    check(&mut ok, crate::obs::bytes(1536).as_str() == "1.5K", "bytes(1.5K) — the fractional part");
    check(&mut ok, crate::obs::bytes(20 << 20).as_str() == "20M", "bytes(20M) — without a fractional part from 10 on");
    check(&mut ok, crate::obs::bytes(3 << 30).as_str() == "3.0G", "bytes(3G)");
    check(&mut ok, crate::obs::bytes(2 << 40).as_str() == "2.0T", "bytes(2T)");
    // The rates: a counter and a window. The window is in nanoseconds, the answer is per second.
    check(&mut ok, crate::obs::rate(999, 1_000_000_000).as_str() == "999", "rate(999/s) without a unit");
    check(&mut ok, crate::obs::rate(1000, 1_000_000_000).as_str() == "1.0k", "rate(1000/s) = 1.0k");
    check(&mut ok, crate::obs::rate(1000, 500_000_000).as_str() == "2.0k", "rate(1000 over 0.5 s) = 2000/s");
    check(&mut ok, crate::obs::rate(5, 0).as_str() == "-", "rate with a zero window does not divide by zero");
    check(&mut ok, crate::obs::brate(1 << 20, 1_000_000_000).as_str() == "1.0M", "brate(1 MB over 1 s)");
    // The percentages are in hundredths: 9876 = 98.7%
    check(&mut ok, crate::obs::pct(9876).as_str() == "98.7%", "pct(98.76)");
    check(&mut ok, crate::obs::pct(0).as_str() == "0.0%", "pct(0)");
    check(&mut ok, crate::obs::pct(10000).as_str() == "100.0%", "pct(100)");
    // The durations: three ranges and the carry over a day.
    check(&mut ok, crate::obs::dur(500).as_str() == "500ns", "dur(ns)");
    check(&mut ok, crate::obs::dur(1_500_000).as_str() == "1ms", "dur(ms)");
    check(&mut ok, crate::obs::dur(3_661_000_000_000).as_str() == "01:01:01", "dur(an hour)");
    check(&mut ok, crate::obs::dur(90_061_000_000_000).as_str() == "1d 01:01:01", "dur(a day)");

    // The walk over the tables for the RSS. We count the CURRENT address space: the kernel has no
    // private pages (the identity one is shared), so we check not "greater than zero"
    // but that the walk terminates and does not lie with a number greater than the physical memory.
    let cr3 = crate::paging::current_cr3() & !0xFFF;
    let (res, huge, shr, lazy) = crate::paging::as_footprint(cr3);
    let ram = crate::mem::ram_total_frames();
    check(&mut ok, res <= ram, "the RSS is not greater than the physical memory");
    check(&mut ok, huge <= res, "the huge pages are not more than the resident ones");
    check(&mut ok, shr <= res, "the shared ones are not more than the resident ones");
    println!("obs: the walk over AS {:#x}: resident {} pages, huge {}, shared {}, lazy {} (RAM {} pages)",
        cr3, res, huge, shr, lazy, ram);

    // The difference of PMU snapshots: the numbers differ on different machines, but the ARITHMETIC is one.
    let a = crate::pmu::Sample { cycles: 1000, instrs: 2000, ref_cycles: 900,
        llc_miss: 10, llc_ref: 1000, br_miss: 5, br_ret: 500, tsc: 0 };
    let b = crate::pmu::Sample { cycles: 3000, instrs: 8000, ref_cycles: 2900,
        llc_miss: 30, llc_ref: 3000, br_miss: 15, br_ret: 1500, tsc: 0 };
    let d = b.delta(&a);
    check(&mut ok, d.cycles == 2000 && d.instrs == 6000, "the difference of PMU snapshots");
    check(&mut ok, d.ipc100() == 300, "the IPC out of the difference (6000/2000 = 3.00)");
    check(&mut ok, d.llc_miss_pct100() == 100, "the share of LLC misses (20/2000 = 1.00%)");
    check(&mut ok, d.br_miss_pct100() == 100, "the share of the predictor's misses (10/1000 = 1.00%)");
    // A counter wrapped over the u64 boundary must not give "minus a billion":
    // the reload of a counter at a reprogramming is a regular event.
    let w = crate::pmu::Sample { cycles: 5, ..crate::pmu::Sample::ZERO }
        .delta(&crate::pmu::Sample { cycles: u64::MAX - 4, ..crate::pmu::Sample::ZERO });
    check(&mut ok, w.cycles == 10, "the difference across the counter's wrap");

    // THE UNWINDING OF THE STACK — on a synthetic chain of frames (see `obs::unwind`).
    if !crate::obs::unwind::selftest() { ok = false; }

    // The thinning of the samples: the rate is set in hertz, and the divisor is computed from
    // the slice. A mistake here does not crash — the profile simply covers the wrong window.
    let hz = crate::sched::prof_set_hz(200);
    let ticks = 1_000_000_000 / crate::smp::effective_slice_ns().max(1);
    check(&mut ok, hz <= 200 && hz > 0, "the rate of the samples is not above the ordered one");
    check(&mut ok, crate::sched::prof_hz() == hz, "the declared rate coincides with the computed one");
    let all = crate::sched::prof_set_hz(0);
    check(&mut ok, all == ticks, "a zero rate = a sample on every tick");
    let over = crate::sched::prof_set_hz(ticks * 10);
    check(&mut ok, over == ticks, "a rate above the tick does not invent samples");
    crate::sched::prof_set_hz(200);

    // The ring of the profile's samples: a write, a read, a wrap-around.
    crate::sched::prof_ring_reset();
    let (have0, tot0) = crate::sched::prof_ring_stats();
    check(&mut ok, have0 == 0 && tot0 == 0, "the profile's ring is empty after the reset");
    ok
}

/// Run all the observability commands: they are bound to RUN TO THE END even on
/// a machine where there is nothing (the FS is not mounted, the network is not up, the PMU is off).
/// It is at exactly that moment that the operator calls them.
fn commands() {
    println!("obs: --- ps ---");
    crate::obs::top::cmd_ps("");
    println!("obs: --- ps -t ---");
    crate::obs::top::cmd_ps("-t");
    println!("obs: --- top (one snapshot) ---");
    crate::obs::top::cmd("", false);
    println!("obs: --- netstat ---");
    crate::obs::netstat::cmd("");
    println!("obs: --- kstat ---");
    crate::obs::kstat::cmd("");
    println!("obs: --- kstat no-such-thing ---");
    crate::obs::kstat::cmd("no-such-thing");
    println!("obs: --- perf ---");
    crate::obs::perf::cmd("");
    println!("obs: --- perf top ---");
    crate::obs::perf::cmd("top");
    println!("obs: --- perf poke (one's own core and a non-existent one) ---");
    crate::obs::perf::cmd("poke 0");
    crate::obs::perf::cmd("poke 63");
    crate::obs::perf::cmd("poke nonsense");
    println!("obs: --- perf gdb ---");
    crate::obs::perf::cmd("gdb");
    println!("obs: --- perf ??? ---");
    crate::obs::perf::cmd("no-such-subcommand");
}

/// The live check: the numbers after a multi-threaded workload has run.
/// Called from `metrics::dump()` — that is, at the moment "all the tasks have finished".
pub fn live_report() {
    let mut ok = true;
    let n = crate::sched::task_count().min(crate::sched::max_tasks());
    let (mut run_any, mut sys_any, mut sw_any, mut frames_any) = (false, false, false, false);
    let mut run_sum = 0u64;
    for t in 0..n {
        let r = crate::sched::run_ns(t);
        run_sum += r;
        if r > 0 { run_any = true; }
        if crate::metrics::syscalls_of(t) > 0 { sys_any = true; }
        if crate::sched::switches(t) > 0 { sw_any = true; }
        if crate::metrics::frames_of(t) > 0 { frames_any = true; }
    }
    check(&mut ok, run_any, "the time on the processor is accounted for at least one task");
    check(&mut ok, sys_any, "the syscalls are counted PER THREAD, not only in total");
    check(&mut ok, sw_any, "the context switches are counted per task");
    check(&mut ok, frames_any, "the handed-out memory frames are attributed to a task");
    check(&mut ok, crate::sched::switches_total() > 0, "the switches are counted per core");
    // "Ready and not running" cannot grow for a workload that HAS FINISHED: an open
    // interval on a corpse is a false WAITED% in `top` (was caught by eye, session 26).
    let a: [u64; 64] = core::array::from_fn(|t| if t < n { crate::sched::rq_ns(t) } else { 0 });
    let t0 = crate::tsc::mono_ns();
    while crate::tsc::mono_ns() - t0 < 2_000_000 { core::hint::spin_loop(); }
    let frozen = (0..n.min(64)).all(|t| crate::sched::rq_ns(t) == a[t]);
    check(&mut ok, frozen, "the waiting for the processor of the finished tasks does not grow (there is no open interval on a corpse)");
    let (have, total) = crate::sched::prof_ring_stats();
    check(&mut ok, total > 0, "the profile has collected at least one sample");
    // A sample is bound to point at CODE, not at zero: an empty address means that
    // the interrupt's frame was parsed wrongly, and the whole profile is garbage.
    let mut stk = [0u64; crate::sched::PROFR_D];
    let (mut nonzero, mut deep, mut depth_sum) = (0usize, 0usize, 0u64);
    for i in 0..have {
        let (d, _t, _c, _r) = crate::sched::prof_ring_get(i, &mut stk);
        if d > 0 && stk[0] != 0 { nonzero += 1; }
        if d > 1 { deep += 1; }
        depth_sum += d as u64;
        // Every frame of the stack is bound to be an address of code, not a zero and not garbage:
        // a zero frame inside the stack means that the unwinder wrote down a length
        // it did not fill in.
        for k in 0..d { if stk[k] == 0 { check(&mut ok, false, "a zero frame inside the stack"); break; } }
    }
    check(&mut ok, have == 0 || nonzero == have, "all the profile's samples have a non-zero address");
    println!("obs-live: samples with a stack deeper than the leaf {} out of {}, the average depth {}",
        deep, have, if have > 0 { depth_sum / have as u64 } else { 0 });
    let mut s = crate::obs::S::<64>::new();
    let _ = write!(s, "{}", crate::obs::dur(run_sum));
    println!("obs-live: the processor time per task {}, switches {}, profile samples {} out of {}",
        s, crate::sched::switches_total(), have, total);
    // The whole table — through the operator's eyes. We do not count the RSS: the address
    // spaces have already been torn down, the walk would show zeroes and would only confuse.
    crate::obs::top::cmd_ps("-t -m");
    println!("obs-live: {}", if ok { "ALL OK" } else { "THERE ARE FAILURES" });
    if DEEP.load(core::sync::atomic::Ordering::Relaxed) { deep_report(); }
}

/// Whether to print the live report at the end of the workload (`talaria.test=obslive`).
static LIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Whether to check the DEPTH of the stack (`talaria.test=profdeep`: the workload is built with
/// frame pointers, and the chain is bound to unwind).
static DEEP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
pub fn live_armed() -> bool { LIVE.load(core::sync::atomic::Ordering::Relaxed) }

/// The report of the `profdeep` gate: the chain of the workload's frames is bound to unwind.
fn deep_report() {
    let mut ok = true;
    let mut stk = [0u64; crate::sched::PROFR_D];
    let (have, _) = crate::sched::prof_ring_stats();
    let (mut u3, mut deep, mut full, mut maxd) = (0u64, 0u64, 0u64, 0usize);
    // The most frequent stack by the first FOUR frames: a chain of five levels
    // is bound to repeat rather than scatter into unique ones.
    let mut best = [0u64; 4];
    let mut bestn = 0u64;
    let mut seen = [[0u64; 4]; 16];
    let mut cnt = [0u64; 16];
    let mut ns = 0usize;
    for i in 0..have {
        let (d, _t, _c, r) = crate::sched::prof_ring_get(i, &mut stk);
        if r != 3 || d == 0 { continue; }
        u3 += 1;
        if d > maxd { maxd = d; }
        if d > 1 { deep += 1; }
        if d >= 5 { full += 1; }
        if d < 4 { continue; }
        let mut k = [0u64; 4];
        for j in 0..4 { k[j] = stk[j] & !0xF; }
        let mut hit = false;
        for j in 0..ns { if seen[j] == k { cnt[j] += 1; hit = true; break; } }
        if !hit && ns < 16 { seen[ns] = k; cnt[ns] = 1; ns += 1; }
    }
    for j in 0..ns { if cnt[j] > bestn { bestn = cnt[j]; best = seen[j]; } }
    let mut check = |c: bool, what: &str| { if !c { println!("profdeep: FAIL {}", what); ok = false; } };
    check(u3 > 10, "samples in ring 3 have accumulated (the workload was computing under the profile)");
    // Five levels plus main plus the leaf — the unwinding is bound to reach at least
    // the fifth frame. Less means the chain breaks where there is nothing
    // to break: the workload is built WITH frame pointers.
    check(maxd >= 5, "the depth of the stack reaches five frames and beyond");
    check(deep * 2 > u3, "the MAJORITY of the ring 3 samples unwind deeper than the leaf");
    check(full > 0, "there are samples with a full chain (>=5 frames)");
    check(bestn > 1, "one and the same stack occurs many times (the chain is stable)");
    // The frames inside the stack are bound to DIFFER: identical ones mean a loop
    // that the unwinder did not notice.
    let uniq = best[0] != best[1] && best[1] != best[2] && best[2] != best[3];
    check(bestn == 0 || uniq, "the frames in the stack are distinct (not a loop)");
    println!("profdeep: ring3 samples {}, with a stack {}, the depth up to {}, the most frequent stack {} times: {:#x} <- {:#x} <- {:#x} <- {:#x}",
        u3, deep, maxd, bestn, best[0], best[1], best[2], best[3]);
    println!("profdeep: {}", if ok { "ALL OK" } else { "THERE ARE FAILURES" });
    crate::obs::perf::cmd("top");
    // The dump in the machine format is a part of the gate too: the parsing on the host
    // (`kernel/tools/talaria-prof`) reads exactly these lines, and if the format
    // drifts apart, the instrument will fall silent and nobody will notice.
    crate::obs::perf::cmd("record 12");
}

#[allow(unreachable_code)]
pub fn run(name: &str, _cmdline: &str) -> bool {
    match name {
        // The virtual /proc files are not truncated on a multi-core machine: the workload
        // sizes its pools by them, and the truncation is silent.
        "virtfiles" => {
            crate::smp::start_from_cmdline(_cmdline);
            crate::syscall::selftest_virtfiles();
            crate::suite::park()
        }
        // AN INSTRUMENT THAT CONTRADICTS ITSELF (session 42). The `stat:` block printed
        // "#202: 448 times, 188137 ms, 8190622 ns", where 448 * 8.19 ms = 3669 ms
        // rather than 188137: the multiplier `cycles * 1e9` overflows u64 at 188 seconds
        // of accumulated time. The gate holds the arithmetic (u128) and the parsing of the knob
        // `talaria.sysrow=`: a list of numbers that are printed INDEPENDENTLY of
        // the top ten by time (otherwise the price of `write` cannot be taken — under
        // mongodb it never gets into the top ten).
        "sysns" => {
            println!("talaria: the gate of the syscalls' arithmetic (ns per call + the parsing of talaria.sysrow=)");
            let hz = 2_899_930_012u64;
            // 188 137 ms of accumulated time over 448 calls — the numbers from the stand
            // of session 41; the right answer is 419-420 ms per call, the old formula
            // gave 8.19 ms (an overflow).
            let ticks = 188_137u64 * (hz / 1000);
            let ns = crate::metrics::ns_per_call(ticks, hz, 448);
            let ok_ns = (419_000_000..=421_000_000).contains(&ns);
            println!("SYSNS: 188137 ms / 448 calls -> {} ns per call (we expect 419-421 million)", ns);
            // The small values must not suffer from the change of the arithmetic.
            let small = crate::metrics::ns_per_call(hz / 1000, hz, 1); // exactly 1 ms
            let ok_small = (999_000..=1_001_000).contains(&small);
            println!("SYSNS: 1 ms / 1 call -> {} ns (we expect ~1 000 000)", small);
            let k3 = crate::metrics::sysrow_set("1,18,74");
            let kbad = crate::metrics::sysrow_set("x, ,999999");
            println!("SYSNS: talaria.sysrow=\"1,18,74\" -> accepted {}, \"x, ,999999\" -> accepted {}", k3, kbad);
            if ok_ns && ok_small && k3 == 3 && kbad == 0 { println!("SYSNS: OK"); }
            else { println!("SYSNS: FAIL"); }
            crate::suite::park()
        }
        // THE INSTRUMENT OF STEP 1 OF SESSION 43: TO WHOM THE CORE GOES FROM A THREAD WITH A REQUEST IN PROGRESS.
        // On the answer depends whether the policy "a request in progress
        // returns first" exists at all: if the neighbours are in a request too, the policy
        // degenerates into a long quantum, and that one has already been measured and is harmful (session 42,
        // phase T2). The gate holds the two things that there is nothing to check on the stand with:
        // the decision table (the state lives in three subsystems, and its combinations
        // cannot be enumerated otherwise) and the arithmetic of the counters together with the histogram
        // of the return.
        "reqwho" => {
            use crate::netprof as np;
            println!("talaria: the gate of the instrument \"to whom the core went from a thread with a request in progress\"");
            // 1. The decision table: (in a request, alive, a network waiter, ring0).
            let tbl = [
                ((true,  true,  false, false), np::SW_REQ),
                ((true,  true,  true,  true ), np::SW_REQ),     // "in a request" is the strongest of all
                ((false, true,  true,  false), np::SW_NETW),
                ((false, true,  false, true ), np::SW_KERN),
                ((false, true,  false, false), np::SW_R3),
                ((true,  false, false, false), np::SW_NOBODY),  // there is no such thread — there is nobody
            ];
            let mut ok_tbl = true;
            for ((q, a, w, k), want) in tbl {
                let got = np::who_of(q, a, w, k);
                if got != want { ok_tbl = false; println!("REQWHO: the decision table: ({} {} {} {}) -> {}, we expected {}", q, a, w, k, got, want); }
            }
            println!("REQWHO: the decision table {} ({} combinations)", if ok_tbl { "AGREES" } else { "DISAGREES" }, tbl.len());
            // 2. The counters. The instrument is silent until it is switched on — that is its property,
            // not an accident: the price of a switched-off one equals one read of an atomic.
            np::sched_probe(60, 61, 0);
            let (w0, _, _) = np::sched_counters();
            let quiet = w0.iter().sum::<u64>() == 0;
            println!("REQWHO: a switched-off instrument does not count: {}", if quiet { "yes" } else { "NO" });
            np::app_enable();
            // Two threads "in a request" on one core: t60 is taken off, the core goes to t61.
            np::app_probe_open(60, true);
            np::app_probe_open(61, true);
            np::sched_probe(60, 61, 0);
            // The core goes to a thread that is not in the table at all.
            np::sched_probe(61, crate::sched::NTASKS + 1, 0);
            // t60 returns: "the time until the return" closes.
            np::sched_probe(61, 60, 0);
            let (who, nq, rn) = np::sched_counters();
            let sum: u64 = who.iter().sum();
            // Two rotations of "also in a request" (t60 -> t61 and back) and one "nobody".
            let ok_cnt = who[np::SW_REQ] == 2 && who[np::SW_NOBODY] == 1 && sum == 3 && rn == 1;
            println!("REQWHO: rotations {} — also in a request {}, nobody {}, returns {}", sum, who[np::SW_REQ], who[np::SW_NOBODY], rn);
            println!("REQWHO: threads in a request on the core: 0:{} 1:{} 2:{} 3:{} 4+:{}", nq[0], nq[1], nq[2], nq[3], nq[4]);
            // Both threads are in a request on one core — so it is the bucket "2", and not a single
            // "0" (the bucket "the count disagreed") and not a single "1".
            let ok_nq = nq[2] >= 2 && nq[0] == 0 && nq[1] == 0;
            // Closing the stage returns the core's counter to zero: otherwise the instrument
            // would lie about "how many threads are in a request" already in the second minute.
            np::app_probe_open(60, false);
            np::app_probe_open(61, false);
            np::sched_probe(60, 61, 0);
            let (who2, nq2, rn2) = np::sched_counters();
            let ok_zero = who2.iter().sum::<u64>() == sum && nq2[2] == nq[2] && rn2 == rn;
            println!("REQWHO: a closed stage is not counted as a rotation: {}", if ok_zero { "yes" } else { "NO" });
            crate::netprof::dump_app();
            if ok_tbl && quiet && ok_cnt && ok_nq && ok_zero { println!("REQWHO: OK"); } else { println!("REQWHO: FAIL"); }
            crate::suite::park()
        }
        // THE POLICY OF STEP 2 OF SESSION 43: "A REQUEST IN PROGRESS DOES NOT GIVE THE CORE AWAY
        // TO A NETWORK WAITER" (`talaria.reqfirst=`). The gate holds exactly what makes the
        // policy dangerous: its SILENCE when the knob is off, its narrowness (a network
        // waiter — yes, an ordinary thread — no) and the ABSENCE OF STARVATION — the protection lives no
        // longer than the ceiling since the start of the request, and the expiry of the ceiling is counted.
        "reqfirst" => {
            println!("talaria: the gate of the policy \"a request in progress does not give the core away to a network waiter\"");
            let (me, waiter, plain) = (40usize, 41usize, 42usize);
            crate::netsys::nw_probe(waiter, true);
            crate::netsys::nw_probe(plain, false);
            // 1. The knob is off — there is no policy at all.
            crate::sched::REQFIRST_NS.store(0, core::sync::atomic::Ordering::Relaxed);
            crate::sched::req_probe(me, true);
            let off = crate::sched::reqfirst_probe(me, waiter);
            println!("REQFIRST: switched off (0) -> the protection {}", if off { "IS THERE (an error)" } else { "is silent" });
            // 2. The knob is on: a network waiter is shielded from, an ordinary thread is not.
            // The flag is armed BEFORE every probe: between the probes there is a print
            // into the console, and on the serial line it takes milliseconds — the protection's ceiling
            // (200 us) would have expired just from it, and the gate would be judging the print.
            crate::sched::REQFIRST_NS.store(200_000, core::sync::atomic::Ordering::Relaxed);
            crate::sched::req_probe(me, true);
            let vs_waiter = crate::sched::reqfirst_probe(me, waiter);
            crate::sched::req_probe(me, true);
            let vs_plain = crate::sched::reqfirst_probe(me, plain);
            println!("REQFIRST: a network waiter -> {}, an ordinary thread -> {}",
                if vs_waiter { "we keep the core" } else { "WE GIVE IT AWAY (an error)" },
                if vs_plain { "WE KEEP IT (an error)" } else { "we give it away" });
            // 3. There is no request in progress — there is no protection (the answer has gone, `mark_app_out`).
            crate::sched::req_probe(me, false);
            let no_req = crate::sched::reqfirst_probe(me, waiter);
            println!("REQFIRST: there is no request in progress -> the protection {}", if no_req { "IS THERE (an error)" } else { "is silent" });
            // 4. THERE IS NO STARVATION: the ceiling since the start of the request expires, and that is counted.
            let cap0 = crate::sched::REQFIRST_CAP.load(core::sync::atomic::Ordering::Relaxed);
            crate::sched::REQFIRST_NS.store(1_000, core::sync::atomic::Ordering::Relaxed); // 1 us
            crate::sched::req_probe(me, true);
            let hz = crate::tsc::calibrate_cached().max(1);
            let t0 = crate::tsc::rdtsc();
            while crate::tsc::rdtsc().wrapping_sub(t0) < hz / 1000 {} // 1 ms — ten times more than the ceiling
            let expired = crate::sched::reqfirst_probe(me, waiter);
            let cap1 = crate::sched::REQFIRST_CAP.load(core::sync::atomic::Ordering::Relaxed);
            println!("REQFIRST: the ceiling is 1 us, 1 ms has passed -> the protection {} (expiries {})",
                if expired { "IS THERE (STARVATION)" } else { "is lifted" }, cap1 - cap0);
            crate::sched::REQFIRST_NS.store(0, core::sync::atomic::Ordering::Relaxed);
            crate::sched::req_probe(me, false);
            crate::netsys::nw_probe(waiter, false);
            if !off && vs_waiter && !vs_plain && !no_req && !expired && cap1 > cap0 { println!("REQFIRST: OK"); }
            else { println!("REQFIRST: FAIL"); }
            crate::suite::park()
        }
        "obs" => {
            println!("talaria: the self-test of observability (the formatting, the walk over the AS, the PMU's arithmetic, the commands)");
            let ok = statics();
            commands();
            println!("obs: {}", if ok { "ALL OK" } else { "THERE ARE FAILURES" });
            crate::suite::park()
        }
        "core" => {
            // THE WORKLOAD'S CORE DUMP. A process from the disk (not a kernel thread): a fatal
            // SIGSEGV in ring 3 goes along the path "kill the process", and it is exactly there
            // that the dump is written. It is the HOST that checks it — it takes the file out of the image
            // and opens it with a real gdb: the format is either understandable to the debugger or it is not,
            // and there is no middle ground here.
            println!("talaria: the gate of the workload's core dump (crashme from the disk, a fatal #PF in ring 3)");
            if !crate::coredump::enabled() {
                println!("core-test: FAIL — talaria.core=<path> was not given");
                crate::suite::park()
            }
            if crate::fs::init() {
                // The name of the started file is the same as the one the production path sets from
                // the manifest: `Core was generated by` is taken from it.
                crate::fs::set_exe(b"/crashme\0".as_ptr());
                unsafe { crate::user::run_process_disk(b"/crashme", &[b"crashme"], &[b"HOME=/"]); }
            }
            println!("core-test: FAIL — /crashme could not be started from the disk");
            crate::suite::park()
        }
        "corekernel" => {
            // THE KERNEL'S CORE DUMP. The fault is provoked deliberately: a write to a knowingly
            // unmapped address in ring 0. The path is the same as at a real
            // crash — so the dump is the same too.
            println!("talaria: the gate of the KERNEL's core dump (a deliberate #PF in ring 0)");
            if !crate::coredump::enabled() {
                println!("core-test: FAIL — talaria.core=<path> was not given");
                crate::suite::park()
            }
            if !crate::fs::init() {
                println!("core-test: FAIL — the FS is not mounted, there is nowhere to write the dump");
                crate::suite::park()
            }
            // The call goes through several frames: that way the dump will hold NOT ONE address but
            // a chain, and the gate will be able to check that the stack unwound.
            core_fault_l3();
            println!("core-test: FAIL — a #PF was expected, and there was none");
            crate::suite::park()
        }
        "profdeep" => {
            // THE GATE OF THE UNWINDING ON A REAL WORKLOAD. A synthetic chain of
            // frames (`unwind::selftest`) checks the unwinder, but it does not
            // check the main thing: that the RBP is taken from the RIGHT place of the interrupt's
            // frame and that a ring 3 chain is read from a foreign address
            // space. A mistake in the register's offset would give plausible
            // garbage, which is not visible on the synthetic one at all.
            crate::sched::prof_set_hz(0);   // a sample on every tick: the workload is short
            crate::sched::PROF_ON.store(true, core::sync::atomic::Ordering::SeqCst);
            DEEP.store(true, core::sync::atomic::Ordering::SeqCst);
            LIVE.store(true, core::sync::atomic::Ordering::SeqCst);
            static PD: &[u8] = user_elf!("profdeep");
            println!("talaria: the gate of the unwinding of the stack on the workload profdeep ({} bytes, a chain of five levels)", PD.len());
            unsafe { crate::user::run_threaded(PD); }
        }
        "obslive" => {
            // We switch the profile on BEFORE the workload: it is collected by the scheduler's tick,
            // and the tick arrives only while the workload lives.
            crate::sched::PROF_ON.store(true, core::sync::atomic::Ordering::SeqCst);
            LIVE.store(true, core::sync::atomic::Ordering::SeqCst);
            static PT: &[u8] = user_elf!("pthreads");
            println!("talaria: observability on a live workload ({} bytes, clone+futex+mutex+join)", PT.len());
            unsafe { crate::user::run_threaded(PT); }
        }
        _ => return false,
    }
    true
}
