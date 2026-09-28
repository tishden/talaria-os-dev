//! `perf` — hardware counters and a sampling profile.
//!
//! ## Why not `perf_event_open`
//!
//! There is no perf subsystem here and there will not be one: it drags in event
//! file descriptors, ring buffers with `mmap`, ioctls and nested group
//! accounting — exactly the machinery Talaria gets rid of. Everything perf
//! ultimately PROVIDES is available more cheaply:
//!
//! | need | on Linux | here |
//! |---|---|---|
//! | core counters | `perf stat` | `perf` (rdpmc, readable from ring 3 without a syscall) |
//! | profile by address | `perf record` | `perf on` + `perf record` (the scheduler tick) |
//! | symbols | `perf report` | on the HOST: `kernel/tools/talaria-prof` -> `addr2line` |
//! | flamegraph | `perf script \| stackcollapse` | the same `kernel/tools/talaria-prof` |
//!
//! ## What we honestly CANNOT do
//!
//! * **Stacks without frame pointers.** A stack is unwound only along the chain
//!   of frame pointers: DWARF unwinding is not done in the kernel. A workload
//!   built without `-fno-omit-frame-pointer` gives depth 1 — an honest refusal,
//!   not an invented stack.
//! * **PEBS and exact attribution of an event to an instruction.** We count with
//!   counters, not event samples: "this `mov` missed L3" cannot be asked here.
//! * **Automatic peeking into a dedicated core.** See `perf poke`.
//!
//! ## Sample rate
//!
//! The profile is taken by the SCHEDULER TICK, so its rate is the tick rate:
//! with an 8 ms slice that is 125 samples per second. For a denser profile,
//! shorten the slice for the measurement (`talaria.slice=`) and do not forget to
//! restore it: a short slice by itself changes what you measure.
use super::{dur, pct};
use core::sync::atomic::Ordering::{Relaxed, SeqCst};

const NC: usize = crate::percpu::MAXCPU;

/// Per-core counter table from the latest snapshots.
fn counters() {
    if !crate::pmu::available() {
        crate::println!("perf: PMU unavailable (arch-perfmon disabled). Under QEMU this needs `-cpu host` and");
        crate::println!("perf: a vPMU enabled in the hypervisor; on EC2 metal the counters are always there.");
        return;
    }
    crate::pmu::refresh_all(); // including cores WITHOUT a tick — that is where the workload runs
    crate::println!("perf: counters accumulated SINCE BOOT; stale snapshots taken now (IPI).");
    crate::println!("  {:<5} {:<6} {:>14} {:>14} {:>6}  {:<28} {:<28}",
        "core", "role", "cycles", "instructions", "IPC", "LLC misses", "branch misses");
    let mut seen = 0;
    for c in 0..NC {
        let (s, seq) = crate::pmu::last(c);
        if seq == 0 { continue; }
        seen += 1;
        crate::println!("  {:<5} {:<6} {:>14} {:>14} {:>3}.{:02}  {:<28} {:<28}",
            c, super::role(c), s.cycles, s.instrs, s.ipc100() / 100, s.ipc100() % 100,
            super::miss_llc(&s), super::miss_br(&s));
    }
    if seen == 0 { crate::println!("  (no snapshots)"); }
    // Cores without a tick are silent not because they broke: there is NO tick there, and that is normal.
    let mut quiet = false;
    for c in 0..NC {
        if crate::affinity::role(c) != 0 && crate::pmu::last(c).1 == 0 {
            if !quiet { crate::print!("perf: no snapshot (dedicated or idle core):"); quiet = true; }
            crate::print!(" {}", c);
        }
    }
    if quiet { crate::println!(" — take one explicitly: `perf poke <core>`"); }
}

/// Take the counters of the given core through an IPI. Interrupts the workload — on explicit request.
fn poke(arg: &str) {
    let Some(c) = arg.trim().parse::<usize>().ok() else {
        crate::println!("perf poke: a core number is needed, e.g. `perf poke 3`");
        return;
    };
    if c == crate::percpu::cpu_id() {
        crate::println!("perf poke: core {} is the one running the console: counters are read right here, without an IPI", c);
    } else {
        crate::println!("perf poke: core {} will be INTERRUPTED by one IPI for a counter snapshot", c);
    }
    if crate::pmu::poke(c) {
        let (s, _) = crate::pmu::last(c);
        crate::println!("perf poke: core {}: cycles {}, instructions {}, IPC {}.{:02}, LLC misses {}, branch misses {}",
            c, s.cycles, s.instrs, s.ipc100() / 100, s.ipc100() % 100,
            super::miss_llc(&s), super::miss_br(&s));
    } else {
        crate::println!("perf poke: core {} did not answer within 1 ms (offline, IF=0 or PMU unavailable)", c);
    }
}

/// Top addresses from the sample ring — kernel and workload separately.
///
/// TWO LISTS, not one. "Where the time goes" and "who led there" are different
/// questions: the LEAF shows the hot function (`memcpy`), the STACK shows who
/// calls it, and it is usually called from several places, each fixed its own way.
fn top_addrs(limit: usize) {
    // As in `record`: the summary is computed over a ring that keeps filling
    // meanwhile — no samples are taken while counting and printing.
    let on = crate::sched::PROF_ON.swap(false, SeqCst);
    top_addrs_frozen(limit);
    crate::sched::PROF_ON.store(on, SeqCst);
}
fn top_addrs_frozen(limit: usize) {
    let (have, total) = crate::sched::prof_ring_stats();
    if total == 0 {
        crate::println!("perf: profile off. Enable: `perf on` (or `talaria.prof=1` at boot)");
        return;
    }
    crate::println!("perf: samples in the ring {} of {} total{}, rate {} Hz, stack depth up to {}",
        have, total, if total as usize > crate::sched::PROFR_N { " (the ring wrapped — old samples lost)" } else { "" },
        crate::sched::prof_hz(), crate::sched::PROFR_D);
    let uf = crate::obs::unwind::faults();
    if uf != 0 {
        crate::println!("perf: {} reads failed while unwinding — the frame chain is garbage in places (workload without -fno-omit-frame-pointer?)", uf);
    }
    const NB: usize = 24;
    let mut stk = [0u64; crate::sched::PROFR_D];
    for ring in [3u8, 0u8] {
        let (mut key, mut hit) = ([0u64; NB], [0u64; NB]);
        let (mut n, mut tot, mut depth_sum, mut deep) = (0usize, 0u64, 0u64, 0u64);
        for i in 0..have {
            let (d, _tid, _cpu, r) = crate::sched::prof_ring_get(i, &mut stk);
            if r != ring || d == 0 { continue; }
            tot += 1;
            depth_sum += d as u64;
            if d > 1 { deep += 1; }
            // Fold by LEAF with 16-byte precision: otherwise one line of code
            // smears over neighbouring instructions.
            let k = stk[0] & !0xF;
            let mut found = false;
            for j in 0..n { if key[j] == k { hit[j] += 1; found = true; break; } }
            if !found && n < NB { key[n] = k; hit[n] = 1; n += 1; }
        }
        if tot == 0 { continue; }
        crate::println!("perf: {} — {} samples, average depth {}.{}, with a stack {} of {}:",
            if ring == 3 { "WORKLOAD (ring 3)" } else { "KERNEL (ring 0)" },
            tot, depth_sum / tot, (depth_sum * 10 / tot) % 10, deep, tot);
        let mut shown = [false; NB];
        for _ in 0..limit.min(n) {
            let (mut b, mut bv) = (usize::MAX, 0u64);
            for j in 0..n { if !shown[j] && hit[j] > bv { bv = hit[j]; b = j; } }
            if b == usize::MAX { break; }
            shown[b] = true;
            crate::println!("perf:   {:#018x}  {:>6} ({})", key[b], hit[b], pct(hit[b] * 10_000 / tot));
        }
        crate::println!("perf:   symbols: addr2line -f -e {} <address>",
            if ring == 3 { "<workload binary>" } else { "kernel/target/x86_64-talaria/release/talaria" });
    }
    top_stacks(5);
}

/// The most frequent whole STACKS. A stack is compared by its first three frames:
/// beyond that almost all diverge, and the list would become a list of samples.
fn top_stacks(limit: usize) {
    let (have, _) = crate::sched::prof_ring_stats();
    if have == 0 { return; }
    const NB: usize = 16;
    const K: usize = 3;
    let mut key = [[0u64; K]; NB];
    let mut hit = [0u64; NB];
    let mut ringof = [0u8; NB];
    let (mut n, mut tot) = (0usize, 0u64);
    let mut stk = [0u64; crate::sched::PROFR_D];
    for i in 0..have {
        let (d, _tid, _cpu, r) = crate::sched::prof_ring_get(i, &mut stk);
        if d < 2 { continue; }        // nothing to compare without a stack — it is in the leaf list
        tot += 1;
        let mut k = [0u64; K];
        for j in 0..K { k[j] = if j < d { stk[j] & !0xF } else { 0 }; }
        let mut found = false;
        for j in 0..n { if key[j] == k { hit[j] += 1; found = true; break; } }
        if !found && n < NB { key[n] = k; hit[n] = 1; ringof[n] = r; n += 1; }
    }
    if tot == 0 {
        crate::println!("perf: no stacks — not a single sample unwinds deeper than the leaf");
        crate::println!("perf:   build the workload with -fno-omit-frame-pointer, otherwise there is no frame chain");
        return;
    }
    crate::println!("perf: most frequent stacks (leaf -> callers, {} samples with a stack):", tot);
    let mut shown = [false; NB];
    for _ in 0..limit.min(n) {
        let (mut b, mut bv) = (usize::MAX, 0u64);
        for j in 0..n { if !shown[j] && hit[j] > bv { bv = hit[j]; b = j; } }
        if b == usize::MAX { break; }
        shown[b] = true;
        crate::print!("perf:   {:>6} ({}) [{}]", hit[b], pct(hit[b] * 10_000 / tot),
            if ringof[b] == 3 { "ring3" } else { "ring0" });
        for j in 0..K { if key[b][j] != 0 { crate::print!(" {:#x}", key[b][j]); } }
        crate::println!("");
    }
}

/// Dump the sample ring in machine form: one line per sample with its STACK.
/// Parsed by `kernel/tools/talaria-prof` (symbols + flamegraph).
fn record(arg: &str) {
    // A SNAPSHOT MUST BE A SNAPSHOT. The ring keeps filling while the dump travels
    // over serial: two thousand lines at 115200 baud take about twenty seconds, and
    // samples arrive at 400 Hz per core. The slot index is counted from the current
    // head of the ring, so what was printed was not "the last N samples" but what
    // lay in the slots AT PRINT TIME: on `examples/mdpipe` the ring-3 share shrank
    // from 25 % to 0 %, because by the end of the dump the workload had finished and
    // the ring was full of waiting. No samples are taken while printing.
    let on = crate::sched::PROF_ON.swap(false, SeqCst);
    let (have, total) = crate::sched::prof_ring_stats();
    if have == 0 {
        crate::sched::PROF_ON.store(on, SeqCst);
        crate::println!("perf record: no samples — run `perf on` first"); return;
    }
    // `perf record <N>` — only the last N samples. Two thousand lines over serial at
    // 115200 baud take about twenty seconds, and usually the tail is what is needed
    // ("it is slow right now"), not the whole buffer.
    let want = arg.trim().parse::<usize>().unwrap_or(have).min(have).max(1);
    let from = have - want;
    crate::println!("#talaria-prof v2 samples={} total={} tsc_hz={} slice_ns={} hz={} depth={}",
        want, total, crate::tsc::calibrate_cached(), crate::smp::effective_slice_ns(),
        crate::sched::prof_hz(), crate::sched::PROFR_D);
    crate::println!("#format: SAMPLE <ring> <tid> <cpu> <thread-name> <address> [<return-address> ...]");
    let mut stk = [0u64; crate::sched::PROFR_D];
    for i in from..have {
        let (d, tid, cpu, ring) = crate::sched::prof_ring_get(i, &mut stk);
        if d == 0 { continue; }
        crate::print!("SAMPLE {} {} {} {}", ring, tid, cpu, super::comm(tid));
        for k in 0..d { crate::print!(" {:#x}", stk[k]); }
        crate::println!("");
    }
    crate::println!("#talaria-prof end");
    crate::sched::PROF_ON.store(on, SeqCst); // the profile continues on the same ring
}

/// An "at a glance" summary: where the bottleneck is.
fn verdict() {
    if !crate::pmu::available() { return; }
    let (mut cyc, mut ins, mut lm, mut lr, mut bm, mut br) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    for c in 0..NC {
        let (s, seq) = crate::pmu::last(c);
        if seq == 0 { continue; }
        cyc += s.cycles; ins += s.instrs; lm += s.llc_miss; lr += s.llc_ref; bm += s.br_miss; br += s.br_ret;
    }
    if cyc == 0 { return; }
    let ipc = ins * 100 / cyc;
    let llc = if lr > 0 { lm * 10_000 / lr } else { 0 };
    let brm = if br > 0 { bm * 10_000 / br } else { 0 };
    crate::println!("");
    let d = crate::pmu::Sample { cycles: cyc, instrs: ins, llc_miss: lm, llc_ref: lr, br_miss: bm, br_ret: br, ..crate::pmu::Sample::ZERO };
    crate::println!("perf: SUMMARY  IPC {}.{:02}, LLC misses {}, branch misses {}",
        ipc / 100, ipc % 100, super::miss_llc(&d), super::miss_br(&d));
    // Thresholds are not truth but reasons to look. Each is named together with
    // what to do about it: a number without an action does not help the operator.
    if ipc < 100 { crate::println!("perf:   IPC below 1.0 — the CPU is WAITING. Look at the cache misses below and `kstat paging`."); }
    // Thresholds are by MPKI: a miss ratio needs a denominator, and a machine
    // with four counters does not have one. Ten LLC misses per thousand
    // instructions is a trip to memory about every hundred instructions, and
    // there is nothing further to look for: memory-bound, not CPU-bound.
    let llc_mpki = d.llc_mpki100();
    let br_mpki = d.br_mpki100();
    if llc_mpki > 1000 || (d.has_llc_ratio() && llc > 500) {
        crate::println!("perf:   many LLC misses ({}) — the working set does not fit in the cache.", super::miss_llc(&d));
        if crate::cat::present() { crate::println!("perf:   CAT is present: try giving the workload more L3 ways (`llc=` in the manifest, see `cores`)."); }
        else { crate::println!("perf:   no CAT on this machine — there is no way to isolate the cache; look at the working set size."); }
    }
    if br_mpki > 500 || (d.has_br_ratio() && brm > 300) {
        crate::println!("perf:   frequent branch mispredictions ({}) — data-dependent branching in a hot loop.", super::miss_br(&d));
    }
    if ipc >= 200 && llc_mpki <= 200 { crate::println!("perf:   counters are healthy — look for the bottleneck in wait time (`kstat sync`, `top`), not in the \
        code."); }
}

pub fn cmd(arg: &str) {
    let (head, rest) = match arg.trim().split_once(' ') { Some((h, r)) => (h, r.trim()), None => (arg.trim(), "") };
    match head {
        "" | "stat" => { counters(); verdict(); }
        "poke" => poke(rest),
        "on" => {
            // The sample rate is given IN HERTZ, not as a tick divider: the operator
            // thinks of "two hundred samples a second", and how many ticks that is
            // depends on the slice, which may be anything on this machine.
            let hz = rest.parse::<u64>().unwrap_or(200);
            let got = crate::sched::prof_set_hz(hz);
            crate::sched::prof_ring_reset();
            crate::sched::PROF_ON.store(true, SeqCst);
            crate::println!("perf: profile ON — {} samples/s (requested {}, slice {}), stack up to {} frames",
                got, hz, dur(crate::smp::effective_slice_ns()), crate::sched::PROFR_D);
            crate::println!("perf: the ring holds {} samples — that is {} of recording. View: `perf top`, dump: `perf record`",
                crate::sched::PROFR_N, dur(crate::sched::PROFR_N as u64 * 1_000_000_000 / got.max(1)));
            crate::println!("perf: a stack is visible only for code WITH FRAME POINTERS (-fno-omit-frame-pointer);");
            crate::println!("perf: without them the depth is 1 — an honest refusal, not an invented stack.");
        }
        "off" => { crate::sched::PROF_ON.store(false, SeqCst); crate::println!("perf: profile off (the ring is kept)"); }
        "reset" => { crate::sched::prof_ring_reset(); crate::println!("perf: samples forgotten"); }
        "top" => top_addrs(10),
        "record" => record(rest),
        "code" => crate::metrics::top_code(),
        "gdb" => gdb_help(),
        _ => {
            crate::println!("perf: subcommands:");
            crate::println!("  perf            hardware counters per core + summary (IPC, cache, branch predictor)");
            crate::println!("  perf poke <N>   take the counters of DEDICATED core N (interrupts it with one IPI)");
            crate::println!("  perf on [hz]    sampling profile with stacks (default 200 samples/s)");
            crate::println!("  perf off        turn the profile off (the ring stays)");
            crate::println!("  perf top        top leaves and most frequent stacks (kernel and workload separately)");
            crate::println!("  perf record [N] dump samples line by line, the last N (parse with kernel/tools/talaria-prof)");
            crate::println!("  perf code       histogram by code page (short summary)");
            crate::println!("  perf reset      forget the accumulated samples");
            crate::println!("  perf gdb        how to attach GDB to this machine");
        }
    }
}

/// Debugger attach instructions: printed where people will look for them.
fn gdb_help() {
    crate::println!("perf gdb: the gdb stub speaks the GDB Remote Serial Protocol on COM2 (0x2F8).");
    crate::println!("  1. QEMU: `-M pc` and `-serial tcp::1234,server,nowait` as the SECOND serial port");
    crate::println!("     (the disk on -M pc is `-device nvme`, virtio there is mmio only).");
    crate::println!("  2. Enter the debugger: `gdb break` in the console (live machine), `talaria.gdb=break` (before the");
    crate::println!("     first workload instruction) or boot with `talaria.mode=gdb`.");
    crate::println!("  3. On the host: gdb <workload binary>");
    crate::println!("               (gdb) target remote :1234");
    crate::println!("               (gdb) add-symbol-file kernel/target/x86_64-talaria/release/talaria -o 0");
    crate::println!("  Workload code is R-X (W^X): breakpoints on it are HARDWARE ones, `hbreak <function>` (up to three).");
    crate::println!("  Supported: registers (read and write), memory, step, continue, detach,");
    crate::println!("  software breakpoints (Z0, kernel text), hbreak (Z1) and watchpoints (Z2/Z3/Z4 via DR0-DR7).");
    crate::println!("  Threads: `info threads`, `thread N`. On SMP — all-stop: the other cores are parked.");
    crate::println!("  On bare metal COM2 is a physical port: connect through it.");
    crate::println!("  IMPORTANT: stopping in the debugger stops THE WHOLE KERNEL, including the datapath.");
}

/// One-shot summary for the regression and `status`: one line on the observability state.
pub fn oneline() {
    crate::println!("perf: PMU {}, profile {}, samples {}, per-core snapshots {:#x}",
        if crate::pmu::available() { "present" } else { "absent" },
        if crate::sched::PROF_ON.load(Relaxed) { "on" } else { "off" },
        crate::sched::prof_ring_stats().1, crate::pmu::armed_mask());
}
