//! The profile of the network datapath by stages (PLAN-V1.1 §7.1): where the time goes
//! between "a frame appeared in the receive ring" and "the frame was handed to the device for transmission".
//!
//! It is switched on by `talaria.netprof=1`, it prints a summary every N processed
//! turnarounds. The stages are marked where the frame changes its owner:
//!
//!   an RX descriptor was seen  --ST_RX2APP-->  recvfrom/read returned into ring 3
//!   ring 3 processed it        --ST_APP----->  the entry into sendto/write
//!   the entry into sendto      --ST_TX------>  the TX descriptor is written and the device is rung
//!   (the sum)                  --ST_RX2TX--->  the full turnaround inside Talaria
//!
//! Plus ST_POLL — the price of one pass of the stack (iface.poll) when there was a frame in it,
//! and the counter of passes per turnaround: one can see how many times the stack is driven in vain.
//! Everything is in TSC cycles, the printing is in nanoseconds. The overhead with the profile off:
//! one atomic read per stage.
//!
//! TWO THINGS WITHOUT WHICH A TAIL CANNOT BE ANALYSED BY THIS (session 21).
//!
//! 1. AN AVERAGE DOES NOT ANSWER THE QUESTION ABOUT p99. The stages printed an average and a maximum;
//!    an average hides one case in a hundred, while a maximum is one point about
//!    which it is unknown whether it is alone or whether there are a thousand of them. So every stage now has
//!    A HISTOGRAM (4 buckets per octave, a step of 25 %) and printing by percentiles.
//!    The question "is our p99 stage X?" is settled by comparing the stage's p99 with the request's
//!    p99, not by an average.
//!
//! 2. THE MARKING OF A TURNAROUND BREAKS ON A MULTI-THREADED SERVER. The marks T_RX/T_APP were
//!    ONE PER MACHINE — which is correct exactly for a single-threaded echo (the kernel echo), where a
//!    turnaround cannot overlap with another one. mongod has four connections and two
//!    threads: while one goes from `read` to `write`, the second one sets T_RX with its own frame,
//!    and the difference is computed BETWEEN DIFFERENT turnarounds. The numbers come out plausible
//!    and wrong. The turnaround stages are left as they are (they are honest for an echo and
//!    are marked), while for multi-threaded workloads a stage has been introduced that can be
//!    measured WITHOUT gluing: ST_DELIV — "the data lie in the socket's buffer" -> "ring 3
//!    picked them up". The count is kept IN THE SOCKET'S SLOT (netsys::Tcp.rdy), so the
//!    connections do not mix, while the meaning is exactly the one we argue about: how long
//!    the frame waited for THE READER. The scheduling latency is visible here and nowhere else.
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

pub const ST_RX2APP: usize = 0;
pub const ST_APP: usize = 1;
pub const ST_TX: usize = 2;
pub const ST_RX2TX: usize = 3;
pub const ST_POLL: usize = 4;
/// "The data are in the socket's buffer" -> "ring 3 picked them up": the pure latency of THE DELIVERY
/// TO THE READER, without gluing the connections (see item 2 in the header).
pub const ST_DELIV: usize = 5;
const NST: usize = 6;
const NAMES: [&str; NST] = ["rx->app", "app", "app->tx", "rx->tx", "poll", "socket->reader"];

// ── the histogram: 4 buckets per octave (a step of 25 %) ─────────────────────
//
// The bucket by cycles: c<4 — bucket c; otherwise (the octave-1)*4 + the two bits below the top one.
// The bucket's lower bound is restored exactly, so a percentile is printed
// as "not less than" — for the question "in which stage is the tail" that is more than enough.
const NB: usize = 256;
#[inline]
fn bucket(c: u64) -> usize {
    if c < 4 { return c as usize; }
    let oct = 63 - c.leading_zeros() as usize;
    let sub = (c >> (oct - 2)) & 3;
    ((oct - 1) << 2) + sub as usize
}
fn bucket_lo(b: usize) -> u64 {
    if b < 4 { return b as u64; }
    let oct = (b >> 2) + 1;
    (4 + (b & 3) as u64) << (oct - 2)
}

static ON: AtomicBool = AtomicBool::new(false);
static N: [AtomicU64; NST] = [const { AtomicU64::new(0) }; NST];
static SUM: [AtomicU64; NST] = [const { AtomicU64::new(0) }; NST];
static MAX: [AtomicU64; NST] = [const { AtomicU64::new(0) }; NST];
static HIST: [[AtomicU64; NB]; NST] = [const { [const { AtomicU64::new(0) }; NB] }; NST];
static POLLS: AtomicU64 = AtomicU64::new(0);      // the stack's passes in total
static TURNS: AtomicU64 = AtomicU64::new(0);      // the full turnarounds (a frame -> an answer)
static EVERY: AtomicU64 = AtomicU64::new(2000);   // print once every N turnarounds

// The timestamps of the current turnaround (the datapath is single-threaded: one core, the syscalls are not nested).
static T_RX: AtomicU64 = AtomicU64::new(0);
static T_APP: AtomicU64 = AtomicU64::new(0);
static T_TX0: AtomicU64 = AtomicU64::new(0);

pub fn enable(every: u64) { ON.store(true, Ordering::Relaxed); EVERY.store(every.max(1), Ordering::Relaxed); }
#[inline] pub fn on() -> bool { ON.load(Ordering::Relaxed) }

#[inline]
fn add(st: usize, cyc: u64) {
    N[st].fetch_add(1, Ordering::Relaxed);
    SUM[st].fetch_add(cyc, Ordering::Relaxed);
    if cyc > MAX[st].load(Ordering::Relaxed) { MAX[st].store(cyc, Ordering::Relaxed); }
    HIST[st][bucket(cyc)].fetch_add(1, Ordering::Relaxed);
}

/// The stage of the delivery to the reader: it is called from `netsys::read` when the data are HANDED into
/// ring 3, with the mark from the socket's slot (the moment when the data appeared there).
#[inline]
pub fn mark_deliv(rdy: u64) {
    if !on() || rdy == 0 { return; }
    add(ST_DELIV, crate::tsc::rdtsc().wrapping_sub(rdy));
}

// ── THE `app` STAGE PER THREAD (`talaria.appprof=1`) ─────────────────────────
//
// The turnaround stage `app` above is one per machine and on a multi-threaded server it
// glues different turnarounds together (item 2 in the header). Here the count is kept IN
// THE TASK: from the `read` that returned data to the entry into `write` of the same thread. And
// it is split into two parts by the scheduler's accounting (`sched::run_cycles`): how much the
// thread RAN (its own count) and how much it was TAKEN OFF THE CORE (preempted
// or asleep). Session 32: the delivery of a frame to the reader was fixed (`wakepull`), while
// p99 stands still — so the extra 100 us are somewhere between `read` and `write`,
// and the first question is "was the thread computing or waiting for the processor".
static APP_ON: AtomicBool = AtomicBool::new(false);
pub fn app_enable() { APP_ON.store(true, Ordering::Relaxed); }
const NT: usize = crate::sched::NTASKS;
static A_T0: [AtomicU64; NT] = [const { AtomicU64::new(0) }; NT];
static A_RUN0: [AtomicU64; NT] = [const { AtomicU64::new(0) }; NT];
static A_SW0: [AtomicU64; NT] = [const { AtomicU64::new(0) }; NT];
const AST_ALL: usize = 0; const AST_RAN: usize = 1; const AST_OFF: usize = 2;
// The second stage is THE TURNAROUND'S TAIL: from the entry into `write` to the entry into the network wait
// (`NetWaiter::new`). The read->write stage of mysql pk costs p99 45 us with a request's p99
// of 160 — so the extra is either here (ring 3 after the answer) or before `read`.
const WST_ALL: usize = 3; const WST_RAN: usize = 4; const WST_OFF: usize = 5; const NAST: usize = 6;
const ANAMES: [&str; NAST] = ["read->write", "  on the core", "  off the core", "write->wait", "  on the core", "  off the core"];
static W_T0: [AtomicU64; NT] = [const { AtomicU64::new(0) }; NT];
static W_RUN0: [AtomicU64; NT] = [const { AtomicU64::new(0) }; NT];
static AN: [AtomicU64; NAST] = [const { AtomicU64::new(0) }; NAST];
static ASUM: [AtomicU64; NAST] = [const { AtomicU64::new(0) }; NAST];
static AMAX: [AtomicU64; NAST] = [const { AtomicU64::new(0) }; NAST];
static AHIST: [[AtomicU64; NB]; NAST] = [const { [const { AtomicU64::new(0) }; NB] }; NAST];
/// The slow ones (off the core >= 50 us) by the number of the thread's switches during the stage: 0/1/2/3+.
static A_SLOW_SW: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// The slow ones by HOW MUCH the thread ran: <10 / <50 / >=50 us.
static A_SLOW_RAN: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
fn aadd(st: usize, cyc: u64) {
    AN[st].fetch_add(1, Ordering::Relaxed);
    ASUM[st].fetch_add(cyc, Ordering::Relaxed);
    if cyc > AMAX[st].load(Ordering::Relaxed) { AMAX[st].store(cyc, Ordering::Relaxed); }
    AHIST[st][bucket(cyc)].fetch_add(1, Ordering::Relaxed);
}
#[inline] fn app_in_task() {
    let t = crate::sched::current_id();
    if t >= NT { return; }
    A_RUN0[t].store(crate::sched::run_cycles(t), Ordering::Relaxed);
    A_SW0[t].store(crate::sched::switches(t), Ordering::Relaxed);
    A_OFFT[t].store(0, Ordering::Relaxed);
    // "A request in progress" is an OPEN stage; in order not to walk the task
    // table at every rotation, the number of open ones is kept by a per-core counter
    // (the rule of session 39: a walk in the hot path is an instrument that lies about itself).
    if A_T0[t].swap(crate::tsc::rdtsc(), Ordering::Relaxed) == 0 { nreq_add(t, 1); }
}
#[inline] fn app_out_task() {
    let t = crate::sched::current_id();
    if t >= NT { return; }
    let t0 = A_T0[t].swap(0, Ordering::Relaxed);
    if t0 == 0 { return; }
    nreq_add(t, -1);
    A_OFFT[t].store(0, Ordering::Relaxed);
    let now = crate::tsc::rdtsc();
    let all = now.wrapping_sub(t0);
    let run_now = crate::sched::run_cycles(t);
    let ran = run_now.wrapping_sub(A_RUN0[t].load(Ordering::Relaxed)).min(all);
    let off = all - ran;
    aadd(AST_ALL, all); aadd(AST_RAN, ran); aadd(AST_OFF, off);
    W_T0[t].store(now, Ordering::Relaxed); W_RUN0[t].store(run_now, Ordering::Relaxed);
    let hz = crate::tsc::calibrate_cached().max(1);
    let us = |c: u64| c / (hz / 1_000_000).max(1);
    if us(off) >= 50 {
        let sw = crate::sched::switches(t).wrapping_sub(A_SW0[t].load(Ordering::Relaxed));
        A_SLOW_SW[sw.min(3) as usize].fetch_add(1, Ordering::Relaxed);
        let r = us(ran);
        A_SLOW_RAN[if r < 10 { 0 } else if r < 50 { 1 } else { 2 }].fetch_add(1, Ordering::Relaxed);
        app_dump(t, t0, us(all), us(off), r, sw);
    }
}
/// THE ANALYSIS: THE THREAD WAS TAKEN OFF THE CORE IN THE MIDDLE OF A REQUEST. X12 (an honest instrument): for the slow
/// turnarounds (>=100 us) 90 % of the extra is the read->write stage, and the thread was not
/// computing meanwhile but was taken off the core, having worked <50 us. Who took the core and why —
/// the decisions of the scheduler of the thread's core during the stage (as in the analysis of a delivery).
pub const APP_DUMP_N: usize = 4;
pub const APP_DUMP_EV: usize = 10;
pub struct AppDump {
    /// The slot's claim (a swap: two cores that ended a slow turnaround at once both saw a
    /// free `used` and wrote one slot field by field) and its publication (`used`).
    pub claim: AtomicBool, pub used: AtomicBool, pub t: u8, pub cpu: u8, pub sw: u8, pub us: u32, pub off: u32, pub ran: u32,
    pub t0: u64, pub n: u8, pub ev: [crate::sched::SchedEv; APP_DUMP_EV],
}
const AD0: AppDump = AppDump { claim: AtomicBool::new(false), used: AtomicBool::new(false), t: 0, cpu: 0, sw: 0, us: 0, off: 0, ran: 0, t0: 0, n: 0, ev: [crate::sched::SEV0; APP_DUMP_EV] };
pub static mut APP_DUMP: [AppDump; APP_DUMP_N] = [AD0; APP_DUMP_N];
static APP_DUMP_MISSED: AtomicU64 = AtomicU64::new(0);
fn app_dump(t: usize, t0: u64, us: u64, off: u64, ran: u64, sw: u64) {
    // While the `stat:` block is being flushed — do not write (the same lesson as for `dl_dump`).
    if crate::obs::flush_active() { return; }
    for k in 0..APP_DUMP_N {
        // SAFETY: only the place is taken; nothing is read or written through it here.
        let p = unsafe { core::ptr::addr_of_mut!(APP_DUMP[k]) };
        // SAFETY: the claim is an atomic in a static, read through a shared view.
        if unsafe { (*p).claim.swap(true, Ordering::Acquire) } { continue; }
        // SAFETY: the slot is this core's from the claim on; the reader looks at it only
        // after `used` (Release below, Acquire there).
        let d = unsafe { &mut *p };
        let cpu = crate::sched::cpu_of(t);
        d.t = t as u8; d.cpu = cpu as u8; d.sw = sw.min(255) as u8;
        d.us = us.min(u32::MAX as u64) as u32; d.off = off.min(u32::MAX as u64) as u32; d.ran = ran.min(u32::MAX as u64) as u32;
        d.t0 = t0;
        d.n = crate::sched::strace_take(cpu, t0, 2, &mut d.ev) as u8;
        d.used.store(true, Ordering::Release);
        return;
    }
    APP_DUMP_MISSED.fetch_add(1, Ordering::Relaxed);
}
// ── STEP 1 OF SESSION 43: TO WHOM THE CORE GOES FROM A THREAD THAT HAS A REQUEST IN PROGRESS ──
//
// Session 42 named the payer of the 140-400 us band on `mongodb insert`: at
// `p99.9` of the `read->write` stage the thread is 226 us out of 316 NOT ON THE CORE, and it is taken off by
// A TICK in the middle of a request. That cannot be cured by the quantum's length — it has been checked with a knob
// (`talaria.tickmin=200000` WIDENED the band). What remains is the policy "the thread that
// has a request in progress returns to the core first", but it exists
// in exactly one case: if the core goes to a thread WITHOUT a request. If the neighbours are also in
// a request, the policy degenerates into a long quantum, that is, into harm that has already been
// measured. The instrument answers this fork WITH A NUMBER, before a single change.
//
// It is counted at every rotation that takes a thread with an OPEN stage off: who got
// the core, how many threads on that core are in a request simultaneously and after how long the
// removed thread came back.
pub const SW_REQ: usize = 0;      // the chosen thread is ALSO in a request
pub const SW_R3: usize = 1;       // ring 3, there is no request in progress
pub const SW_NETW: usize = 2;     // a network waiter
pub const SW_KERN: usize = 3;     // a kernel thread (a ring0 task)
pub const SW_NOBODY: usize = 4;   // nobody (a number outside the task table / idle)
pub const NSW: usize = 5;
const SWNAMES: [&str; NSW] = ["also in a request", "ring 3 without a request", "a network waiter", "a kernel thread", "nobody"];
static S_WHO: [AtomicU64; NSW] = [const { AtomicU64::new(0) }; NSW];
/// How many threads on the core were in a request AT THAT MOMENT: 0 / 1 / 2 / 3 / 4+.
/// The bucket "0" is not an empty cell but THE INSTRUMENT'S HONESTY: the counter of open
/// stages per core diverges from the truth if a thread died with an open stage
/// or moved to another core in the middle of a request. While "0" is small, the counter
/// can be trusted; if it is noticeable, it cannot be, and that is visible at once.
static S_NREQ: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
/// The open stages per core (a counter instead of a walk) and the core on which the stage
/// was opened (a thread may move — then the counter will return to where it was taken from).
static A_NREQ: [AtomicU64; crate::sched::MAXCPU] = [const { AtomicU64::new(0) }; crate::sched::MAXCPU];
static A_CPU: [AtomicU8; NT] = [const { AtomicU8::new(255) }; NT];
/// The cycle at which a thread taken off in the middle of a request left the core (0 — it is on the core).
static A_OFFT: [AtomicU64; NT] = [const { AtomicU64::new(0) }; NT];
/// The return of a removed thread: the same histogram as for the stages.
static R_N: AtomicU64 = AtomicU64::new(0);
static R_SUM: AtomicU64 = AtomicU64::new(0);
static R_MAX: AtomicU64 = AtomicU64::new(0);
static R_HIST: [AtomicU64; NB] = [const { AtomicU64::new(0) }; NB];

fn nreq_add(t: usize, d: i64) {
    if d > 0 {
        let c = crate::sched::cpu_of(t);
        A_CPU[t].store(c.min(255) as u8, Ordering::Relaxed);
        if c < crate::sched::MAXCPU { A_NREQ[c].fetch_add(1, Ordering::Relaxed); }
    } else {
        let c = A_CPU[t].swap(255, Ordering::Relaxed) as usize;
        if c < crate::sched::MAXCPU && A_NREQ[c].load(Ordering::Relaxed) > 0 {
            A_NREQ[c].fetch_sub(1, Ordering::Relaxed);
        }
    }
}
/// The classification of THE CHOSEN thread — exactly the fork for whose sake the instrument exists.
/// The decision table is separated from reading the state ON PURPOSE: the state lives in
/// three different subsystems, and there is no other way to check their combinations with a gate.
pub fn who_of(in_req: bool, alive: bool, net_waiter: bool, ring0: bool) -> usize {
    if !alive { return SW_NOBODY; }
    if in_req { return SW_REQ; }
    if net_waiter { return SW_NETW; }
    if ring0 { return SW_KERN; }
    SW_R3
}
fn who_is(to: usize) -> usize {
    if to >= NT { return SW_NOBODY; }
    who_of(A_T0[to].load(Ordering::Relaxed) != 0, true,
           crate::netsys::is_net_waiter(to), crate::sched::kstack_top(to) == 0)
}
/// The scheduler's decision: the core `cpu` moves from the thread `from` to the thread `to`.
/// It is called from `sched::strace_note` only at A CHANGE of thread (from != to).
#[inline]
pub fn app_sched_note(cpu: usize, from: usize, to: usize) {
    if !APP_ON.load(Ordering::Relaxed) { return; }
    let now = crate::tsc::rdtsc();
    // A thread taken off in the middle of a request is awaited back: we close it by its return.
    if to < NT {
        let off = A_OFFT[to].swap(0, Ordering::Relaxed);
        if off != 0 {
            let d = now.wrapping_sub(off);
            R_N.fetch_add(1, Ordering::Relaxed);
            R_SUM.fetch_add(d, Ordering::Relaxed);
            if d > R_MAX.load(Ordering::Relaxed) { R_MAX.store(d, Ordering::Relaxed); }
            R_HIST[bucket(d)].fetch_add(1, Ordering::Relaxed);
        }
    }
    if from >= NT || A_T0[from].load(Ordering::Relaxed) == 0 { return; }
    S_WHO[who_is(to)].fetch_add(1, Ordering::Relaxed);
    let n = if cpu < crate::sched::MAXCPU { A_NREQ[cpu].load(Ordering::Relaxed) } else { 0 };
    S_NREQ[n.min(4) as usize].fetch_add(1, Ordering::Relaxed);
    A_OFFT[from].store(now, Ordering::Relaxed);
}
/// The instrument's lines into the `stat:` block (it is called from `dump_app`).
fn dump_sched() {
    let who: [u64; NSW] = core::array::from_fn(|i| S_WHO[i].load(Ordering::Relaxed));
    let all: u64 = who.iter().sum();
    if all == 0 { return; }
    crate::println!("stat: app rotations in the middle of a request: in total {}", all);
    for i in 0..NSW {
        if who[i] == 0 { continue; }
        crate::println!("stat:   the core went to: {:26} {} ({}.{:02} %)",
            SWNAMES[i], who[i], who[i] * 100 / all, who[i] * 10000 / all % 100);
    }
    let nq: [u64; 5] = core::array::from_fn(|i| S_NREQ[i].load(Ordering::Relaxed));
    crate::println!("stat:   threads in a request on this core: 0:{} 1:{} 2:{} 3:{} 4+:{}", nq[0], nq[1], nq[2], nq[3], nq[4]);
    let n = R_N.load(Ordering::Relaxed);
    if n == 0 { return; }
    let hz = crate::tsc::calibrate_cached().max(1);
    let ns = |c: u64| c * 1_000 / (hz / 1_000_000).max(1);
    let q = |p: u64| -> u64 {
        let want = n * p / 10000;
        let (mut acc, mut b) = (0u64, 0usize);
        while b < NB {
            acc += R_HIST[b].load(Ordering::Relaxed);
            if acc > want { return bucket_lo(b); }
            b += 1;
        }
        R_MAX.load(Ordering::Relaxed)
    };
    crate::println!("stat:   the return of the removed thread: n={} avg={} p50={} p90={} p99={} p99.9={} max={} (ns)",
        n, ns(R_SUM.load(Ordering::Relaxed) / n), ns(q(5000)), ns(q(9000)), ns(q(9900)), ns(q(9990)), ns(R_MAX.load(Ordering::Relaxed)));
}
/// The gate `reqwho`: the instrument is checked on A SYNTHETIC sequence of
/// decisions — otherwise the arithmetic of the shares and the histogram of the return live only on
/// the stand, where there is nothing to compare an error in them with.
pub fn sched_probe(from: usize, to: usize, cpu: usize) { app_sched_note(cpu, from, to); }
pub fn sched_counters() -> ([u64; NSW], [u64; 5], u64) {
    (core::array::from_fn(|i| S_WHO[i].load(Ordering::Relaxed)),
     core::array::from_fn(|i| S_NREQ[i].load(Ordering::Relaxed)),
     R_N.load(Ordering::Relaxed))
}
/// Open/close a stage of A FOREIGN thread (the gate only: on a live machine a stage
/// is opened where `read` returned data).
pub fn app_probe_open(t: usize, on: bool) {
    if t >= NT { return; }
    A_OFFT[t].store(0, Ordering::Relaxed);   // as in `app_out_task`: the stage is closed — there is nobody to wait back
    if on { if A_T0[t].swap(crate::tsc::rdtsc(), Ordering::Relaxed) == 0 { nreq_add(t, 1); } }
    else if A_T0[t].swap(0, Ordering::Relaxed) != 0 { nreq_add(t, -1); }
}

/// A thread entered the network wait — the end of the write -> the wait stage.
#[inline] pub fn mark_wait_in() {
    if !APP_ON.load(Ordering::Relaxed) { return; }
    let t = crate::sched::current_id();
    if t >= NT { return; }
    let t0 = W_T0[t].swap(0, Ordering::Relaxed);
    if t0 == 0 { return; }
    let all = crate::tsc::rdtsc().wrapping_sub(t0);
    let ran = crate::sched::run_cycles(t).wrapping_sub(W_RUN0[t].load(Ordering::Relaxed)).min(all);
    aadd(WST_ALL, all); aadd(WST_RAN, ran); aadd(WST_OFF, all - ran);
}
/// The summary of the `app` stage per thread — into the `stat:` block (see metrics.rs).
pub fn dump_app() {
    if !APP_ON.load(Ordering::Relaxed) { return; }
    let hz = crate::tsc::calibrate_cached().max(1);
    let ns = |c: u64| c * 1_000 / (hz / 1_000_000).max(1);
    for st in 0..NAST {
        let n = AN[st].load(Ordering::Relaxed);
        if n == 0 { continue; }
        let q = |p: u64| -> u64 {
            let want = n * p / 10000;
            let (mut acc, mut b) = (0u64, 0usize);
            while b < NB {
                acc += AHIST[st][b].load(Ordering::Relaxed);
                if acc > want { return bucket_lo(b); }
                b += 1;
            }
            AMAX[st].load(Ordering::Relaxed)
        };
        crate::println!("stat: app per thread {:12} n={} avg={} p50={} p90={} p99={} p99.9={} max={} (ns)",
            ANAMES[st], n, ns(ASUM[st].load(Ordering::Relaxed) / n),
            ns(q(5000)), ns(q(9000)), ns(q(9900)), ns(q(9990)), ns(AMAX[st].load(Ordering::Relaxed)));
    }
    let sw: [u64; 4] = core::array::from_fn(|i| A_SLOW_SW[i].load(Ordering::Relaxed));
    let rn: [u64; 3] = core::array::from_fn(|i| A_SLOW_RAN[i].load(Ordering::Relaxed));
        crate::println!("stat:   off the core >=50 us: {} — the thread's switches 0:{} 1:{} 2:{} 3+:{}; it ran <10us:{} <50:{} >=50:{}",
        sw.iter().sum::<u64>(), sw[0], sw[1], sw[2], sw[3], rn[0], rn[1], rn[2]);
    let hz = crate::tsc::hz().max(1);
    for k in 0..APP_DUMP_N {
        let d = unsafe { &mut *core::ptr::addr_of_mut!(APP_DUMP[k]) };
        if !d.used.load(Ordering::Acquire) { continue; }
        crate::print!("stat:   the app analysis: the thread t{} on core {}: read->write {} us, off the core {}, it ran {}, switches {}; the core's decisions:",
            d.t, d.cpu, d.us, d.off, d.ran, d.sw);
        for j in 0..(d.n as usize).min(APP_DUMP_EV) {
            let e = d.ev[j];
            let dt = e.tsc.wrapping_sub(d.t0);
            let (sg, us) = if dt < (1u64 << 62) { ("+", dt.saturating_mul(1_000_000) / hz) } else { ("-",
                d.t0.wrapping_sub(e.tsc).saturating_mul(1_000_000) / hz) };
            let why = match e.why & 0x7f { 1 => "hint", 2 => "a foreign hint->rotation", 3 => "rotation", 4 => "stay:lock", 5 => "stay:nobody",
                6 => "stay:interception", 7 => "steal", 8 => "stay:it ran little", 9 => "stay:a request in progress", _ => "?" };
            crate::print!(" [{}{} {} t{}->t{} {}", sg, us, if e.why & 0x80 != 0 { "tick" } else { "yield" }, e.from, e.to, why);
            if e.hint != 255 { crate::print!(" h:t{}", e.hint); }
            if e.why & 0x80 != 0 { if e.rip == 3 { crate::print!(" @r3"); } else if e.rip != 0 { crate::print!(" @{:#x}", e.rip); } }
            else if e.rip != 0 { crate::print!(" from:{}", crate::sched::YO_NAMES[(e.rip as usize).min(crate::sched::YO_NAMES.len() - 1)]); }
            crate::print!("]");
        }
        crate::println!();
        d.used.store(false, Ordering::Release);
    }
    let m = APP_DUMP_MISSED.swap(0, Ordering::Relaxed);
    if m != 0 { crate::println!("stat:   app analyses that did not fit: {}", m); }
    dump_sched();
}

/// A frame was seen in the receive ring (l1::SmolDev::receive).
#[inline] pub fn mark_rx() { if on() { T_RX.store(crate::tsc::rdtsc(), Ordering::Relaxed); } }
/// §4.7: the same, but the moment of reference is THE ENTRY into the stack's pass that found
/// the frame (the kernel echo answers inside the pass, so "we saw a frame" and "we processed it"
/// cannot be separated otherwise). That way ST_RX2TX is the full internal price of a turnaround.
#[inline] pub fn mark_rx_at(t0: u64) { if on() && t0 != 0 { T_RX.store(t0, Ordering::Relaxed); } }
/// recvfrom/read returned into ring 3 with data.
#[inline] pub fn mark_app_in() {
    // A REQUEST IN PROGRESS is a per-task sign that is ALWAYS on (step 2 of session 43,
    // `sched::REQ_T0`): one store, only the scheduler reads it and only
    // when the knob `talaria.reqfirst=` is not zero.
    crate::sched::req_begin();
    if APP_ON.load(Ordering::Relaxed) { app_in_task(); }
    if !on() { return; }
    let t = crate::tsc::rdtsc();
    let r = T_RX.load(Ordering::Relaxed);
    if r != 0 { add(ST_RX2APP, t.wrapping_sub(r)); }
    T_APP.store(t, Ordering::Relaxed);
}
/// The entry into sendto/write from ring 3.
#[inline] pub fn mark_app_out() {
    crate::sched::req_end();      // the answer has gone — there is no request in progress any more
    if APP_ON.load(Ordering::Relaxed) { app_out_task(); }
    if !on() { return; }
    let t = crate::tsc::rdtsc();
    let a = T_APP.load(Ordering::Relaxed);
    if a != 0 { add(ST_APP, t.wrapping_sub(a)); T_APP.store(0, Ordering::Relaxed); }
    T_TX0.store(t, Ordering::Relaxed);
}
/// The transmit descriptor is written (the frame has gone to the device).
#[inline] pub fn mark_tx() {
    if !on() { return; }
    let t = crate::tsc::rdtsc();
    let s = T_TX0.load(Ordering::Relaxed);
    if s != 0 { add(ST_TX, t.wrapping_sub(s)); T_TX0.store(0, Ordering::Relaxed); }
    let r = T_RX.load(Ordering::Relaxed);
    if r != 0 {
        add(ST_RX2TX, t.wrapping_sub(r));
        T_RX.store(0, Ordering::Relaxed);
        let n = TURNS.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(EVERY.load(Ordering::Relaxed)) { dump_deferred(); }
    }
}
/// One pass of the stack (iface.poll): cyc is its price.
#[inline] pub fn mark_poll(cyc: u64) {
    if !on() { return; }
    POLLS.fetch_add(1, Ordering::Relaxed);
    add(ST_POLL, cyc);
}

/// THE SUMMARY FROM THE DATAPATH — ONLY THROUGH A BUFFER. It used to be printed right here, between
/// frames: a dozen lines over a polled UART (40-60 us per byte) in the middle of
/// processing a packet. The measurement: mongodb insert with `talaria.netprof=1` gives
/// a service p50 of 32 MS instead of 130 us — the profile killed exactly what it measured,
/// and the stage numbers from such a run mean nothing. Now the summary
/// is collected into memory and leaves in portions (by the same mechanism as the snapshot of
/// `talaria.stat=`). Where there is nobody to shepherd the portions — for a workload without a
/// scheduler (the echo server, the userver gates) — the block is pushed out in place:
/// the behaviour is the former one, and the gates of those workloads look exactly at the summary.
fn dump_deferred() {
    if !crate::obs::deferred_print(dump) { SKIPPED.fetch_add(1, Ordering::Relaxed); }
}
/// How many summaries were skipped (the previous one has not been flushed yet).
static SKIPPED: AtomicU64 = AtomicU64::new(0);

pub fn dump() {
    let hz = crate::tsc::calibrate_cached().max(1);
    let ns = |c: u64| c * 1_000_000_000 / hz;
    let turns = TURNS.load(Ordering::Relaxed).max(1);
    if SKIPPED.load(Ordering::Relaxed) != 0 {
        crate::println!("netprof: summaries skipped {} (the previous one has not been flushed to the line yet)", SKIPPED.load(Ordering::Relaxed));
    }
    crate::println!("netprof: turnarounds={} stack_passes/turnaround={}.{:02}",
        TURNS.load(Ordering::Relaxed), POLLS.load(Ordering::Relaxed) / turns,
        POLLS.load(Ordering::Relaxed) * 100 / turns % 100);
    // Next to the number of passes — by what they were cancelled and whether the sign
    // "the card is silent" can be trusted (see netsys::RING_LIED). Here, and not only in `kstat`,
    // because for a workload without a scheduler (the echo server) there is nobody to ask `kstat`,
    // while the gate is obliged to check this sign.
    {   let sk = crate::netsys::POLL_SKIPPED.load(Ordering::Relaxed);
        if sk != 0 {
            crate::println!("netprof: idle_passes_cancelled={} by_the_timers_debt={} the_ring_lied={}",
                sk, crate::netsys::POLL_HARD_RUNS.load(Ordering::Relaxed),
                crate::netsys::RING_LIED.load(Ordering::Relaxed));
        }
    }
    for st in 0..NST {
        let n = N[st].load(Ordering::Relaxed);
        if n == 0 { continue; }
        let q = |p: u64| -> u64 {
            let want = n * p / 1000;          // p in per mille
            let (mut acc, mut b) = (0u64, 0usize);
            while b < NB {
                acc += HIST[st][b].load(Ordering::Relaxed);
                if acc > want { return bucket_lo(b); }
                b += 1;
            }
            MAX[st].load(Ordering::Relaxed)
        };
        crate::println!("netprof:   {:12} n={} avg={} p50={} p99={} p99.9={} max={} (ns)",
            NAMES[st], n, ns(SUM[st].load(Ordering::Relaxed) / n),
            ns(q(500)), ns(q(990)), ns(q(999)), ns(MAX[st].load(Ordering::Relaxed)));
    }
    if let Some(d) = crate::netsys::db_writes() {
        crate::println!("netprof:   calls to the device={} ({}.{:02} per turnaround)",
            d, d / turns, d * 100 / turns % 100);
    }
}
