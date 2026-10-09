//! A SLOW INTERVAL NAMED BY THE WORKLOAD ITSELF: who held the cores while it lasted
//! (ARM64-PLAN §11 item 2b).
//!
//! Why. `examples/mdpipe` on a Graviton with two cores has a stage p99 of 33 us against Linux's
//! 4.6-5.7, and every kernel-side histogram so far missed the size of it: "woken -> ran" has only
//! 1.2-2 k wakeups over 20 us where a p99 needs ~6 k samples (ENGINEERING-NOTES item 47). Only the
//! workload knows which of its intervals were slow — a tick sits in a queue, waits for a mutex,
//! for a worker, for a core — so it says so: `prctl(PR_TALARIA_SLOW, t0_ns, t1_ns)` with the two
//! CLOCK_MONOTONIC stamps of the interval (mdpipe's `--mark-us N`: a stage over N us). Linux
//! answers EINVAL to the unknown option, so the same binary runs on both.
//!
//! What the kernel keeps. For every mark: the scheduler's decisions of EVERY core inside
//! [t0, t1] (the rings of `sched::STRACE`), folded into "who was on the cores" — the cycles each
//! task (or nobody) held a core during slow intervals, summed over all the marks; the first
//! `KEEP` marks are kept whole, decision by decision. The `stat:` block prints both.
//!
//! Its limits. The occupant is the task the scheduler put on the core; a futex waiter sleeping
//! in `wfe`/`mwait` as its core's current task counts as occupying it — which is exactly one of
//! the candidates — and a ring of 128 decisions per core covers a window of a few milliseconds
//! at most (an older start is reported as `cut`). The price: nothing until a process calls it;
//! a call walks the rings of the online cores (128 records each).
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::sched::{SchedEv, SEV0, NTASKS, MAXCPU};

/// The option number: "TAL" + 1; no Linux option is near it.
pub const PR_TALARIA_SLOW: u64 = 0x5441_4c01;
/// Marks kept decision by decision.
const KEEP: usize = 6;
/// Cores kept per mark, decisions per core.
const KCPU: usize = 4;
const KEV: usize = 12;
/// The occupant index for "nobody" (the pickup loop / an idle core).
const IDLE: usize = 255;

static MARKS: AtomicU64 = AtomicU64::new(0);
static MARK_CYC: AtomicU64 = AtomicU64::new(0);
/// A window older than the ring: the start of the interval was before the core's oldest record.
static CUT: AtomicU64 = AtomicU64::new(0);
/// The cycles each task held a core during the marked intervals (index 255 — nobody).
static OCC: [AtomicU64; 256] = [const { AtomicU64::new(0) }; 256];
/// The decisions inside the windows by their reason (`why & 0x7f`, 0..15) and the ticks among them.
static WHY: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];
static TICKS: AtomicU64 = AtomicU64::new(0);
/// The interval's own thread: how much of the windows it was on SOME core.
static SELF_CYC: AtomicU64 = AtomicU64::new(0);
/// Of `OCC`: the cycles its occupant spent ASLEEP as the core's current task (a futex waiter in
/// `wfe`/`mwait`/`hlt`, a `nanosleep`) — on the core, but not running.
static OCC_SLEEP: [AtomicU64; 256] = [const { AtomicU64::new(0) }; 256];

// THE CORES' SLEEPS. `paging::halt_enter/exit` bracket every sleep of a core; once the first mark
// came (`ARMED`), each core keeps its last `NSL` sleep intervals, so a mark can subtract a sleeping
// occupant's sleep from its holding. Before that the price is one Relaxed load per sleep.
const NSL: usize = 32;
static ARMED: AtomicBool = AtomicBool::new(false);
static SL_T0: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
static SL_POS: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
static SL: [[(AtomicU64, AtomicU64); NSL]; MAXCPU] = [const { [const { (AtomicU64::new(0), AtomicU64::new(0)) }; NSL] }; MAXCPU];
#[inline]
pub fn halt_in(cpu: usize) {
    if ARMED.load(Ordering::Relaxed) && cpu < MAXCPU { SL_T0[cpu].store(crate::tsc::rdtsc(), Ordering::Relaxed); }
}
#[inline]
pub fn halt_out(cpu: usize) {
    if !ARMED.load(Ordering::Relaxed) || cpu >= MAXCPU { return; }
    let t0 = SL_T0[cpu].swap(0, Ordering::Relaxed);
    if t0 == 0 { return; }
    let p = SL_POS[cpu].fetch_add(1, Ordering::Relaxed) as usize % NSL;
    // The end first as zero: a reader that meets a half-written pair skips it.
    SL[cpu][p].1.store(0, Ordering::Relaxed);
    SL[cpu][p].0.store(t0, Ordering::Relaxed);
    SL[cpu][p].1.store(crate::tsc::rdtsc(), Ordering::Release);
}
/// The cycles the core `c` slept inside [s, e) — the kept intervals plus a sleep still going.
fn slept_in(c: usize, s: u64, e: u64) -> u64 {
    let mut v = 0u64;
    let ov = |x: u64, y: u64| if y > x { y.min(e).saturating_sub(x.max(s)) } else { 0 };
    for k in 0..NSL {
        let y = SL[c][k].1.load(Ordering::Acquire);
        let x = SL[c][k].0.load(Ordering::Relaxed);
        if y != 0 && x != 0 { v += ov(x, y); }
    }
    let t0 = SL_T0[c].load(Ordering::Relaxed);
    if t0 != 0 { v += ov(t0, e); }
    v.min(e.saturating_sub(s))
}

struct Kept {
    claim: AtomicBool, used: AtomicBool,
    t: u8, cpu: u8, us: u32, t0: u64,
    n: [u8; KCPU], ev: [[SchedEv; KEV]; KCPU],
}
const K0: Kept = Kept { claim: AtomicBool::new(false), used: AtomicBool::new(false), t: 0, cpu: 0, us: 0, t0: 0,
    n: [0; KCPU], ev: [[SEV0; KEV]; KCPU] };
static mut KEPT: [Kept; KEEP] = [K0; KEEP];

/// A monotonic nanosecond stamp as a counter value (`wallclock::mono_ns` is the counter over its rate).
fn cyc_of(ns: u64, hz: u64) -> u64 { ((ns as u128 * hz as u128) / 1_000_000_000) as u64 }

/// `prctl(PR_TALARIA_SLOW, t0_ns, t1_ns)` from the current task. Returns 0, or -EINVAL for an
/// interval that is empty, in the future or longer than a second.
pub fn mark(t0_ns: u64, t1_ns: u64) -> i64 {
    let hz = crate::tsc::calibrate_cached().max(1);
    let now = crate::tsc::rdtsc();
    let (a, b) = (cyc_of(t0_ns, hz), cyc_of(t1_ns, hz).min(now));
    if b <= a || b - a > hz { return -22; }
    let me = crate::sched::current_id();
    ARMED.store(true, Ordering::Relaxed);
    MARKS.fetch_add(1, Ordering::Relaxed);
    MARK_CYC.fetch_add(b - a, Ordering::Relaxed);
    let ncpu = crate::smp::nr_cpus().clamp(1, MAXCPU);
    // The slot for the whole record, if one is free (a mark is not refused for want of a slot).
    let mut slot: Option<&mut Kept> = None;
    for k in 0..KEEP {
        // SAFETY: only the place is taken here; the claim below makes the slot this core's.
        let p = unsafe { core::ptr::addr_of_mut!(KEPT[k]) };
        // SAFETY: the claim is an atomic in a static.
        if unsafe { (*p).claim.swap(true, Ordering::Acquire) } { continue; }
        // SAFETY: claimed: nobody else writes it, the printer reads it only after `used`.
        slot = Some(unsafe { &mut *p });
        break;
    }
    if let Some(s) = slot.as_deref_mut() {
        s.t = me.min(255) as u8; s.cpu = crate::percpu::cpu_id().min(255) as u8;
        s.us = ((b - a) * 1_000_000 / hz).min(u32::MAX as u64) as u32; s.t0 = a;
    }
    // 1.5 KB on the syscall's stack: a window of a slow interval holds a few dozen decisions.
    let mut buf = [SEV0; 64];
    for c in 0..ncpu {
        let n = crate::sched::strace_take(c, a, 1, &mut buf);
        if let Some(s) = slot.as_deref_mut() {
            if c < KCPU {
                let k = n.min(KEV);
                s.ev[c][..k].copy_from_slice(&buf[..k]);
                s.n[c] = k as u8;
            }
        }
        // Fold the window: the occupant before the first decision inside it is that decision's
        // `from` (or the previous record's `to`), and each decision hands the core to its `to`.
        let mut at = a;
        let mut occ = usize::MAX;
        let mut started = false;
        for e in &buf[..n] {
            let inside = e.tsc.wrapping_sub(a) < (1u64 << 62);
            if !inside { occ = e.to as usize; started = true; continue; }
            if e.tsc >= b { break; }
            // No record before the start: the ring no longer holds it (or the core never switched).
            if !started { occ = e.from as usize; started = true; CUT.fetch_add(1, Ordering::Relaxed); }
            add_occ(c, occ, at, e.tsc, me);
            WHY[(e.why & 0x0f) as usize].fetch_add(1, Ordering::Relaxed);
            if e.why & crate::sched::SW_TICK != 0 { TICKS.fetch_add(1, Ordering::Relaxed); }
            occ = e.to as usize;
            at = e.tsc;
        }
        if !started {
            // No decision at or after the start: the core did not switch inside the window; its
            // occupant is the core's current task.
            let cur = crate::percpu::cur_task_of(c);
            occ = if cur == u64::MAX || cur as usize >= NTASKS { IDLE } else { cur as usize };
        }
        add_occ(c, occ, at, b.max(at), me);
    }
    if let Some(s) = slot { s.used.store(true, Ordering::Release); }
    0
}

/// The core `c` was held by `t` over [s, e).
fn add_occ(c: usize, t: usize, s: u64, e: u64, me: usize) {
    let i = if t >= NTASKS || t >= IDLE { IDLE } else { t };
    let cyc = e - s;
    OCC[i].fetch_add(cyc, Ordering::Relaxed);
    OCC_SLEEP[i].fetch_add(slept_in(c, s, e), Ordering::Relaxed);
    if t == me { SELF_CYC.fetch_add(cyc, Ordering::Relaxed); }
}

/// Into the `stat:` block; silent when nothing was marked.
pub fn dump() {
    let marks = MARKS.load(Ordering::Relaxed);
    if marks == 0 { return; }
    let hz = crate::tsc::calibrate_cached().max(1);
    let us = |c: u64| c.saturating_mul(1_000_000) / hz;
    let win = MARK_CYC.load(Ordering::Relaxed);
    let ncpu = crate::smp::nr_cpus().clamp(1, MAXCPU) as u64;
    crate::println!("stat: slow intervals marked by the workload: {} ({} us in all, avg {} us), core-time {} us over {} cores; its own thread on a core {} %",
        marks, us(win), us(win) / marks, us(win) * ncpu, ncpu,
        SELF_CYC.load(Ordering::Relaxed) * 100 / (win * ncpu).max(1));
    // The occupants, the largest first (at most 10).
    let mut done = [false; 256];
    crate::print!("stat:   the cores were held by (of it asleep as the core's current task):");
    for _ in 0..10 {
        let mut best = (0u64, 256usize);
        for i in 0..256 { let v = OCC[i].load(Ordering::Relaxed); if !done[i] && v > best.0 { best = (v, i); } }
        if best.1 == 256 { break; }
        done[best.1] = true;
        let pct = best.0 * 1000 / (win * ncpu).max(1);
        let sl = OCC_SLEEP[best.1].load(Ordering::Relaxed) * 1000 / (win * ncpu).max(1);
        if best.1 == IDLE { crate::print!(" nobody {}.{}%", pct / 10, pct % 10); }
        else { crate::print!(" t{}({}) {}.{}%", best.1, crate::obs::comm(best.1), pct / 10, pct % 10); }
        crate::print!(" (asleep {}.{})", sl / 10, sl % 10);
    }
    crate::println!();
    let w: [u64; 16] = core::array::from_fn(|i| WHY[i].load(Ordering::Relaxed));
    crate::println!("stat:   decisions inside: {} (ticks {}): hint {} hint-rejected {} rotation {} stay:lock {} stay:nobody {} stay:taken {} steal {} stay:ran-little {} stay:request {}; a window older than the ring {}",
        w.iter().sum::<u64>(), TICKS.load(Ordering::Relaxed), w[1], w[2], w[3], w[4], w[5], w[6], w[7], w[8], w[9], CUT.load(Ordering::Relaxed));
    let hzr = crate::tsc::hz().max(1);
    for k in 0..KEEP {
        // SAFETY: the slot is read after its `used` (Acquire), set by the writer last.
        let d = unsafe { &mut *core::ptr::addr_of_mut!(KEPT[k]) };
        if !d.used.load(Ordering::Acquire) { continue; }
        crate::println!("stat:   a slow interval of t{}({}) on core {}: {} us; each core's decisions:", d.t, crate::obs::comm(d.t as usize), d.cpu, d.us);
        for c in 0..KCPU {
            if d.n[c] == 0 { continue; }
            crate::print!("stat:     core {}:", c);
            for e in &d.ev[c][..d.n[c] as usize] {
                let dt = e.tsc.wrapping_sub(d.t0);
                let (sg, u) = if dt < (1u64 << 62) { ("+", dt.saturating_mul(1_000_000) / hzr) } else { ("-", d.t0.wrapping_sub(e.tsc).saturating_mul(1_000_000) / hzr) };
                crate::print!(" [{}{} {} t{}->t{} {}", sg, u, if e.why & 0x80 != 0 { "tick" } else { "yield" }, e.from, e.to, why_name(e.why));
                if e.hint != 255 { crate::print!(" h:t{}", e.hint); }
                if e.why & 0x80 == 0 && e.rip != 0 { crate::print!(" from:{}", crate::sched::YO_NAMES[(e.rip as usize).min(crate::sched::YO_NAMES.len() - 1)]); }
                crate::print!("]");
            }
            crate::println!();
        }
        // Printed: the slot goes back for a later interval (the next block shows newer ones).
        d.used.store(false, Ordering::Relaxed);
        d.claim.store(false, Ordering::Release);
    }
}

fn why_name(w: u8) -> &'static str {
    match w & 0x7f { 1 => "hint", 2 => "hint-rejected", 3 => "rotation", 4 => "stay:lock", 5 => "stay:nobody",
        6 => "stay:taken", 7 => "steal", 8 => "stay:ran-little", 9 => "stay:request", _ => "?" }
}
