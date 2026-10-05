//! `top` and `ps`: who eats the CPU, memory and syscalls.
//!
//! ## Why %CPU is counted in cycles rather than ticks
//!
//! Talaria's main thread usually lives on a DEDICATED core, where the scheduler's timer
//! is not armed at all — that is the point of a dedicated core. The tick counter there is
//! always zero, and a tick-based `top` would show `0%` for exactly the thread everything
//! is done for. So the time comes from `sched::run_ns` (TSC cycles,
//! the accumulated ones + the current stretch), and a dedicated core is as visible as
//! a shared one.
//!
//! ## What is expensive and when it is paid for
//!
//! * RSS — a walk of the process's page tables. It is computed ONCE PER PROCESS per update
//!   and only if the operator has not switched it off (`top -m`). A process's threads are shown
//!   its RSS: threads share an AS, and "a thread's RSS" is a meaningless quantity.
//! * the per-core PMU counters are taken from the snapshots made by the tick; a dedicated core
//!   has no tick, and there an explicit `perf poke` is needed (see `obs::perf`).
use super::{brate, bytes, comm, dur, pct, rate, state};
use core::fmt::Write;

const NT: usize = crate::sched::NTASKS;
const NC: usize = crate::percpu::MAXCPU;

/// The previous snapshot of the counters: without it there is NOT A SINGLE derived quantity
/// (%CPU, allocations per second, syscalls per second) — and those are exactly what is needed.
struct Prev {
    t_ns: u64,
    run: [u64; NT],
    /// "Ready and not running" (`sched::rq_ns`): the share of the window a thread SPENT standing in
    /// the queue for a CPU. A low CPU% with a high WAITED% means the thread is there and a core is not.
    rq: [u64; NT],
    sys: [u64; NT],
    frm: [u64; NT],
    sw: [u64; NT],
    pf: [u64; NT],
    cpu_busy: [u64; NC],
    /// The previous PMU snapshots per core + their numbers: without the difference `top` would show
    /// the IPC accumulated SINCE BOOT — a quantity that on a live machine does not
    /// change at all and is therefore useless.
    pmu: [crate::pmu::Sample; NC],
    pmu_seq: [u64; NC],
    valid: bool,
}
static mut PREV: Prev = Prev {
    t_ns: 0, run: [0; NT], rq: [0; NT], sys: [0; NT], frm: [0; NT], sw: [0; NT], pf: [0; NT], cpu_busy: [0; NC],
    pmu: [crate::pmu::Sample::ZERO; NC], pmu_seq: [0; NC], valid: false,
};
/// The number of a task's last syscall (-1 — it never called one).
///
/// Without it an unnamed thread eating a core cannot be identified: it has no name
/// (the runtime does not give one to everybody), its syscalls per second are zero, and one is left guessing.
/// The number answers with a single number — "it sits in futex", "it sits in epoll_wait",
/// "it spins in user space" (the number is old while there are no calls).
fn last_sys(t: usize) -> i64 {
    // The word packs THE NUMBER (the high 16 bits) and the return rip — we take the number.
    let v = crate::syscall::LAST_SYS[t.min(crate::sched::NTASKS - 1)].load(core::sync::atomic::Ordering::Relaxed);
    if v == u64::MAX { -1 } else { (v >> 48) as i64 }
}

fn prev() -> &'static mut Prev { unsafe { &mut *core::ptr::addr_of_mut!(PREV) } }

/// The window (ns) since the previous snapshot. The FIRST call has no window — and then the window
/// is the whole uptime: "how much this thread ate the CPU since boot" is an honest
/// answer, while dividing by an arbitrary unit would give 100% for everybody.
fn take() -> (u64, bool) {
    let p = prev();
    let now = crate::tsc::mono_ns();
    if p.valid { (now.saturating_sub(p.t_ns).max(1), false) } else { (now.max(1), true) }
}
fn commit() {
    let p = prev();
    p.t_ns = crate::tsc::mono_ns();
    let n = crate::sched::task_count().min(NT);
    for t in 0..n {
        p.run[t] = crate::sched::run_ns(t);
        p.rq[t] = crate::sched::rq_ns(t);
        p.sys[t] = crate::metrics::syscalls_of(t);
        p.frm[t] = crate::metrics::frames_of(t);
        p.sw[t] = crate::sched::switches(t);
        p.pf[t] = crate::metrics::page_faults_of(t);
    }
    for c in 0..NC {
        p.cpu_busy[c] = crate::sched::cpu_busy_ns(c);
        let (s, seq) = crate::pmu::last(c);
        p.pmu[c] = s; p.pmu_seq[c] = seq;
    }
    p.valid = true;
}

/// The share in hundredths of a percent: `d` nanoseconds over a window of `win` nanoseconds.
fn share100(d: u64, win: u64) -> u64 { if win == 0 { 0 } else { (d.saturating_mul(10_000) / win).min(10_000) } }

// ── the processes' RSS ───────────────────────────────────────────────────────
/// The process's memory footprint computed on this update: (resident,
/// shared, lazily reserved) in pages. Computed once per process.
struct Rss { leader: [u8; 64], res: [u64; 64], shr: [u64; 64], lazy: [u64; 64], n: usize }
static mut RSS: Rss = Rss { leader: [0; 64], res: [0; 64], shr: [0; 64], lazy: [0; 64], n: 0 };
fn rss() -> &'static mut Rss { unsafe { &mut *core::ptr::addr_of_mut!(RSS) } }

fn rss_scan(on: bool) {
    let r = rss();
    r.n = 0;
    if !on { return; }
    let n = crate::sched::task_count().min(NT);
    for t in 0..n {
        if !crate::sched::task_alive(t) { continue; }
        let p = crate::sched::proc_of(t);
        if p != t { continue; } // we count by THE LEADER: threads share an address space
        if r.n >= 64 { break; }
        let cr3 = crate::sched::task_cr3(t);
        let pml4 = if cr3 != 0 { cr3 & !0xFFF } else { crate::paging::current_cr3() & !0xFFF };
        let (res, _huge, shr, lazy) = crate::paging::as_footprint(pml4);
        r.leader[r.n] = t as u8; r.res[r.n] = res; r.shr[r.n] = shr; r.lazy[r.n] = lazy;
        r.n += 1;
    }
}
fn rss_of(leader: usize) -> (u64, u64, u64) {
    let r = rss();
    for i in 0..r.n { if r.leader[i] as usize == leader { return (r.res[i], r.shr[i], r.lazy[i]); } }
    (0, 0, 0)
}

// ── the header ───────────────────────────────────────────────────────────────
fn header(win: u64) {
    let p = prev();
    let hz = crate::tsc::calibrate_cached().max(1);
    let up = crate::tsc::mono_ns();
    let n = crate::sched::task_count().min(NT);
    let (mut run, mut ready, mut sleep, mut dead) = (0, 0, 0, 0);
    for t in 0..n {
        match crate::sched::state_of(t) { 0 => dead += 1, 2 => run += 1, 4 => sleep += 1, _ => ready += 1 }
    }
    let mut procs = 0;
    for t in 0..n { if crate::sched::task_alive(t) && crate::sched::proc_of(t) == t { procs += 1; } }
    crate::println!("Talaria  uptime {}  TSC {} MHz  slice {}  tasks {} (running {}, ready {}, waiting {}, dead {}), processes {}",
        dur(up), hz / 1_000_000, dur(crate::smp::effective_slice_ns()),
        n, run, ready, sleep, dead, procs);

    // THE CORES: each one's occupancy. A core without work and a core busy waiting are different
    // illnesses, so next to the percentage stands the core's role and whether its timer is armed.
    let online = crate::affinity::online_count().max(1);
    let mut busy_sum = 0u64;
    crate::print!("CPU  ");
    for c in 0..NC {
        if crate::affinity::role(c) == 0 && c != 0 { continue; }
        let b = crate::sched::cpu_busy_ns(c);
        let d = b.saturating_sub(p.cpu_busy[c]);
        busy_sum += d;
        crate::print!(" [{}]{}", c, pct(share100(d, win)));
    }
    crate::println!("");
    crate::println!("     {} in total over {} cores ({} shared, {} dedicated); migrations {}, steals {}",
        pct(share100(busy_sum, win.saturating_mul(online as u64))), online,
        (0..NC).filter(|&c| crate::affinity::role(c) == 1).count(),
        (0..NC).filter(|&c| crate::affinity::role(c) == 2).count(),
        crate::affinity::MIGRATIONS.load(core::sync::atomic::Ordering::Relaxed), crate::sched::STOLEN.load(core::sync::atomic::Ordering::Relaxed));

    // MEMORY: how much there is, how much is free and how fast it is going.
    let total = crate::mem::ram_total_frames();
    let free = crate::mem::free_frames();
    let recl = crate::mem::free_list_pages();
    let mut dfrm = 0u64;
    for t in 0..n { dfrm += crate::metrics::frames_of(t).saturating_sub(p.frm[t]); }
    crate::println!("MEMORY {} in total, free {} (bump {} + returned {}), the kernel {}; handed out {}/s",
        bytes(total * 4096), bytes((free + recl) * 4096), bytes(free * 4096), bytes(recl * 4096),
        bytes(crate::mem::kernel_frames() * 4096), brate(dfrm * 4096, win));

    // SYSCALLS AND SWITCHES — the total rates: "the system as a whole is twitching".
    let mut dsys = 0u64; let mut dsw = 0u64;
    for t in 0..n {
        dsys += crate::metrics::syscalls_of(t).saturating_sub(p.sys[t]);
        dsw += crate::sched::switches(t).saturating_sub(p.sw[t]);
    }
    crate::println!("FLOW   syscalls {}/s, switches {}/s, faults in total {} (COW {}), steals {}",
        rate(dsys, win), rate(dsw, win), crate::metrics::page_faults(), crate::metrics::cow_faults(),
        crate::sched::STOLEN.load(core::sync::atomic::Ordering::Relaxed));

    pmu_line();
    fs_line();
    net_line();
}

/// The hardware counters' line: IPC, cache and branch-predictor misses OVER THE INTERVAL.
pub fn pmu_line() {
    if !crate::pmu::available() {
        crate::println!("PMU    is unavailable (no vPMU from the hypervisor) — IPC and misses are not counted");
        return;
    }
    // Our own core can be taken right here and for free: `rdpmc` reads the counters
    // of the core it runs on, and the console runs on the BSP.
    crate::pmu::note_here();
    let p = prev();
    let mut d = crate::pmu::Sample::ZERO;
    let mut seen = 0;
    let mut fresh = 0;
    for c in 0..NC {
        let (s, seq) = crate::pmu::last(c);
        if seq == 0 { continue; }
        seen += 1;
        // The difference makes sense only if a core TOOK a new snapshot: otherwise it is
        // the difference of a value with itself, i.e. zero, and it would blur the total.
        if p.valid && p.pmu_seq[c] != 0 && seq != p.pmu_seq[c] {
            let x = s.delta(&p.pmu[c]);
            // A difference no core can produce is not added: more cycles than 16 per tick of
            // the counter (no core runs 16x its timer), or an IPC above 8. One such core used
            // to wrap the sum into an IPC of tens of millions; now it is named with its numbers.
            if !x.plausible() {
                crate::println!("PMU    core {}: an impossible difference (cycles {} instrs {} over {} counter ticks) — left out",
                    c, x.cycles, x.instrs, x.tsc);
                continue;
            }
            d.cycles += x.cycles; d.instrs += x.instrs;
            d.llc_miss += x.llc_miss; d.llc_ref += x.llc_ref;
            d.br_miss += x.br_miss; d.br_ret += x.br_ret;
            fresh += 1;
        }
    }
    if seen == 0 {
        crate::println!("PMU    there are no snapshots yet (no tick has come) — `perf poke <core>` takes one by force");
        return;
    }
    // The cores WITHOUT a snapshot are listed by name. There are exactly two reasons, and both are legitimate:
    // the core is dedicated (there is no tick there by construction) or the core is idle (a tick
    // comes when there is someone to preempt). Both are cured by the same `poke`.
    let mut quiet = super::S::<64>::new();
    for c in 0..NC {
        if crate::affinity::role(c) != 0 && crate::pmu::last(c).1 == 0 { let _ = write!(quiet, " {}", c); }
    }
    if fresh == 0 {
        crate::println!("PMU    over the interval no core took the counters (there was no tick); snapshots in total from {} cores", seen);
    } else {
        crate::println!("PMU    over the interval, over {} cores: IPC {}.{:02}, LLC misses {}, branch misses {}",
            fresh, d.ipc100() / 100, d.ipc100() % 100, super::miss_llc(&d), super::miss_br(&d));
    }
    if !quiet.as_str().is_empty() {
        crate::println!("PMU    without a snapshot:{} — the core is dedicated or idle; to take one by force: `perf poke <core>`", quiet);
    }
}

fn fs_line() {
    match crate::shell::backend() {
        crate::shell::Backend::Ext2 => {
            let (bs, t, f, _, _) = crate::ext2::statfs();
            let (bs, t, f) = (bs as u64, t as u64, f as u64);
            let (h, m) = crate::ext2::cache_stats();
            crate::println!("FS     ext2 RW {} in total, free {}, the block cache {} MB (bypassed {}), the journal is {}",
                bytes(t * bs), bytes(f * bs), crate::ext2::cache_mb(),
                pct(if h + m > 0 { m * 10_000 / (h + m) } else { 0 }),
                if crate::ext2::jrn_active() { "on" } else { "off" });
        }
        crate::shell::Backend::Tfs => crate::println!("FS     TFS (read-only), files {}", crate::fs::nfiles()),
        crate::shell::Backend::None => crate::println!("FS     is not mounted"),
    }
}

fn net_line() {
    if !crate::netsys::ready() { crate::println!("NET    the stack is not up"); return; }
    let (rx, tx, live) = crate::netsys::totals();
    let (l, u) = crate::netsys::listen_udp_count();
    crate::println!("NET    interfaces {}, connections {}, listeners {} (+{} UDP), rx {} tx {}",
        crate::netsys::nifs(), live, l, u, bytes(rx), bytes(tx));
}

// ── the table ────────────────────────────────────────────────────────────────
fn table_head(threads: bool) {
    if threads {
        crate::print!("\n  PID  TID NAME            CPU CPU% WAIT%    RSS  SHR LAZY ALLOC/s SWITCH/s SYSC/s");
    } else {
        crate::print!("\n  PID THRDS NAME            CPU CPU% WAIT%    RSS  SHR LAZY ALLOC/s SWITCH/s SYSC/s");
    }
    // THE LAST SYSCALL — next to the state. Without it an unnamed thread
    // eating a core cannot be identified at all: it has no name (the runtime gives one
    // not to everybody), its syscalls per second are zero, and one is left guessing. The number of the last
    // call answers that with one number — "it sits in futex", "it sits in
    // epoll_wait", "it spins in user space" (the number is old, there are no calls).
    if super::wide() { crate::print!("  #PF/s   L3  STATE   TIME     ALIVE SYSCALL"); } else { crate::print!(" STATE SYSCALL"); }
    crate::println!("");
}

fn row_thread(t: usize, win: u64) {
    let p = prev();
    let leader = crate::sched::proc_of(t);
    let (res, shr, lazy) = rss_of(leader);
    let dr = crate::sched::run_ns(t).saturating_sub(p.run[t]);
    let dq = crate::sched::rq_ns(t).saturating_sub(p.rq[t]);
    crate::print!("{:>5}{:>5} {:<15} {:>3} {:>5} {:>5}  {:>5} {:>4} {:>5} {:>7} {:>8} {:>6}",
        leader, t, comm(t), crate::sched::cpu_of(t), pct(share100(dr, win)), pct(share100(dq, win)),
        bytes(res * 4096), bytes(shr * 4096), bytes(lazy * 4096),
        brate(crate::metrics::frames_of(t).saturating_sub(p.frm[t]) * 4096, win),
        rate(crate::sched::switches(t).saturating_sub(p.sw[t]), win),
        rate(crate::metrics::syscalls_of(t).saturating_sub(p.sys[t]), win));
    if super::wide() {
        let l3 = crate::cat::share_of_cpu(crate::sched::cpu_of(t));
        crate::print!("  {:>6} {:>4} {} {:>8} {:>8}",
            rate(crate::metrics::page_faults_of(t).saturating_sub(p.pf[t]), win),
            if l3 == 0 { super::S::<12>::new() } else { bytes(l3) },
            state(t), dur(crate::sched::run_ns(t)), dur(crate::sched::age_ns(t)));
        crate::print!(" {}", last_sys(t));
    } else {
        crate::print!(" {} {}", state(t), last_sys(t));
    }
    crate::println!("");
}

fn row_proc(leader: usize, win: u64) {
    let p = prev();
    let n = crate::sched::task_count().min(NT);
    let (res, shr, lazy) = rss_of(leader);
    let (mut dr, mut dfrm, mut dsw, mut dsys, mut nth) = (0u64, 0u64, 0u64, 0u64, 0usize);
    let mut dpf = 0u64;
    let mut dq = 0u64;
    for t in 0..n {
        if crate::sched::proc_of(t) != leader || !crate::sched::task_alive(t) { continue; }
        nth += 1;
        dr += crate::sched::run_ns(t).saturating_sub(p.run[t]);
        dq += crate::sched::rq_ns(t).saturating_sub(p.rq[t]);
        dfrm += crate::metrics::frames_of(t).saturating_sub(p.frm[t]);
        dsw += crate::sched::switches(t).saturating_sub(p.sw[t]);
        dsys += crate::metrics::syscalls_of(t).saturating_sub(p.sys[t]);
        dpf += crate::metrics::page_faults_of(t).saturating_sub(p.pf[t]);
    }
    crate::print!("{:>5}{:>6} {:<15} {:>3} {:>5} {:>5}  {:>5} {:>4} {:>5} {:>7} {:>8} {:>6}",
        leader, nth, comm(leader), crate::sched::cpu_of(leader), pct(share100(dr, win)), pct(share100(dq, win)),
        bytes(res * 4096), bytes(shr * 4096), bytes(lazy * 4096),
        brate(dfrm * 4096, win), rate(dsw, win), rate(dsys, win));
    if super::wide() {
        let l3 = crate::cat::share_of_cpu(crate::sched::cpu_of(leader));
        crate::print!("  {:>6} {:>4} {} {:>8} {:>8}",
            rate(dpf, win),
            if l3 == 0 { super::S::<12>::new() } else { bytes(l3) },
            state(leader), dur(crate::sched::run_ns(leader)), dur(crate::sched::age_ns(leader)));
    } else {
        crate::print!(" {}", state(leader));
    }
    crate::println!("");
}

/// The output order: by %CPU (the default) or by memory.
fn draw(threads: bool, by_mem: bool, win: u64, limit: usize) {
    header(win);
    table_head(threads);
    let n = crate::sched::task_count().min(NT);
    let p = prev();
    let mut shown = [false; NT];
    for _ in 0..limit {
        let (mut best, mut bv) = (usize::MAX, 0i64);
        for t in 0..n {
            if shown[t] { continue; }
            if !threads && crate::sched::proc_of(t) != t { continue; }
            let v = if by_mem { rss_of(crate::sched::proc_of(t)).0 as i64 }
                    else { crate::sched::run_ns(t).saturating_sub(p.run[t]) as i64 };
            // The living are ALWAYS above the dead, while the dead are still in the list and in their
            // own order: "the thread died" is an answer, while its absence from the table
            // is not an answer. An offset rather than an inversion: an inversion
            // would reverse the order among the dead.
            const DEAD_BIAS: i64 = 1 << 62;
            let v = if crate::sched::task_alive(t) { v } else { v - DEAD_BIAS };
            if best == usize::MAX || v > bv { bv = v; best = t; }
        }
        if best == usize::MAX { break; }
        shown[best] = true;
        if threads { row_thread(best, win) } else { row_proc(best, win) }
    }
}

// ── the commands ─────────────────────────────────────────────────────────────
/// `ps [-t|-l]` — one snapshot. `-t` — by thread (by process by default),
/// `-m` — without walking the page tables (RSS is not computed: cheap and fine on a live machine).
pub fn cmd_ps(arg: &str) {
    let threads = arg.contains("-t");
    let nomem = arg.contains("-m");
    let (win, first) = take();
    rss_scan(!nomem);
    if first { crate::println!("ps: the first snapshot — the rates are averaged OVER THE WHOLE UPTIME ({}); a repeated call gives an interval", dur(win)); }
    draw(threads, false, win, NT);
    commit();
}

/// `top [-t] [-m] [-1] [interval_ms]` — a refreshing table.
///
/// `-1` — ONE snapshot and exit. It is needed where there is nobody to press a key: a regression
/// suite, the log of a machine without an operator, output into a file. Outside the interactive console
/// (from the scheduler tick, on top of a workload) one snapshot happens without the flag too:
/// waiting for a key from an interrupt handler is not allowed.
pub fn cmd(arg: &str, interactive: bool) {
    let interactive = interactive && !arg.contains("-1");
    let mut threads = arg.contains("-t");
    let mut mem = !arg.contains("-m");
    let mut by_mem = false;
    let mut ms: u64 = 1000;
    for tok in arg.split_whitespace() {
        if let Ok(v) = tok.parse::<u64>() { ms = v.clamp(100, 60_000); }
    }
    if !interactive {
        let (win, _) = take();
        rss_scan(mem);
        draw(threads, by_mem, win, 20);
        commit();
        return;
    }
    crate::print!("\x1b[?25l"); // the cursor must not blink over a refreshing table
    let rows = crate::shell::rows().saturating_sub(12).max(4);
    loop {
        crate::print!("\x1b[H\x1b[2J");
        let (win, _) = take();
        rss_scan(mem);
        draw(threads, by_mem, win, rows);
        commit();
    crate::println!("\n[q]quit [t]threads/processes [m]RSS on/off [c]sort by CPU [s]sort by memory [+/-]interval {} ms", ms);
        // We wait out the interval but parse a key at once: the operator must not
        // feel that the console has died.
        let until = crate::tsc::mono_ns() + ms * 1_000_000;
        loop {
            if let Some(c) = crate::serial::getc() {
                match c {
                    b'q' | 27 | 3 => { crate::print!("\x1b[?25h"); crate::println!(""); return; }
                    b't' => { threads = !threads; break; }
                    b'm' => { mem = !mem; break; }
                    b'c' => { by_mem = false; break; }
                    b's' => { by_mem = true; break; }
                    b'+' => { ms = (ms * 2).min(60_000); break; }
                    b'-' => { ms = (ms / 2).max(100); break; }
                    _ => { break; }
                }
            }
            if crate::tsc::mono_ns() >= until { break; }
            core::hint::spin_loop();
        }
    }
}
