//! The layout of the threads over the cores: pinning by the manifest, dedicated cores, the spreading
//! of the unpinned threads over the common pool and the levelling of the load with a minimum
//! of migrations. The policy is here; the switching mechanics are in `sched.rs`.
//!
//! Three things the whole policy consists of:
//!
//! 1. **A rule is more important than a number.** The manifest says `pin=rx*:dedicated` rather than
//!    "core 3": a core's number is a property of the machine rather than of the workload, and on another
//!    machine it is a different one. The OS chooses the number itself and prints the choice into the log.
//! 2. **A dedicated core is given away for good.** There is one thread on it, the timer is not
//!    armed, there is no preemption. And that is exactly what Talaria exists for;
//!    everything else here is the servicing of the common pool that lives next to it.
//! 3. **A migration is a price rather than an instrument.** The balancer moves a NOT
//!    executing thread, not more often than one per pass and only at a skew of >= 2
//!    tasks. In a steady state there are zero migrations: the goal is an even load rather
//!    than even statistics of rearrangements.
//!
//! NUMA: the threads of one process are brought onto one node ("the process's home node")
//! while there is somewhere to put them there; when the node is overloaded relative to the machine
//! by more than `NUMA_SLACK`, a thread moves to a neighbouring one — "if possible"
//! means exactly that rather than "at any price".

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use crate::cellman::{CellManifest, CorePool, MAXPIN, MAXPOOLS, PIN_DEDICATED, PIN_SHARED, PIN_UNSET, PinRule, pin_pool_index};
use crate::percpu::MAXCPU;
use crate::sched::NTASKS;
pub use crate::placement::{World, W_FULL, POOL_NONE, pick_shared_in, pick_in_pool, pick_dedicated_in,
    node_exists, pick_node_in, imbalance_in, imbalance_in_pool, demand_after, demand_decay};
use crate::placement::{DEMAND_SHIFT, IMBALANCE_W};
const _: () = assert!(crate::placement::MAXCPU == MAXCPU, "placement.rs and percpu.rs disagree on MAXCPU");

/// How often TO LOOK AT THE SKEW — IN TIME rather than in ticks.
///
/// Previously "once every 64 BSP ticks" stood here, and that read as "once every 32 ms"
/// exactly until the tick's period was a constant. Since session 20 it has stopped
/// being one (short on a core with a network waiter, long where there is nobody
/// to preempt), and the "32 ms" came to mean from 6 to 128 ms — depending on
/// what THE CONTROL PLANE is doing at that moment rather than the workload being levelled.
/// The same error as with the scheduler's slice: a value set in time
/// cannot be measured by a counter whose frequency is its own on every machine and at every
/// moment.
const BALANCE_EVERY_NS: u64 = 32_000_000;
/// How often TO REFRESH THE DEMAND — also in time, and MUCH more often than the decisions
/// are made.
///
/// The demand is a running average of the readiness, and its time constant is set by THE NUMBER
/// of updates. While it was refreshed once per pass of the balancer, ten updates
/// accumulated over a run: the demand of an always-ready thread reached a third
/// of its limit, NEVER reached the skew's threshold, and over the whole
/// session the balancer did not move a single task (`BAL_SEEN` = 0 out of 12 passes).
/// Thirty-two updates per decision window give a reach of the limit with headroom.
const DEMAND_EVERY_NS: u64 = 1_000_000;
/// How many rearrangements are allowed per pass. One is a protection against
/// chattering, but also the speed of the spreading: seven threads from one core would spread out over
/// seven windows, that is, a quarter of a second. The skew is recomputed after every one,
/// so a batch stops by itself.
const MOVES_PER_PASS: usize = 4;
/// How long a task "cools down" after a move (a hysteresis against chattering) — in time.
const COOLDOWN_NS: u64 = 128_000_000;

/// A core's role.
pub const ROLE_OFF: u8 = 0;        // the core is outside the scheduler's pool
pub const ROLE_SHARED: u8 = 1;     // the common pool: a round robin by the tick
pub const ROLE_DEDICATED: u8 = 2;  // dedicated to a thread: run-to-completion, without a tick

static CONFIGURED: AtomicBool = AtomicBool::new(false);
static SPREAD: AtomicBool = AtomicBool::new(false);
static BALANCE: AtomicBool = AtomicBool::new(false);
static NUMA_ON: AtomicBool = AtomicBool::new(false);

/// The owner of a dedicated core: tid+1 (0 — the core is not dedicated).
static DEDICATED: [AtomicU32; MAXCPU] = [const { AtomicU32::new(0) }; MAXCPU];
/// The core is dedicated to one task (run-to-completion) — it does not take foreign work.
pub fn is_dedicated(cpu: usize) -> bool { cpu < MAXCPU && DEDICATED[cpu].load(Ordering::SeqCst) != 0 }
/// The NUMA node of a logical core (255 — unknown / a machine without a SRAT).
static NODE_OF: [AtomicU8; MAXCPU] = [const { AtomicU8::new(255) }; MAXCPU];
/// The task was placed BY A RULE — the balancer does not touch it.
static PINNED: [AtomicBool; NTASKS] = [const { AtomicBool::new(false) }; NTASKS];
/// Is the task pinned (a mask limits the set of cores — see `set_affinity`).
pub fn is_pinned(t: usize) -> bool { t < NTASKS && PINNED[t].load(Ordering::SeqCst) }
/// The tick of the task's last move (the hysteresis).
static LAST_MOVE: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS];
/// The running average of the task's READINESS: `w -= w/8; w += W_FULL/8 if the task
/// is ready to run right now`. An always-ready one settles at `W_FULL`, an always
/// sleeping one at zero, while a thread that works a quarter of the time weighs a quarter.
///
/// Why the readiness is measured rather than the consumed time. The consumed time is
/// the load OF THE CORE rather than the task's demand: a core with eight busy threads and a core
/// with one busy one are equally 100% busy, and by such a metric there is nothing
/// to level. The demand, on the other hand, adds up, and it is exactly it that must be divided equally.
static RUNW: [AtomicU32; NTASKS] = [const { AtomicU32::new(0) }; NTASKS];
/// The process's home node (by the leader); 255 — not chosen yet.
static PROC_NODE: [AtomicU8; NTASKS] = [const { AtomicU8::new(255) }; NTASKS];

// ── the named pools of cores (`pool=` in the manifest) ──────────────────────
//
// A pool is the middle between "a particular core" and "somewhere": the operator says
// WHICH cores are given to this class of threads, while laying them out inside it and levelling
// between them is the OS's work. A core's number is still named, but once and
// in one place rather than in every rule.
//
// The pools PARTITION the set of cores: a core belongs to at most one pool, while everything that
// is not named forms the common pool (`POOL_NONE`). Intersections are forbidden and
// reported: two pools sharing a core are two promises of isolation, of which
// one is false.
static POOL_MASK: [AtomicU64; MAXPOOLS] = [const { AtomicU64::new(0) }; MAXPOOLS];
static mut POOL_NAME: [CorePool; MAXPOOLS] = [CorePool::empty(); MAXPOOLS];
static NPOOL: AtomicU8 = AtomicU8::new(0);
/// The task's pool (`POOL_NONE` — the common one).
static TASK_POOL: [AtomicU8; NTASKS] = [const { AtomicU8::new(POOL_NONE) }; NTASKS];

/// The pool the core belongs to (`POOL_NONE` — the common one).
pub fn pool_of_cpu(cpu: usize) -> u8 {
    if cpu >= MAXCPU { return POOL_NONE; }
    let n = NPOOL.load(Ordering::SeqCst) as usize;
    for i in 0..n.min(MAXPOOLS) {
        if POOL_MASK[i].load(Ordering::SeqCst) & (1u64 << cpu) != 0 { return i as u8; }
    }
    POOL_NONE
}
/// The task's pool.
pub fn pool_of_task(t: usize) -> u8 { if t < NTASKS { TASK_POOL[t].load(Ordering::SeqCst) } else { POOL_NONE } }
/// The pool's name (an empty slice — the common pool or an index out of range).
fn pool_name(i: u8) -> &'static [u8] {
    if i as usize >= MAXPOOLS { return b""; }
    unsafe { (*core::ptr::addr_of!(POOL_NAME[i as usize])).name() }
}

/// How many times a thread moved between cores (the gate: "we minimise the migrations" is
/// a checkable statement rather than an intention).
pub static MIGRATIONS: AtomicU64 = AtomicU64::new(0);
/// How many times the balancer really moved a task.
pub static BALANCED: AtomicU64 = AtomicU64::new(0);
/// The requests "give me a dedicated core" that could not be satisfied.
pub static DED_DENIED: AtomicU64 = AtomicU64::new(0);

// The rules (they are written once in configure on the BSP before the workload starts).
static mut RULES: [PinRule; MAXPIN] = [PinRule::empty(); MAXPIN];
static mut NRULE: usize = 0;

pub fn note_migration() { MIGRATIONS.fetch_add(1, Ordering::Relaxed); }


/// A task's slot is reused for a new thread: a foreign name and marks must not
/// go to it by inheritance.
pub fn forget(t: usize) {
    if t >= NTASKS { return; }
    AFF[t].store(0, Ordering::SeqCst);   // the slot is reused — a foreign mask is not inherited
    PINNED[t].store(false, Ordering::SeqCst);
    LAST_MOVE[t].store(0, Ordering::SeqCst);
    PROC_NODE[t].store(255, Ordering::SeqCst);
    TASK_POOL[t].store(POOL_NONE, Ordering::SeqCst);
    for c in 0..MAXCPU {
        if DEDICATED[c].load(Ordering::SeqCst) == t as u32 + 1 { DEDICATED[c].store(0, Ordering::SeqCst); }
    }
}

/// A reset before a restart of the workload by the supervisor (the rules stay: they come from
/// the manifest rather than from the state of the crashed instance).
pub fn reset() {
    for c in 0..MAXCPU { DEDICATED[c].store(0, Ordering::SeqCst); }
    for t in 0..NTASKS {
        PINNED[t].store(false, Ordering::SeqCst);
        LAST_MOVE[t].store(0, Ordering::SeqCst);
        PROC_NODE[t].store(255, Ordering::SeqCst);
        TASK_POOL[t].store(POOL_NONE, Ordering::SeqCst);
    }
}

/// Is the core given to the common pool (a round robin with a tick)? It is asked by `sched::ap_loop`
/// before the entry into a task — the answer decides whether there will be a timer on the core.
/// While the layout is not set, the behaviour is the former one: an AP is a dedicated core.
pub fn shared_core(cpu: usize) -> bool { role(cpu) == ROLE_SHARED }

pub fn role(cpu: usize) -> u8 {
    if cpu >= MAXCPU || !crate::sched::sched_cpu(cpu) { return ROLE_OFF; }
    if DEDICATED[cpu].load(Ordering::SeqCst) != 0 { return ROLE_DEDICATED; }
    if cpu == 0 { return ROLE_SHARED; } // the BSP is the control plane, always common
    if !CONFIGURED.load(Ordering::SeqCst) { return ROLE_DEDICATED; } // the legacy path
    ROLE_SHARED
}

/// The core's NUMA node (255 — unknown).
pub fn node_of(cpu: usize) -> u8 { if cpu < MAXCPU { NODE_OF[cpu].load(Ordering::SeqCst) } else { 255 } }

/// The mask of the cores on which the thread `t` IS ALLOWED to live — what the workload sees in
/// `sched_getaffinity` (and through it `nproc`, `hardware_concurrency`, the sizes
/// of the thread pools).
///
/// Previously the `ONLINE` mask was returned, and that became ambiguous exactly when
/// the dedicated cores appeared: a workload that counted the number of available cores by it
/// included in the count the cores given TO IT ITSELF for the latency-critical threads — and
/// started a pool of workers on them, that is, broke its own isolation. Plus
/// `ONLINE` includes the cores in self-test mode, which do not take tasks at all.
///
/// Now these are the cores OF THE scheduler's POOL minus the foreign dedicated ones. One's own dedicated
/// core stays in the mask: a thread must see the core it lives on, otherwise
/// `sched_getaffinity` would contradict `sched_getcpu`.
/// How many cores the workload sees (the number of ones in the current task's `allowed_mask`).
/// By this number the applications lay out their thread pools — and they take it both from
/// `sched_getaffinity` and from `/proc/cpuinfo`, so both answers must
/// coincide.
pub fn online_count() -> usize {
    let t = crate::sched::current_id();
    let m = allowed_mask(t);
    if m == 0 { 1 } else { m.count_ones() as usize }
}

/// The mask the thread requested itself (`sched_setaffinity`). 0 — it did not request one.
///
/// Previously it simply did not exist: the syscall remembered the fact "the thread is pinned" and moved
/// it onto the mask's first core, while `sched_getaffinity` kept returning ALL the online ones.
/// For an application that is a lie in the most inconvenient direction: a thread pool is laid out
/// by that mask, and a workload seated on two cores starts a pool on all
/// four. Now the mask is stored and takes part both in the answer and in the layout.
static AFF: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS];

/// How many threads have limited themselves with a mask (pinned at their own request) — in `stat:`.
/// A pinned thread is invisible to stealing and to the balancer, and without this number
/// "who pinned themselves and to what" has to be read out of the steal's rejection counter.
pub static SELFPIN: AtomicU64 = AtomicU64::new(0);
/// Remember the requested mask. false — there is not a single online core in the mask
/// (Linux in that case answers EINVAL, and that is the right answer: a silent
/// agreement would mean that the thread works not where it was asked to).
pub fn set_affinity(t: usize, mask: u64) -> bool {
    if t >= NTASKS { return false; }
    let online = crate::percpu::ONLINE.load(Ordering::SeqCst);
    if mask & online == 0 { return false; }
    AFF[t].store(mask & online, Ordering::SeqCst);
    // A FULL MASK IS NOT A PINNING BUT THE ABSENCE OF A LIMITATION.
    //
    // `PINNED` means "placed BY A RULE, it must not be moved": it is touched neither by the
    // balancer, nor by stealing, nor by the transplant at a wakeup. Setting it at
    // ANY `sched_setaffinity` means freezing on a core a thread that
    // has just said "I may go anywhere".
    //
    // And that is how ClickHouse's fourth core was lost. The counter of the rejection of steal
    // candidates named that with one number: 87 916 refusals "it is pinned" against 24 300
    // "it is executing" and 213 "the donor has no queue" — that is, stealing was not looking for
    // work, it simply had nothing to take. The runtime calls `sched_setaffinity` with
    // a full mask (a typical trick: "learn/confirm the set of cores"), and after
    // the very first such call the whole thread pool became immobile.
    //
    // A real limitation (a mask that is ALREADY narrower than the online one) still both pins and
    // is respected: `allowed_mask` is checked at a steal separately.
    let constrains = mask & online != online;
    if constrains { PINNED[t].store(true, Ordering::SeqCst); SELFPIN.fetch_add(1, Ordering::Relaxed); }
    true
}

pub fn allowed_mask(t: usize) -> u64 {
    let online = crate::percpu::ONLINE.load(Ordering::SeqCst);
    // The thread's own request works ALWAYS — even when there is no layout plan.
    // (Previously there was an early return here, and `sched_getaffinity` returned all
    // the machine's cores even right after a `sched_setaffinity` onto one.)
    let own = AFF[t.min(NTASKS - 1)].load(Ordering::SeqCst);
    if !CONFIGURED.load(Ordering::SeqCst) {
        return if own != 0 && own & online != 0 { own & online } else { online };
    }
    // A thread given into a named pool sees EXACTLY its cores: that is the same answer
    // as `sched_getaffinity` gives in Linux under a cpuset, and by it the workload will measure
    // itself a thread pool of the needed size.
    let p = pool_of_task(t);
    if p != POOL_NONE && (p as usize) < MAXPOOLS {
        let m = POOL_MASK[p as usize].load(Ordering::SeqCst) & crate::sched::sched_cpus();
        if m != 0 { return m; }
    }
    let mut m = crate::sched::sched_cpus();
    for c in 0..MAXCPU {
        let owner = DEDICATED[c].load(Ordering::SeqCst);
        if owner != 0 && owner != t as u32 + 1 { m &= !(1u64 << c); }
    }
    let m = if m == 0 { online } else { m };
    if own != 0 && own & m != 0 { own & m } else { m }
}

// ── the configuration by the manifest ───────────────────────────────────────

/// Accept the layout from the manifest and print THE PLAN — what exactly the OS is going
/// to do. The plan in the log costs one line per rule and removes a whole class
/// of questions "why is the thread not there": both the rule and the fact that it has been parsed are visible.
pub fn configure(m: &CellManifest) {
    unsafe {
        NRULE = m.pinc.min(MAXPIN);
        for i in 0..NRULE { RULES[i] = *m.pin(i); }
    }
    // The pools: the cores the machine does not have are discarded; so are the intersections, and about
    // all of it we speak. A pool half made of non-existent cores would look
    // as if it worked and would silently give half as many cores as it promised the operator.
    let np = m.poolc.min(MAXPOOLS);
    let cpus = crate::sched::sched_cpus();
    let mut taken = 0u64;
    for i in 0..np {
        let p = m.pool(i);
        let want = p.mask;
        let mut got = want & cpus & !taken;
        if got != want {
            let lost = want & !cpus;
            let clash = want & taken;
            if lost != 0 { crate::println!("sched-plan: ATTENTION — the pool has cores the machine does not have (the mask {:#x}) — they are discarded", lost);
                }
            if clash != 0 { crate::println!("sched-plan: ATTENTION — the cores {:#x} are already given to another pool — they are discarded (the pools do not \
                intersect)", clash); }
        }
        if got == 0 { crate::println!("sched-plan: ATTENTION — the pool has stayed EMPTY, its threads will go into the common pool"); got = 0; }
        taken |= got;
        POOL_MASK[i].store(got, Ordering::SeqCst);
        unsafe { POOL_NAME[i] = *p; }
    }
    NPOOL.store(np as u8, Ordering::SeqCst);
    // HUGE PAGES ARE A PROPERTY OF THE WORKLOAD, AND IT IS THE ONE THAT ASKS FOR THEM.
    //
    // The threshold lives in the machine (one address space of the workload per cell), but it
    // is DECLARED in the manifest — together with the binary rather than in the stand's
    // command line. The difference is not cosmetic: the command line belongs to the operator
    // and to the stand, while the manifest travels with the workload. To analytics with a random access over
    // megabytes huge pages give tens of percent, to a latency workload they
    // cost one and a half microseconds of p50 — and it is the workload itself that knows about that.
    // The same goes for the other six measured settings (slice/idleslice/netslice/
    // wakepreempt/fscache/readahead/netyield): every one of them is a property of the workload
    // chosen BY MEASUREMENT rather than of the stand. They are applied by one function
    // so that the place of the application is exactly one for all seven.
    crate::cellman::apply_tuning(m);
    SPREAD.store(m.spread, Ordering::SeqCst);
    BALANCE.store(m.balance, Ordering::SeqCst);
    NUMA_ON.store(m.numa, Ordering::SeqCst);
    IRQ_CORE.store(if m.irq >= 0 { (m.irq as usize).min(254) as u8 } else { 255 }, Ordering::SeqCst);
    WANT_NODE.store(if m.numa_node >= 0 { (m.numa_node as usize).min(254) as u8 } else { 255 }, Ordering::SeqCst);
    CONFIGURED.store(true, Ordering::SeqCst);
    numa_init();
    if m.pin_bad != 0 {
        crate::println!("sched-plan: ATTENTION — {} lines of pin= were not parsed and are NOT APPLIED (we expect <pattern>:<number|dedicated|shared>)",
            m.pin_bad);
    }
    crate::println!("sched-plan: rules {}, spreading={} balancing={} numa={}, the interrupts per core {}, the scheduler's cores {:#x}",
        m.pinc, onoff(m.spread), onoff(m.balance), onoff(m.numa), irq_core(), crate::sched::sched_cpus());
    for i in 0..np {
        crate::print!("sched-plan:   the pool ");
        puts(pool_name(i as u8));
        crate::println!(" = the cores {:#x}", POOL_MASK[i].load(Ordering::SeqCst));
    }
    for i in 0..m.pinc.min(MAXPIN) {
        let r = m.pin(i);
        crate::print!("sched-plan:   ");
        print_rule(r);
    }
    dump_nodes();
    apply_llc(m);
}

/// The shares of the common L3 (`llc=`) come right after the layout over the cores. A group of cores has already
/// been named by the operator, what remains is to translate the name into a mask and give it to CAT. The cache has NO
/// nomenclature of cores of its own on purpose: two independent numberings of one and
/// the same thing will sooner or later diverge, and they will diverge silently.
fn apply_llc(m: &CellManifest) {
    if m.llcc == 0 { return; }
    let mut rules = [(0u64, 0u32); crate::cellman::MAXLLC];
    let mut n = 0usize;
    let cpus = crate::sched::sched_cpus();
    for i in 0..m.llcc.min(crate::cellman::MAXLLC) {
        let r = m.llc(i);
        let who = r.who();
        let cores = if who == b"*" {
            cpus
        } else if !who.is_empty() && who.iter().all(|c| c.is_ascii_digit()) {
            let mut v = 0u32;
            for &c in who { v = v.wrapping_mul(10) + (c - b'0') as u32; }
            if v >= 64 { 0 } else { 1u64 << v }
        } else if let Some(pi) = m.pool_index(who) {
            POOL_MASK[pi].load(Ordering::SeqCst)
        } else {
            crate::print!("sched-plan: ATTENTION — llc= refers to an unknown group of cores '");
            puts(who);
            crate::println!("' — the rule is DISCARDED");
            continue;
        };
        if cores == 0 {
            crate::print!("sched-plan: ATTENTION — the group of cores '");
            puts(who);
            crate::println!("' is empty, it does not need a share of the cache — the rule is DISCARDED");
            continue;
        }
        rules[n] = (cores, r.mask);
        n += 1;
    }
    crate::cat::apply(&rules[..n]);
    crate::cat::apply_local_if_changed(0); // the control plane — here and now
}

fn onoff(v: bool) -> &'static str { if v { "on" } else { "off" } }

// ── the same thing from the command line ────────────────────────────────────
//
// `talaria.pin=main:1,rx*:dedicated` and `talaria.sched=nospread,nobalance,nonuma`
// are the same rules without a disk. They are needed for the same reason as `talaria.ext2=dxlimit`:
// so that a behaviour that is otherwise checked only by a full production image has
// a home gate. It is parsed early (the command line is read before everything), while it
// IS APPLIED in `sched::install` — there the cores are already in the pool, and the layout has something
// to look at. The manifest is senior: if it was there, the command line stays silent.
static mut CMDPLAN: CellManifest = CellManifest::empty();
static HAS_CMD: AtomicBool = AtomicBool::new(false);

/// `talaria.pin=<pattern>:<target>[,<pattern>:<target>...]`
pub fn stash_pins(list: &str) {
    unsafe {
        let p = &mut *core::ptr::addr_of_mut!(CMDPLAN);
        for spec in list.split(',') { p.add_pin_spec(spec.as_bytes()); }
    }
    HAS_CMD.store(true, Ordering::SeqCst);
}

/// `talaria.pool=<name>:<a list of cores>` — one declaration per token
/// (the command line is cut by spaces, so the commas inside the list of cores
/// stay commas: `talaria.pool=fast:2,3 talaria.pool=bg:4-7`).
pub fn stash_pool(spec: &str) {
    unsafe {
        let p = &mut *core::ptr::addr_of_mut!(CMDPLAN);
        p.add_pool_spec(spec.as_bytes());
    }
    HAS_CMD.store(true, Ordering::SeqCst);
}

/// `talaria.sched=spread|nospread|balance|nobalance|numa|nonuma`
pub fn stash_flags(list: &str) {
    unsafe {
        let p = &mut *core::ptr::addr_of_mut!(CMDPLAN);
        for f in list.split(',') {
            match f {
                "spread" => p.spread = true, "nospread" => p.spread = false,
                "balance" => p.balance = true, "nobalance" => p.balance = false,
                "numa" => p.numa = true, "nonuma" => p.numa = false,
                _ if f.starts_with("node=") => {
                    p.numa = true;
                    p.numa_node = f[5..].parse::<i32>().unwrap_or(PIN_UNSET);
                }
                _ if f.starts_with("irq=") => {
                    let v = &f[4..];
                    p.irq = if v == "controlplane" || v == "cp" { 0 }
                            else { v.parse::<i32>().unwrap_or(PIN_UNSET) };
                }
                _ => crate::println!("sched-plan: an unknown option talaria.sched={} — it is skipped", f),
            }
        }
    }
    HAS_CMD.store(true, Ordering::SeqCst);
}

/// Apply the deferred plan from the command line. It is called from `sched::install`:
/// the scheduler's cores are sorted out by that moment, the workload has not started yet.
pub fn apply_stashed() {
    if !HAS_CMD.load(Ordering::SeqCst) || CONFIGURED.load(Ordering::SeqCst) { return; }
    let p = unsafe { &*core::ptr::addr_of!(CMDPLAN) };
    crate::println!("sched-plan: the layout is set by the command line (there is no manifest)");
    configure(p);
}

/// Printing raw bytes (a thread's name / a pattern) through the kernel's common output channel.
fn puts(s: &[u8]) { for &b in s { crate::print!("{}", if b.is_ascii_graphic() { b as char } else { '?' }); } }

fn print_rule(r: &PinRule) {
    if r.is_main { crate::print!("main"); }
    else {
        if r.star_head() { crate::print!("*"); }
        puts(r.pattern());
        if r.star_tail() { crate::print!("*"); }
    }
    if let Some(i) = pin_pool_index(r.target) {
        crate::print!(" -> the pool ");
        puts(pool_name(i as u8));
        crate::println!(" (the cores {:#x})", POOL_MASK[i.min(MAXPOOLS - 1)].load(Ordering::SeqCst));
        return;
    }
    match r.target {
        PIN_DEDICATED => crate::println!(" -> a dedicated core (the number is chosen by the OS)"),
        PIN_SHARED => crate::println!(" -> the common pool"),
        n => crate::println!(" -> the core {}", n),
    }
}

/// The map "a logical core -> a NUMA node" by the SRAT. Without a SRAT (QEMU by default,
/// Nitro guests) there is one node and that is the truth rather than a stub: the machine really has one.
fn numa_init() {
    let n = crate::smp::nr_cpus();
    let mut nodes = 0u64;
    for c in 0..n.min(MAXCPU) {
        let nd = crate::acpi::node_of_apic(crate::smp::apic_of(c));
        NODE_OF[c].store(nd, Ordering::SeqCst);
        if nd != 255 { nodes |= 1 << (nd & 63); }
    }
    if nodes == 0 { for c in 0..n.min(MAXCPU) { NODE_OF[c].store(0, Ordering::SeqCst); } }
}

fn dump_nodes() {
    let mut seen = 0u64;
    for c in 0..MAXCPU { let nd = NODE_OF[c].load(Ordering::SeqCst); if nd != 255 { seen |= 1 << (nd & 63); } }
    crate::println!("sched-plan: the NUMA nodes {} (the mask {:#x}), the source is the SRAT",
        seen.count_ones(), seen);
}

// ── a snapshot of the world: what the policy computes over ──────────────────

/// Take a snapshot from the live scheduler.
pub fn snapshot() -> World { snapshot_for(usize::MAX) }

/// A snapshot for THE PLACEMENT of the task `me`: it itself is subtracted from the cores' load.
///
/// Otherwise a new thread counts itself as the load. A task is created with `CPU_OF=0`,
/// that is, on the control plane, and at the moment of the core's choice it looks one
/// task busier than it is. It was observed exactly like that: eight threads over four cores
/// landed 1/3/2/2 instead of 2/2/2/2 — a layout that is "almost right", and that is the worst
/// kind of wrong.
pub fn snapshot_for(me: usize) -> World {
    let mut w = World::empty();
    w.pool = crate::sched::sched_cpus();
    let n = crate::sched::task_count().min(NTASKS);
    let my_cpu = if me < NTASKS { crate::sched::cpu_of(me) } else { usize::MAX };
    for c in 0..MAXCPU {
        w.load[c] = crate::sched::tasks_on(c).min(255) as u8;
        if c == my_cpu { w.load[c] = w.load[c].saturating_sub(1); }
        w.ded[c] = DEDICATED[c].load(Ordering::SeqCst) != 0;
        w.node[c] = NODE_OF[c].load(Ordering::SeqCst);
        w.cpool[c] = pool_of_cpu(c);
    }
    for t in 0..n {
        if !crate::sched::task_alive(t) || t == me { continue; }
        let c = crate::sched::cpu_of(t);
        if c >= MAXCPU { continue; }
        if PINNED[t].load(Ordering::SeqCst) { w.pinned[c] = w.pinned[c].saturating_add(1); }
        w.weight[c] = w.weight[c].saturating_add(RUNW[t].load(Ordering::SeqCst));
    }
    w
}

/// THE DEMAND BY CORE in one pass (the sum of the `RUNW` of the core's live tasks).
///
/// The same as `weight` in `snapshot_for`, but without the rest of the snapshot: stealing needs
/// only this array, while a snapshot costs three more passes and a read of the layout.
pub fn demand_counts(out: &mut [u32; MAXCPU]) {
    *out = [0; MAXCPU];
    let n = crate::sched::task_count().min(NTASKS);
    for t in 0..n {
        if !crate::sched::task_alive(t) { continue; }
        let c = crate::sched::cpu_of(t);
        if c >= MAXCPU { continue; }
        out[c] = out[c].saturating_add(RUNW[t].load(Ordering::Relaxed));
    }
}

/// Refresh the tasks' demand (the running average of the readiness).
///
/// It is called MUCH more often than the decisions are made (see `DEMAND_EVERY_NS`):
/// the filter's time constant is set by the number of updates, and polling it with
/// the same frequency with which it is read means measuring nothing.
fn refresh_demand() {
    let n = crate::sched::task_count().min(NTASKS);
    for t in 0..n {
        if !crate::sched::task_alive(t) { RUNW[t].store(0, Ordering::Relaxed); continue; }
        let w = RUNW[t].load(Ordering::Relaxed);
        let ready = crate::sched::is_running(t) || crate::sync::runnable_peek(t);
        RUNW[t].store(w - (w >> DEMAND_SHIFT) + if ready { W_FULL >> DEMAND_SHIFT } else { 0 },
                      Ordering::Relaxed);
    }
}

/// How many updates of the demand there are per one decision of the balancer.
pub fn demand_per_pass() -> u32 { (BALANCE_EVERY_NS / DEMAND_EVERY_NS) as u32 }
/// The skew's threshold — for the gate.
pub fn imbalance_threshold() -> u32 { IMBALANCE_W }

/// The core for a new thread when there is NO layout plan.
///
/// The key is the pair (the demand, the number of tasks). The demand is more important: a core on which
/// fifteen sleeping threads of a runtime hang is freer than a core with one computing one. But
/// a just-created thread's demand is not measured yet (zero), and a batch of threads
/// created in a volley would all land on one core — so at an equal demand
/// the number of tasks decides.
fn pick_default(w: &World) -> usize {
    let mut best = usize::MAX;
    let mut best_key = (usize::MAX, usize::MAX);
    for c in 0..MAXCPU {
        if !w.in_pool(c, POOL_NONE) { continue; }
        // The first key is the READY tasks rather than all the live ones: fifteen threads
        // of a runtime sleeping on a futex occupy the core with nothing, and counting them
        // as load means driving a new thread to where it is in fact more crowded.
        let key = (crate::sched::runnable_on(c), w.load[c] as usize);
        if key < best_key { best_key = key; best = c; }
    }
    if best == usize::MAX { 0 } else { best }
}

// ── the application of the policy to the live tasks ─────────────────────────

/// The node chosen for THE WORKLOAD that is about to start. It is needed earlier than
/// its first task appears: the process's stack and heap are allocated BEFORE the `spawn`, and
/// putting them on one node and the threads on another means paying for locality and
/// not getting it. 255 — not chosen (a machine with one node or there is no layout).
static LAUNCH_NODE: AtomicU8 = AtomicU8::new(255);
/// The node NAMED by the operator (`numa=<N>`); 255 — the OS chooses. The operator knows
/// which socket the card and the disk are attached to — the OS does not see that through ACPI.
static WANT_NODE: AtomicU8 = AtomicU8::new(255);

/// Choose a node for the starting workload and return it. It is called from the launch path
/// BEFORE its memory is allocated; after that the same node goes to its threads.
pub fn pick_launch_node() -> u8 {
    if !CONFIGURED.load(Ordering::SeqCst) || !NUMA_ON.load(Ordering::SeqCst) { return 255; }
    let want = WANT_NODE.load(Ordering::SeqCst);
    let w = snapshot();
    // A named node is applied only if the machine HAS it: otherwise the workload
    // would silently move "into nowhere", while such a node would look as if it worked.
    let nd = if want != 255 && node_exists(&w, want) { want } else {
        if want != 255 {
            crate::println!("sched-plan: ATTENTION — the machine has no NUMA node {}, the node is chosen by the OS", want);
        }
        pick_node_in(&w)
    };
    LAUNCH_NODE.store(nd, Ordering::SeqCst);
    if nd != 255 {
        crate::println!("sched-plan: the workload -> the NUMA node {} (the memory and the threads on one node)", nd);
        // The FS block cache was allocated at the mounting — earlier than the node became
        // known (the manifest lies on the same disk). We take it onto the workload's
        // node while only the reading of the manifest has gone through it.
        crate::ext2::cache_relocate(nd);
    }
    nd
}
/// The starting workload's node (255 — not chosen).
pub fn launch_node() -> u8 { LAUNCH_NODE.load(Ordering::SeqCst) }

/// The node to take memory from for the task `t`: the home node of its process,
/// otherwise the node of the started workload. 255 — it does not matter (a machine with one node,
/// no layout is set or `numa=off`).
///
/// It is asked from the memory syscalls (`mmap`, COW): a thread brought onto a node
/// must get its frames from there too — otherwise the locality is paid for and not
/// received.
pub fn node_of_task(t: usize) -> u8 {
    if !CONFIGURED.load(Ordering::SeqCst) || !NUMA_ON.load(Ordering::SeqCst) { return 255; }
    let leader = crate::sched::proc_of(t).min(NTASKS - 1);
    let nd = PROC_NODE[leader].load(Ordering::SeqCst);
    if nd != 255 { nd } else { LAUNCH_NODE.load(Ordering::SeqCst) }
}
/// The memory node for THE CURRENT task (a short form for the syscalls).
pub fn my_node() -> u8 { node_of_task(crate::sched::current_id()) }

/// The home node of the leader process (it is chosen at the first access).
fn proc_home(leader: usize, w: &World) -> u8 {
    if !NUMA_ON.load(Ordering::SeqCst) { return 255; }
    let cur = PROC_NODE[leader.min(NTASKS - 1)].load(Ordering::SeqCst);
    if cur != 255 { return cur; }
    // The node where the workload's MEMORY already lies is senior to a fresh choice: otherwise
    // the threads would move to one node while their heap would stay on another.
    let nd = { let l = LAUNCH_NODE.load(Ordering::SeqCst); if l != 255 { l } else { pick_node_in(w) } };
    PROC_NODE[leader.min(NTASKS - 1)].store(nd, Ordering::SeqCst);
    nd
}

/// The core the devices' interrupts are directed to (`irq=` in the manifest).
/// 255 — not set.
static IRQ_CORE: AtomicU8 = AtomicU8::new(255);

/// Where to send the devices' interrupts. By default it is the control plane
/// (core 0): that is exactly what it has always been, and that is right in essence too.
///
/// A dedicated core is forbidden here, and not "just in case": a device's
/// interrupt on the core of a latency-critical thread is jitter that comes not from
/// us and not by our schedule, that is, the cancellation of the whole idea of a dedicated core.
/// A rule that asks for exactly that is rejected LOUDLY.
pub fn irq_core() -> usize {
    let want = IRQ_CORE.load(Ordering::SeqCst) as usize;
    if want == 255 || !crate::sched::sched_cpu(want) { return 0; }
    if DEDICATED[want].load(Ordering::SeqCst) != 0 {
        crate::println!("sched-plan: ATTENTION — the core {} is dedicated to a thread, the devices' interrupts are NOT directed there (they stay on the \
            control plane)", want);
        return 0;
    }
    want
}

/// Direct a device's interrupts to the core `irq_core()` and say so into the log.
/// It is called by a driver right after the device is brought up.
///
/// MSI-X if there is one; otherwise MSI. The difference matters not for us but for the cards:
/// MSI-X appeared in PCIe 3.0, and the FPGA shells (Xilinx's XDMA), the old 10G ones and
/// custom functions all over the place declare ONLY MSI. Such a card previously
/// came up silently and just as silently delivered not a single interrupt. A card with
/// INTx alone will not work now either — but that is said out loud rather than
/// found out on a stand.
pub fn route_device_irqs(d: &crate::pci::PciDev, what: &str) {
    let cpu = irq_core();
    let kind = crate::pci::intr_kind(d);
    let apic = if d.msix_cap != 0 {
        crate::pci::msix_route(d, cpu)
    } else if d.msi_cap != 0 {
        // The vector is the same row of stubs with an EOI as with MSI-X (idt: MSI_VECTOR_BASE..).
        let a = crate::pci::msi_enable(d, cpu, crate::idt::MSI_VECTOR_BASE as u8);
        // A read-back: with a function behind a bridge that does not forward it, a write into the
        // config goes into nowhere, and "we configured it" would look like "it works".
        let (addr, data, on) = crate::pci::msi_readback(d);
        let want = crate::arch::msi::expect_addr(cpu);
        if !on || addr != want || data != crate::arch::msi::expect_data(crate::idt::MSI_VECTOR_BASE as u16) {
            crate::println!("sched-plan: ATTENTION — the MSI of {} was not accepted by the device (the address {:#x}, we expected {:#x}, the data {:#x}, \
                enabled {})",
                what, addr, want, data, on);
            return;
        }
        a
    } else {
        crate::println!("sched-plan: ATTENTION — {} has only INTx: the kernel does not route it, there will be NO interrupts from the card (the datapath will \
            stay polling)", what);
        return;
    };
    if apic == u32::MAX { return; }
    crate::println!("sched-plan: the interrupts of {} ({}) -> the core {} (apic {}), the core's role: {}",
        what, kind, cpu, apic,
        match role(cpu) { ROLE_DEDICATED => "dedicated", ROLE_OFF => "outside the pool", _ => "the common pool" });
}

/// A self-check of the interrupt routing (`talaria.test=msix`).
///
/// THE MECHANICS are checked: the MSI-X message address is written into the device's table and
/// IS READ BACK — "we wrote it" and "the device will see it" are different statements, and
/// what must be checked is the second one. Plus the rule "we do not give the interrupts to a dedicated core".
/// A live card (ENA) stays with EC2; here there is what can be checked at home, and
/// that is exactly that class of error where a wrong address means an interrupt quietly
/// arriving at a foreign core.
pub fn selftest_msix() -> ! {
    let mut fail = 0usize;
    let mut check = |cond: bool, what: &str| {
        if cond { crate::println!("msix: OK   {}", what); }
        else { crate::println!("msix: FAILURE {}", what); fail += 1; }
    };
    // Any device with an MSI-X table: under QEMU that is NVMe or virtio-*-pci.
    let mut found = None;
    for i in 0..crate::pci::ndev() {
        if let Some(d) = crate::pci::dev_at(i) {
            if d.msix_cap != 0 && crate::pci::msix_msg_addr(&d, 0) != u32::MAX { found = Some(d); break; }
        }
    }
    let d = match found {
        Some(d) => d,
        None => { crate::println!("msix: no devices with an MSI-X table were found — the check is impossible"); crate::suite::park() }
    };
    crate::println!("msix: the device {:02x}:{:02x}.{} {:04x}:{:04x}, the MSI-X table is mapped",
        d.bus, d.dev, d.func, d.vendor, d.device);
    crate::pci::msix_enable_masked(&d);

    // 1. A route to core 0 (the control plane) — the address must become the BSP's APIC id.
    let a0 = crate::pci::msix_route(&d, 0);
    let want0 = crate::arch::msi::expect_addr(0);
    let got0 = crate::pci::msix_msg_addr(&d, 0);
    crate::println!("msix: the route to core 0: the address {:#x} (we expected {:#x}), apic {}", got0, want0, a0);
    check(got0 == want0, "the message's address is read back and is the one a message to core 0 carries");

    // 2. A route to another core changes EXACTLY the APIC id field rather than something else.
    let target = if crate::smp::nr_cpus() > 1 { 1 } else { 0 };
    let a1 = crate::pci::msix_route(&d, target);
    let want1 = crate::arch::msi::expect_addr(target);
    let got1 = crate::pci::msix_msg_addr(&d, 0);
    crate::println!("msix: the route to core {}: the address {:#x} (we expected {:#x}), apic {}", target, got1, want1, a1);
    if crate::arch::msi::ADDRESS_NAMES_CORE {
        check(got1 == want1, "the redirection changes the APIC id field in the message's address");
        check(got1 & 0xFFF0_0000 == 0xFEE0_0000, "the address's upper part has stayed the LAPIC's window");
    } else {
        // arm64: every message goes to the ITS's translation register; the core is the
        // ITS's business (`talaria.test=its` fires a vector and counts where it arrives).
        check(got1 == want1 && got1 == got0, "the redirection leaves the address alone: the route is a command to the ITS");
        check(a1 != u32::MAX, "the ITS accepted the re-route of every vector");
    }

    // 3. All the table's vectors have been moved rather than only the zeroth one.
    let mut all = true;
    for v in 0..4u16 { if crate::pci::msix_msg_addr(&d, v) != want1 { all = false; } }
    check(all, "ALL the table's vectors have been moved rather than the first one");

    // 4. The rule: the interrupts are not given to a dedicated core.
    IRQ_CORE.store(1, Ordering::SeqCst);
    DEDICATED[1].store(42, Ordering::SeqCst); // core 1 is "occupied" by a thread
    crate::sched::SCHED_CPUS.fetch_or(0b10, Ordering::SeqCst);
    check(irq_core() == 0, "the interrupts are NOT given to a dedicated core (we fall back to the control plane)");
    DEDICATED[1].store(0, Ordering::SeqCst);
    check(irq_core() == 1, "it has been released — the route to the named core is allowed again");
    IRQ_CORE.store(255, Ordering::SeqCst);
    check(irq_core() == 0, "by default it is the control plane");

    // 5. MSI (capability 0x05) is the second half of the same question. A card without
    //    MSI-X (an FPGA shell, an old 10G one) must get its route THAT way.
    //    We look for the device separately: under QEMU there may be none — then it is A SKIP,
    //    but the arithmetic of the capability's decomposition is checked on what was found.
    let mut msid = None;
    for i in 0..crate::pci::ndev() {
        if let Some(d) = crate::pci::dev_at(i) { if d.msi_cap != 0 { msid = Some(d); break; } }
    }
    match msid {
        None => crate::println!("msix: no devices with MSI (without MSI-X) were found — the MSI check is SKIPPED"),
        Some(d) => {
            crate::println!("msix: the MSI of {:02x}:{:02x}.{} {:04x}:{:04x} (cap@{:#x})",
                d.bus, d.dev, d.func, d.vendor, d.device, d.msi_cap);
            let a = crate::pci::msi_enable(&d, 0, crate::idt::MSI_VECTOR_BASE as u8);
            let (addr, data, on) = crate::pci::msi_readback(&d);
            let want = crate::arch::msi::expect_addr(0);
            crate::println!("msix: the MSI is read back: the address {:#x} (we expected {:#x}), the data {:#x}, enabled {}, apic {}",
                addr, want, data, on, a);
            check(addr == want, "MSI: the message's address is written and read back");
            check(data == crate::arch::msi::expect_data(crate::idt::MSI_VECTOR_BASE as u16), "MSI: the message's data is written and read back");
            check(on, "MSI: the Enable bit is raised");
        }
    }

    if fail == 0 { crate::println!("msix: ALL CHECKS PASSED"); }
    else { crate::println!("msix: THERE ARE FAILURES ({})", fail); }
    crate::suite::park()
}

/// The dedicated core the thread already owns.
fn owned_core(t: usize) -> Option<usize> {
    for c in 0..MAXCPU { if DEDICATED[c].load(Ordering::SeqCst) == t as u32 + 1 { return Some(c); } }
    None
}

/// Where to put the thread t by the rule's target. It returns the core's number.
fn resolve(target: i32, t: usize, w: &World) -> usize {
    let leader = crate::sched::proc_of(t);
    let home = proc_home(leader, w);
    // The target is a named pool: the thread gets a core FROM IT and stays in it
    // forever (the balancer moves it only inside the pool).
    if let Some(i) = pin_pool_index(target) {
        if t < NTASKS { TASK_POOL[t].store(i.min(MAXPOOLS - 1) as u8, Ordering::SeqCst); }
        return pick_in_pool(w, home, i.min(MAXPOOLS - 1) as u8);
    }
    match target {
        // A thread that already owns a dedicated core does not get a second one: some
        // runtimes call `PR_SET_NAME` more than once, and every such call
        // would give it a new core — the pool would be pulled apart into threads silently.
        PIN_DEDICATED if owned_core(t).is_some() => owned_core(t).unwrap(),
        PIN_DEDICATED => match pick_dedicated_in(w, home) {
            Some(c) => {
                // The core MUST be marked dedicated first of all: from this second on
                // the spreading bypasses it, and the resettlement does not race with new threads.
                DEDICATED[c].store(t as u32 + 1, Ordering::SeqCst);
                evacuate(c, t);
                c
            }
            None => {
                // A silent success is worse than a refusal (the pitfall of session 10): a thread that did not
                // get its own core must be visible in the log — otherwise
                // "the latency has grown" is looked for afterwards in the workload itself.
                DED_DENIED.fetch_add(1, Ordering::Relaxed);
                crate::println!("sched-plan: there is NO DEDICATED core (the scheduler's cores {:#x} are occupied) — the thread {} goes into the common pool",
                    w.pool, t);
                pick_shared_in(w, home)
            }
        },
        PIN_SHARED => pick_shared_in(w, home),
        n if n >= 0 => {
            let c = n as usize;
            if !w.has(c) {
                crate::println!("sched-plan: ATTENTION — the core {} is not among the scheduler's cores ({:#x}); the thread {} stays on the control plane", c,
                    w.pool, t);
                return 0;
            }
            c
        }
        _ => crate::sched::cpu_of(t),
    }
}

/// Resettle everyone from the core `c` except `owner`: the core goes to a dedicated one.
/// It returns how many threads stayed (they could not be moved right now —
/// they will be picked up by `balance_tick` when they stop executing).
fn evacuate(c: usize, owner: usize) -> usize {
    let n = crate::sched::task_count().min(NTASKS);
    let mut left = 0;
    for t in 0..n {
        if t == owner || crate::sched::cpu_of(t) != c || !crate::sched::task_alive(t) { continue; }
        let w = snapshot_for(t);
        let dst = pick_in_pool(&w, node_of(c), pool_of_task(t));
        if dst == c || !crate::sched::migrate_idle_task(t, dst) { left += 1; continue; }
        MIGRATIONS.fetch_add(1, Ordering::Relaxed);
    }
    left
}

/// The rule for the thread with the name name (`leader` — the process's main thread).
fn find_rule(name: &[u8], leader: bool) -> Option<PinRule> {
    let n = unsafe { NRULE };
    for i in 0..n {
        let r = unsafe { RULES[i] };
        if r.matches(name, leader) { return Some(r); }
    }
    None
}

/// Place THE MAIN thread of the workload. `name` is the binary's base name (a rule may
/// be written for it too, not only for the keyword `main`).
/// It returns true if the thread has moved off the control plane — then the BSP goes
/// idle (`sched::start_idle`) rather than into a task.
pub fn place_main(t: usize, name: &[u8]) -> bool {
    if !CONFIGURED.load(Ordering::SeqCst) { return false; }
    crate::sched::set_comm(t, name);
    let r = match find_rule(name, true) { Some(r) => r, None => return false };
    if r.target == PIN_UNSET { return false; }
    let w = snapshot_for(t);
    let cpu = resolve(r.target, t, &w);
    PINNED[t].store(true, Ordering::SeqCst);
    if cpu == crate::sched::cpu_of(t) { return false; }
    // We print BEFORE handing the task over: from the next line on the thread is already running on another
    // core and writes into the same serial. The output's lock is held for one write, so
    // a line made of two `print!`s arrives torn in half by foreign text.
    crate::println!("sched-plan: the main thread -> the core {} (the node {})", cpu, node_of(cpu));
    crate::sched::set_cpu_of(t, cpu);
    if cpu != 0 { crate::sched::ap_ring(cpu); }
    cpu != 0
}

/// Place a NEW thread (`clone`) — it has no name yet, so the decision
/// is made by the cores' load and the process's home node. When the thread names
/// itself (`prctl`), a rule may move it — but already by address, with one
/// migration, rather than "first everywhere, then we will see".
pub fn place_new_thread(t: usize, creator: usize) {
    if t >= NTASKS { return; }
    if !CONFIGURED.load(Ordering::SeqCst) {
        // There is no plan — but "leave it where it was created" is a decision too, and a bad one:
        // a runtime creates threads in a volley from ONE thread, and the whole volley settles on its
        // core. The balancer resettles one task per pass (32 ms), while
        // the workload's working phase is shorter: with the JVM four threads on four cores
        // gave a speedup of now 3.9x, now 2.3x — exactly by whether the spreading managed in time or
        // not. So we put it straight onto the least loaded core.
        let w = snapshot_for(t);
        let cpu = pick_default(&w);
        if cpu != crate::sched::cpu_of(t) {
            crate::sched::set_cpu_of(t, cpu);
            if cpu != 0 { crate::sched::ap_ring(cpu); }
        }
        return;
    }
    if !SPREAD.load(Ordering::SeqCst) { return; }
    let w = snapshot_for(t);
    // The home node is inherited from the creator: the threads of one process travel together.
    let leader = crate::sched::proc_of(creator);
    let home = proc_home(leader, &w);
    // The pool is inherited too: the workers spawned by a pool's thread belong to it as well
    // until they name themselves and fall under another rule. Otherwise the pool would leak
    // outside exactly where the workload starts a thread pool of its own.
    let mut pool = pool_of_task(creator);
    // The thread is still nameless, but a rule by the PATTERN `*` applies to it too: it
    // describes not a name but "all the rest". Without that `pin=*:bg` would not apply
    // to a workload that does not name its threads — that is, to the majority, and the pool
    // would look declared and empty.
    //
    // We apply only those targets whose answer does NOT depend on what kind of
    // thread this is: a pool and a particular core. `dedicated` waits for a name — handing out
    // a dedicated core to every nameless thread means handing them all out to the workload's very first
    // thread pool.
    let mut fixed = usize::MAX;
    if pool == POOL_NONE {
        if let Some(r) = find_rule(b"", false) {
            if let Some(i) = pin_pool_index(r.target) {
                pool = i.min(MAXPOOLS - 1) as u8;
            } else if r.target >= 0 && w.has(r.target as usize) {
                fixed = r.target as usize;
            }
        }
    }
    if t < NTASKS { TASK_POOL[t].store(pool, Ordering::SeqCst); }
    let cpu = if fixed != usize::MAX { fixed } else { pick_in_pool(&w, home, pool) };
    if cpu == crate::sched::cpu_of(t) { return; }
    crate::sched::set_cpu_of(t, cpu);
    if cpu != 0 { crate::sched::ap_ring(cpu); }
}

/// The thread has named itself (`prctl(PR_SET_NAME)`). If there is a rule for the name and it
/// points at another core — return it: the caller (the syscall) migrates the thread
/// right from its own return frame, that is, without a single extra rearrangement.
pub fn on_set_name(t: usize, name: &[u8]) -> Option<usize> {
    if !CONFIGURED.load(Ordering::SeqCst) || t >= NTASKS { return None; }
    let leader = crate::sched::proc_of(t) == t;
    let r = find_rule(name, leader)?;
    if r.target == PIN_UNSET { return None; }
    let w = snapshot_for(t);
    let cpu = resolve(r.target, t, &w);
    PINNED[t].store(true, Ordering::SeqCst);
    if cpu == crate::sched::cpu_of(t) { return None; }
    Some(cpu)
}

/// Whom an idling core may TAKE for itself right now.
///
/// The periodic levelling moves one task once every 32 ms — for a thread pool
/// that gets work thrown at it in bursts that is an eternity: the threads stand in
/// a queue on their own cores while the neighbouring cores idle. ClickHouse lost three times as much on that:
/// a query in FOUR threads went slower than in one.
///
/// The rules are conservative: we take only a non-executing, non-pinned task
/// for which this core IS ALLOWED (allowed_mask), and only from a core where there is somebody
/// else — taking the only task off means simply rehanging the skew.
/// `talaria.nosteal=1` — switch work stealing off (the diagnostics: a steal carries
/// a thread onto a foreign core, and a computing thread loses its warmed cache and TLB; telling
/// "we won on the occupancy" from "we lost on the transfer" is possible only by a measurement with
/// stealing off).
pub static NOSTEAL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// THE BALANCING AT A WAKEUP: where to seat the task we are waking.
///
/// The layout puts a thread onto the least loaded core at the moment of the `clone` — and that
/// is right exactly once. A thread pool creates its threads IN ADVANCE and all at once, while it wakes
/// them afterwards several at a time per request; which ones exactly will wake up the layout could not
/// know. With ClickHouse that is visible directly: forty threads of the pool are laid out over four
/// cores evenly, while four wake up per query — and they sit two or three at a time on one
/// core while a neighbouring one idles (`stat: the ticks per core` — 1425 and 1211 against
/// 208 and 525). The periodic balancing fixes that, but once every 32 ms, while a query
/// lives 11. It is exactly this hole that Linux closes in `select_task_rq`.
///
/// HONESTLY ABOUT THE MEASUREMENT: on our suite it gives NO gain — a counter-run in
/// one window gives 290 ms with it against 284.5 without, that is, noise. It is kept
/// deliberately: the decision is right in essence (waking a thread onto a busy core when
/// a free one idles next to it is knowingly worse), it costs O(1) per wakeup,
/// and on a workload with a different ratio of "threads to cores" it must give something. The knob
/// `talaria.nowakebal=1` switches it off — so that the contribution can always be remeasured
/// counter-wise rather than argued about.
///
/// THE FIRST ATTEMPT DID NOT WORK THROUGH ITS OWN FAULT, and that is worth remembering: "the core
/// is free" was checked as "it is in the pickup loop" (`cur_task_of == MAX`). But
/// a core on which A SLEEPING task lies does not return into the pickup loop — it
/// is listed as busy although there is no work on it, and the condition almost never
/// fired. The right question is not "is the core empty" but "is there somebody to run there".
///
/// The second thing without which it is impossible: THE MARK OF A GIVEN-AWAY CORE. A pool wakes its threads in a volley, and all
/// of them see one and the same free core — without the mark a volley of four will sit
/// four at a time on one. A given-away core is excluded from the choice for a slice.
///
/// Both checks must be O(1) and without side effects: the version that asked
/// `runnable_on` per every core (a walk over all the tasks four times PER EVERY
/// wakeup, and through `blocked_for_sched` at that, which spins the skip
/// counter) was directly harmful — a `LIKE` over a table went from 12 ms to 41.
static WAKE_CLAIM: [core::sync::atomic::AtomicU64; MAXCPU] =
    [const { core::sync::atomic::AtomicU64::new(0) }; MAXCPU];
/// `talaria.nowakebal=1` — switch it off (it is needed in order to measure the contribution COUNTER-WISE).
pub static NOWAKEBAL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// The rule of the transplant of a waking thread: 1 — the former BINARY sign "there is somebody
/// to run on the core", 2 — a transfer that IMPROVES the balance (`talaria.wakebal=`).
///
/// The knob is needed for the sake of a counter-measurement WITH ONE BINARY. The previous attempt on this topic
/// compared the variants with DIFFERENT builds and different runs, while the numbers on this
/// stand diverge between runs by up to two times — one can argue about a change's contribution in
/// such conditions forever.
pub static WAKEBAL_RULE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(1);
/// How many times a waking thread was transplanted onto a free core (it is visible in `stat:`).
pub static WAKE_MOVED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Is there somebody to run on the core `c` right now (cheaply, without side effects).
///
/// A NETWORK WAITER IS NOT WORK (`talaria.wakeidle=1`). A spinner in a `poll(2)` is listed as
/// ready, and without this reservation a core with it is "busy": a woken background thread
/// stays at home and PREEMPTS a conn thread in the middle of a request (session 32, class B:
/// `[+7 a tick t13->t6 a hint h:t6][+21 a tick t6->t13 a rotation]…` — the request of t13
/// is stretched to 135-166 us). With Linux such a thread would have gone onto an idling core
/// (select_idle_sibling); our idling core is a core with a spinner, while
/// a spinner gives it up by a rotation not later than after `NETYIELD_NS`.
fn cpu_has_work(c: usize) -> bool {
    let t = crate::percpu::cur_task_of(c);
    if t == u64::MAX { return false; }                    // the pickup loop — there is no work
    let t = t as usize;
    if WAKEIDLE.load(Ordering::Relaxed) && crate::netsys::is_net_waiter(t) { return false; }
    t < NTASKS && crate::sched::task_alive(t) && crate::sync::runnable_peek(t)
}
/// `talaria.wakeidle=1` — a core on which a network waiter spins is considered
/// free for a waking thread (see `cpu_has_work`).
pub static WAKEIDLE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn wake_target(t: usize, from: usize) -> Option<usize> {
    if NOWAKEBAL.load(Ordering::Relaxed) { return None; }
    if !CONFIGURED.load(Ordering::SeqCst) || !SPREAD.load(Ordering::SeqCst) { return None; }
    if t >= NTASKS || PINNED[t].load(Ordering::SeqCst) { return None; }
    if DEDICATED[from].load(Ordering::SeqCst) != 0 { return None; }
    if !cpu_has_work(from) { return None; }               // it is free at home — we do not touch it
    let now = crate::tsc::rdtsc();
    let hz = crate::tsc::calibrate_cached().max(1);
    let claim = (hz / 1_000_000) * (crate::smp::effective_slice_ns() / 1000).max(1);
    let allow = allowed_mask(t);
    // A TRANSPLANT MUST IMPROVE THE BALANCE RATHER THAN CHANGE ITS PLACE — the same rule
    // that has already been paid for by a measurement at work stealing (see `steal_candidate`).
    //
    // The binary sign "there is somebody to run on the core" does not tell a core with one
    // ready thread from a core with three. With ClickHouse on three cores that cost
    // THE THIRD CORE entirely (84 -> 83 ms with 83 -> 57 for Linux): the pool wakes
    // its executors in a volley, nobody finds a completely empty core, and a thread
    // stays where it woke up.
    //
    // A direct replacement of the sign with "the target has FEWER ready ones" fixes three cores (58 with
    // 57 for Linux) and DOES HARM on four: with five ready ones over four cores
    // the layout 2/1/1/1 cannot be improved, while the rule still orders a transfer —
    // 2/1/1/1 becomes 1/2/1/1, and that is paid for with a cold cache.
    //
    // The condition below lets through exactly the improving transfers: after a move the
    // donor will have `here-1`, the target `cnt[c]+1`, and the maximum must DECREASE.
    // THE SEPARATOR: variant 3 COUNTS the ready ones (that is, pays exactly the same price
    // as variant 2), but DECIDES the old way. Without it "the policy is wrong" and
    // "the measurement of the policy is too expensive" are indistinguishable, while they are cured
    // differently.
    let rule = WAKEBAL_RULE.load(Ordering::Relaxed);
    let mut cnt = [0u8; MAXCPU];
    let here = if rule >= 2 {
        crate::sched::runnable_counts(&mut cnt);
        cnt[from] as usize
    } else { 0 };
    for c in 0..MAXCPU {
        if c == from || allow & (1u64 << c) == 0 { continue; }
        if !shared_core(c) || DEDICATED[c].load(Ordering::SeqCst) != 0 { continue; }
        if rule == 2 { if cnt[c] as usize + 2 > here { continue; } }
        else if cpu_has_work(c) { continue; }
        if now.wrapping_sub(WAKE_CLAIM[c].load(Ordering::Relaxed)) < claim { continue; } // it has already been given away
        WAKE_CLAIM[c].store(now, Ordering::Relaxed);
        WAKE_MOVED.fetch_add(1, Ordering::Relaxed);
        return Some(c);
    }
    None
}

/// How many times stealing was asked AT ALL, and how many times it did not find a candidate.
///
/// Without these two numbers "the steal did not work" is indistinguishable from "the steal was not called", while
/// they are cured by the opposite: the first by the selection rule, the second by making
/// an underloaded core reach the scheduler at all. It was exactly on this difference
/// that the analysis of ClickHouse's fourth core rested.
pub static STEAL_TRIES: AtomicU64 = AtomicU64::new(0);
/// The steal attempts under a bad layout and the outcome for the donor's extra ready one:
/// [0] the attempts, [1] pinned, [2] asleep, [3] the mask, [4] the balance of the assigned ones, [5] hot, [6] it would have stolen it.
pub static STEAL_MIS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
pub static STEAL_NOCAND: AtomicU64 = AtomicU64::new(0);
/// How many times a steal refused a candidate because it is still cache-hot.
pub static STEAL_HOT: AtomicU64 = AtomicU64::new(0);
/// Why the candidate did not fit — BY FILTER. "We did not find a candidate" as one number
/// cannot be cured: the donor has no queue, the task is executing, the task is asleep and the task
/// is not allowed here — four different diagnoses.
/// 0 — it is executing, 1 — it sleeps on a futex, 2 — the mask does not let it in, 3 — the donor has
/// fewer than two ready ones, 4 — it is pinned/moving.
pub static STEAL_SKIP: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

pub fn steal_candidate(cpu: usize) -> Option<usize> {
    if NOSTEAL.load(Ordering::Relaxed) { return None; }
    if !shared_core(cpu) { return None; }               // a dedicated core does not take foreign work
    if DEDICATED[cpu].load(Ordering::SeqCst) != 0 { return None; }
    STEAL_TRIES.fetch_add(1, Ordering::Relaxed);        // we asked in essence
    // The diagnostics (`talaria.runhist=1`): how many steal attempts fell on
    // A BAD LAYOUT (the thief has no ready ones, somebody has two or more) and with what they
    // ended: the same filters as below, for the donor's first extra ready one.
    if crate::sched::RUNHIST_ON.load(Ordering::Relaxed) {
        let mut c = [0u8; MAXCPU];
        crate::sched::runnable_counts(&mut c);
        if c[cpu] == 0 {
            if let Some(from) = (0..MAXCPU).find(|&k| c[k] >= 2) {
                STEAL_MIS[0].fetch_add(1, Ordering::Relaxed);
                let r5 = STEALBAL_RULE.load(Ordering::Relaxed) == 5;
                let n = crate::sched::task_count().min(NTASKS);
                for t in 0..n {
                    if crate::sched::cpu_of(t) != from || !crate::sched::task_alive(t) || crate::sched::is_running(t)
                        || !crate::sync::runnable_peek(t) { continue; }
                    // The two rules answer the same two questions — "would the steal
                    // improve the balance" and "is the candidate still cache-hot" — by
                    // different measures; the ANSWER is what the counter records, so each
                    // question is asked once here instead of twice in the chain.
                    let bal_refuse = if r5 { c[cpu] as usize + 2 > c[from] as usize }
                                     else { crate::sched::tasks_on(cpu) + 2 > crate::sched::tasks_on(from) };
                    let hot_refuse = if r5 { crate::sched::rq_open_ns(t) < STEAL_WAIT_NS.load(Ordering::Relaxed) }
                                     else { crate::sched::ns_since_off(t) < crate::smp::effective_slice_ns() };
                    let why = if PINNED[t].load(Ordering::SeqCst) { 1 }
                        else if crate::sync::blocked_for_sched(t) { 2 }
                        else if allowed_mask(t) & (1u64 << cpu) == 0 { 3 }
                        else if bal_refuse { 4 }
                        else if hot_refuse { 5 }
                        else { 6 };
                    STEAL_MIS[why].fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }
    }
    // "IT SLEEPS" IS NOT "IT IS FREE", AND A STEAL MUST IMPROVE THE BALANCE RATHER THAN CHANGE
    // ITS SIGN.
    //
    // A thief comes here when it has no READY tasks of its own. But with a pipeline and
    // a fan-out a thread sleeps BETWEEN MESSAGES — microseconds — and the instantaneous sign
    // "there are no ready ones right now" means "busy" rather than "empty". The steal looked
    // only at that sign and at the number of the donor's ready ones, so it calmly
    // took the only extra task from a core with two onto a core with one:
    // the skew 2/1 became 1/2, that is, exactly the same skew from the other
    // side, and nobody spread it apart back any more (the balancer does not reach the skew's
    // threshold — see `BAL_SEEN`, while the thief no longer looks for work: it
    // "has" some).
    //
    // It was caught as the BISTABLE collapse of `actors fanout tsync spread`: 25 us per
    // message instead of 1.5 and 118 thousand messages/s instead of 1.85 million. The steals' log
    // named the culprit word for word: "core 3 <- t4 from core 0, tasks on the thief 1, on
    // the donor 2" — the producer moved onto a core where a consumer already lived.
    //
    // The rule: after a steal the thief must have NO MORE than what is left at
    // the donor. It limits itself: a pool of eight threads on one core
    // spreads out to 4/4 and stops.
    //
    // WHAT TO COUNT THAT "NO MORE" WITH is the whole question, and the first answer was
    // wrong. It was counted by the number of ASSIGNED tasks: it does not flicker, unlike
    // the instantaneous readiness. But in a thread pool the majority of the assigned tasks are
    // ASLEEP, and a count by them speaks not of the load but of the pool's size (see the analysis
    // at the check itself below). Now it is counted by the READY ones, while against the flickering
    // it is protected not by a counter but by the fact that a thief comes here only with an EMPTY core, and
    // the difference must be not less than two.
        let n = crate::sched::task_count().min(NTASKS);
    // THE READY ONES BY CORE IN ONE PASS, before the loop over the candidates (variants 2 and 3).
    // In the former rule `runnable_on(from)` was called for EVERY candidate, that is,
    // a walk over the whole task table inside a walk over it — and through
    // `blocked_for_sched` on top of that, which spins the skip counter (a side effect in
    // a predicate, because of it the count of the ready ones is slightly OVERSTATED). `runnable_counts`
    // costs one pass and has no side effects.
    //
    // It is computed only under the variant that uses it: the default must
    // execute EXACTLY the former code, otherwise "the change's contribution" will mix with "the contribution
    // of a different count of the readiness", and those two have already been confused twice.
    let rule = STEALBAL_RULE.load(Ordering::Relaxed);
    let mut cnt = [0u8; MAXCPU];
    if rule >= 2 { crate::sched::runnable_counts(&mut cnt); }
    // THE DEMAND BY CORE — for rule 4 (the analysis is at `STEALBAL_RULE`).
    let mut wgt = [0u32; MAXCPU];
    if rule >= 4 { demand_counts(&mut wgt); }
    // "The core is empty" BY THE DEMAND rather than by the instantaneous readiness: a thread sleeping
    // microseconds between messages does not lose its demand, while a pool's thread sleeping
    // for seconds does. It is exactly by this that the two cases differ.
    let thief_idle = rule >= 4 && crate::placement::demand_idle(wgt[cpu]);
    let mut best = usize::MAX;
    let mut best_w = 0u32;
    for t in 0..n {
        let from = crate::sched::cpu_of(t);
        if from == cpu || !crate::sched::task_alive(t) || crate::sched::task_pending(t) { continue; }
        // Stealing from the BSP IS ALLOWED: the occupancy of its tasks is marked not with the AP_TAKEN flag
        // but with the CURRENT field, so a thief after the capture verifies once more against CURRENT,
        // while the BSP itself before entering a task verifies against AP_TAKEN (see `ap_schedule` and
        // `rust_schedule_inner`). Without that two threads that settled on the control
        // plane could not be spread apart by anybody.
        if PINNED[t].load(Ordering::SeqCst) { STEAL_SKIP[4].fetch_add(1, Ordering::Relaxed); continue; }
        if crate::sched::is_running(t) { STEAL_SKIP[0].fetch_add(1, Ordering::Relaxed); continue; }
        if crate::sync::blocked_for_sched(t) { STEAL_SKIP[1].fetch_add(1, Ordering::Relaxed); continue; }
        if allowed_mask(t) & (1u64 << cpu) == 0 { STEAL_SKIP[2].fetch_add(1, Ordering::Relaxed); continue; }
        // The donor has only this one — there is nothing to take. The former rule computes that with
        // `runnable_on` (a walk over the table for EVERY candidate), the new ones by
        // a ready slice.
        let donor_ready = if rule >= 2 { cnt[from] as usize } else { crate::sched::runnable_on(from) };
        if donor_ready < 2 { STEAL_SKIP[3].fetch_add(1, Ordering::Relaxed); continue; }
        // THE BALANCE IS COUNTED BY THE READY ONES RATHER THAN BY THE ASSIGNED ONES.
        //
        // The rule "after a steal the thief has no more than what is left at the donor" is right,
        // but measuring it by the number of ASSIGNED tasks is possible only where there are
        // few sleeping ones. In a thread pool they are the majority: ClickHouse keeps two dozen
        // executors, four wake up per query — and a core with five sleeping ones
        // looked "more loaded" than a core with two computing ones. The steal backed off
        // for nothing: 235 refusals against 2 that took place, while the query meanwhile went on
        // THREE cores out of four (`max_threads=4` — 110 ms; the same query with
        // `max_threads=6`, where the extra threads plug the hole themselves, — 86 against Linux's 84).
        // Linux).
        //
        // The reservation for whose sake the rule appeared at all (session 20) has not gone
        // anywhere: with a fan-out and a pipeline a thread sleeps BETWEEN MESSAGES, and
        // the instantaneous readiness flickers there. What holds it here is not a counter but the
        // asymmetry of the condition itself: a thief enters a steal ONLY when it has no ready ones of
        // its own, while the donor must have two more. A flickering "producer-consumer"
        // pair does not give that — it has now one ready, now the other.
        //
        // `talaria.stealbal=1` returns the former count (by the assigned ones) —
        // the rule's contribution is measured counter-wise, as with `nosteal` and `nowakebal`.
        // The rule itself — "the steal must improve the balance" — is
        // `placement::steal_improves` (it is tested on the host, including the
        // convergence of a pool and the threshold of the demand); here is only what
        // to count it over.
        let by = crate::placement::steal_by(rule);
        let (thief_w, donor_w) = match by {
            // The balance OF THE DEMAND, the threshold is the same `IMBALANCE_W` with which the
            // balancer measures the skew (one and a half always-ready threads).
            crate::placement::StealBy::Demand => (wgt[cpu], wgt[from]),
            crate::placement::StealBy::Ready => (cnt[cpu] as u32, cnt[from] as u32),
            crate::placement::StealBy::Assigned => (crate::sched::tasks_on(cpu) as u32, crate::sched::tasks_on(from) as u32),
        };
        let refuse = !crate::placement::steal_improves(by, thief_w, donor_w);
        if refuse {
            STEAL_BUSY.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        // DO NOT TOUCH A CACHE-HOT ONE. A task that has just left a core has left
        // its working set there; a move zeroes it twice — for the task itself and for the one
        // it comes to. While the slice was eight milliseconds, that cost nothing:
        // between steals a task managed to work a whole eternity. On
        // a half-millisecond slice the scheduler enters sixteen times as often,
        // and without this rule the computing threads started roaming over the cores —
        // the JVM's speedup on four cores fell from 4.0x to 2.0x (the measurement: the same
        // run with `talaria.nosteal=1` gave 3.9-4.0x). The threshold is the slice: it makes sense to steal
        // the one that has already missed a whole round, that is, really
        // starves, rather than the one that is simply waiting for its turn.
        // THE COOLING THRESHOLD WAS EQUAL TO EXACTLY THE PERIOD WITH WHICH IT IS CHECKED.
        //
        // Two ready threads on one core replace each other EVERY SLICE. Which means that the one
        // that left the core waits for its turn exactly a slice — and "it has stood without a core
        // for not less than a slice" is NEVER satisfied for it: at the moment of the check
        // it has waited less. The threshold, conceived as a protection against roaming, became
        // a protection of the skew. The measurement on ClickHouse: 745 refusals "it is cache-hot"
        // against 40 steals that took place — with a neighbouring core idling.
        //
        // Removing the threshold entirely is NOT ALLOWED, and that is checked rather than deduced: the variant
        // without the threshold brought back session 20's collapse — `actors fanout tsync spread` gave
        // a p50 of 35.4 us instead of 1.5 and 89 thousand messages/s instead of 1.90 million.
        // A "producer-consumer" pair replaces itself on a core in MICROSECONDS, and without
        // the threshold it is pulled apart over the cores without end.
        //
        // A threshold SMALLER than the replacement period suggested itself: a computing thread that waits
        // half a slice passes it, while a pair that changes over in microseconds does
        // not (250 us against single ones). That is how `talaria.stealbal=3` is arranged — and
        // IT GIVES NO GAIN (~110 ms against ~114 with the former rule). That is, the gain of a removed
        // threshold came not from a "correct" transplant but from the roaming itself:
        // it accidentally staffs the cores for a computing workload and tears the locality
        // of message passing. So the default is the former one (`=1`), while the
        // analysis is at `STEALBAL_RULE`.
        // THE COOLING THRESHOLD IS REMOVED ONLY WHEN ITS WORK IS DONE BY SOMEBODY
        // ELSE. It protected a "producer-consumer" pair from being pulled apart; in
        // rule 4 it is protected by the thief's DEMAND (its core does not look empty), and
        // then the threshold for a knowingly empty core is superfluous — while it was exactly it that did not
        // let ClickHouse's starving executor through.
        // RULE 5: the hotness is measured not by "how long ago it left" but by "how long it
        // HAS ALREADY BEEN WAITING" (`rq_open_ns`): a pair that changes over in microseconds waits
        // microseconds and stays in place; an executor that has stood for
        // `STEAL_WAIT_NS` is starving — it is taken.
        if rule == 5 {
            if crate::sched::rq_open_ns(t) < STEAL_WAIT_NS.load(Ordering::Relaxed) {
                STEAL_HOT.fetch_add(1, Ordering::Relaxed); continue;
            }
        } else {
        let hot_ns = if rule >= 4 && thief_idle { 0 }
                     else if rule == 3 { crate::smp::effective_slice_ns() / 2 }
                     else { crate::smp::effective_slice_ns() };
        if crate::sched::ns_since_off(t) < hot_ns {
            STEAL_HOT.fetch_add(1, Ordering::Relaxed); continue;
        }
        }
        let w = RUNW[t].load(Ordering::Relaxed);                  // the "hungriest" one goes first
        if best == usize::MAX || w > best_w { best = t; best_w = w; }
    }
    if best == usize::MAX { STEAL_NOCAND.fetch_add(1, Ordering::Relaxed); return None; }
    // THE FIRST EIGHT STEALS GO INTO THE LOG. A steal carries a thread onto a foreign core, and that is
    // the largest decision the scheduler makes silently: one
    // wrong steal seats two active threads on one core FOREVER (nobody will spread them
    // apart back — the balancer does not reach the skew, while the thief no longer
    // looks for work). Analysing such a thing by the counter "2 steals" is impossible.
    {
        let k = STEAL_LOG.fetch_add(1, Ordering::Relaxed) as usize;
        if k < STEAL_N {
            let mine = crate::percpu::cur_task_of(cpu);
            STEAL_REC[k][0].store(cpu as u64, Ordering::Relaxed);
            STEAL_REC[k][1].store(best as u64, Ordering::Relaxed);
            STEAL_REC[k][2].store(crate::sched::cpu_of(best) as u64, Ordering::Relaxed);
            STEAL_REC[k][3].store(if mine == u64::MAX { 255 } else { mine }, Ordering::Relaxed);
            STEAL_REC[k][4].store(crate::sched::ns_since_off(best) / 1000, Ordering::Relaxed);
            STEAL_REC[k][5].store(crate::tsc::mono_ns() / 1000, Ordering::Relaxed);
            STEAL_REC[k][6].store(crate::sched::tasks_on(cpu) as u64, Ordering::Relaxed);
            STEAL_REC[k][7].store(crate::sched::tasks_on(crate::sched::cpu_of(best)) as u64, Ordering::Relaxed);
        }
    }
    Some(best)
}

/// The load levelling: once every `BALANCE_EVERY` BSP ticks move ONE non-executing
/// unpinned task from the most loaded core onto the freest
/// one — and only if the skew has grown up to `IMBALANCE`.
/// The balancer's counters for `stat:` — "the passes / a skew found / moved".
/// Without them "the balancing is on" and "the balancing works" are indistinguishable, while
/// the difference between them cost this session a sixteenfold collapse: the skew
/// was counted BY TICKS, while the tick's period stopped being a constant.
/// How many times a steal backed off because it would not have improved the balance
/// (after it the thief would have had more tasks than would be left at the donor).
pub static STEAL_BUSY: AtomicU64 = AtomicU64::new(0);
/// The steal's rule (`talaria.stealbal=`): `1` — the balance by the ASSIGNED tasks and
/// a cooling threshold of a whole slice (THE DEFAULT, exactly the former code); `2` — the balance by
/// the READY ones; `3` — by the ready ones and a cooling threshold of HALF a slice; `4` — the balance
/// by the DEMAND (`RUNW`) and without a threshold when the thief's demand is close to zero.
///
/// THE DEFAULT IS KEPT THE FORMER ONE DELIBERATELY: not a single variant wins with
/// both workloads at once, and that means the lever is the wrong one. What was measured:
///
/// | variant | ClickHouse (4 cores, `max_threads`=4) | `actors fanout tsync spread` |
/// |---|---|---|
/// | `1` | ~114 ms, four cores busy in 2009 ticks | p50 1.5 us, 1.90 million msg/s |
/// | `2` | ~106 ms, 2045 | not measured |
/// | without a threshold at all | **~95 ms, 2251** | **p50 35.4 us, 89 thousand msg/s** |
/// | `3` (half a slice) | ~110 ms, 1594-1854 | not measured |
///
/// The variant without a threshold is the only one that gave ClickHouse a real gain, and it
/// also brought back session 20's collapse TWENTY-THREE TIMES OVER. Which means the gain there came not from
/// a "correct" transplant but from THE ROAMING: it accidentally staffs the cores for
/// a computing workload and tears the locality for message passing. Half a slice
/// removes the roaming — and the gain disappears together with it.
///
/// What is missing for a real decision: a metric "how long the task HAS BEEN READY and not
/// running", different from "how much time has passed since it left a core". For
/// a preempted one standing in a queue that is one and the same number, so the present
/// threshold does not tell a starving executor from a cache-hot thread in principle.
///
/// Variant `4` was introduced after it turned out WHY going through the rules did not
/// move the numbers: stealing did not reach the selection at all. The counter of the rejection of candidates
/// named that with one number — 87 916 refusals "it is pinned" against 24 300
/// "it is executing" and 213 "the donor has no queue". The culprit was `set_affinity`,
/// which set `PINNED` on ANY mask, including a full one (see there). After
/// the fix the "it is pinned" refusals are 2200-3100, the stealing works — but the outcome of `4`
/// is still within the spread (four cores busy 2049-2412 ticks against
/// 1743-1850, the median of `t=4` is the same). So the default is the former one.
///
/// The JVM's `speedup4` does not tell the variants apart at all (3.78-3.98 against 4.01-3.76).
pub static STEALBAL_RULE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(1);
/// The starvation threshold for rule 5 (`talaria.stealwait=<us>`), ns.
pub static STEAL_WAIT_NS: AtomicU64 = AtomicU64::new(100_000);
static STEAL_LOG: AtomicU64 = AtomicU64::new(0);
/// The log of the first steals: (the thief core, the victim, the donor, its own task, it left N us ago, the moment in us).
/// It is printed in `stat:` — printing from the steal itself is not allowed, the lines of several
/// cores arrive mixed character by character.
pub const STEAL_N: usize = 8;
pub static STEAL_REC: [[AtomicU64; 8]; STEAL_N] =
    [const { [const { AtomicU64::new(u64::MAX) }; 8] }; STEAL_N];
/// `talaria.nobalance=1` — switch the periodic levelling off (the diagnostics:
/// the mechanism's contribution is measured only COUNTER-WISE, as with `nosteal` and `nowakebal`).
pub static NOBALANCE: AtomicBool = AtomicBool::new(false);
pub static BAL_PASS: AtomicU64 = AtomicU64::new(0);
pub static BAL_SEEN: AtomicU64 = AtomicU64::new(0);
/// The greatest skew the balancer has seen (in units of W_FULL*1000/1024).
pub static BAL_MAXW: AtomicU64 = AtomicU64::new(0);

/// The balancer's timestamps (the TSC): the last refresh of the demand and the last decision.
static DEMAND_AT: AtomicU64 = AtomicU64::new(0);
static BALANCE_AT: AtomicU64 = AtomicU64::new(0);

pub fn balance_tick() {
    // The periodic levelling happens only by the plan (`balance=` in the manifest).
    // We tried switching it on without a plan too, as an insurance on top of work stealing:
    // it did not narrow the spread of the speedup on four cores (a rearrangement once every
    // 32 ms comes late for a working phase of 66 ms), while it brought down one run out of six
    // — it moves CPU_OF bypassing the occupancy mark that the thief uses.
    // It is not worth it: the resettlement is done by the layout at a thread's creation and by
    // work stealing, both fire at once.
    if NOBALANCE.load(Ordering::Relaxed) { return; }
    if !CONFIGURED.load(Ordering::SeqCst) || !BALANCE.load(Ordering::SeqCst) { return; }
    // THE CLOCK IS THE TSC RATHER THAN THE TICK COUNTER (see BALANCE_EVERY_NS). Both values below
    // are set in time, so a change of the tick's period no longer shifts them.
    let now = crate::tsc::rdtsc();
    let hz = crate::tsc::calibrate_cached();
    if hz == 0 { return; }
    let per_ns = |ns: u64| (hz / 1_000_000).max(1) * (ns / 1000).max(1);
    // The demand is refreshed AT EVERY pass where enough time has passed —
    // regardless of whether it came to a decision.
    let d0 = DEMAND_AT.load(Ordering::Relaxed);
    if now.wrapping_sub(d0) < per_ns(DEMAND_EVERY_NS) { return; }
    DEMAND_AT.store(now, Ordering::Relaxed);
    refresh_demand();
    let b0 = BALANCE_AT.load(Ordering::Relaxed);
    if now.wrapping_sub(b0) < per_ns(BALANCE_EVERY_NS) { return; }
    BALANCE_AT.store(now, Ordering::Relaxed);
    let tick = now;                  // the "cooling" after a move is in time too
    BAL_PASS.fetch_add(1, Ordering::Relaxed);
    // WE count inside the tick's handler, so the cheap refusal comes first: with one
    // core of the pool there is nothing to balance, while a snapshot costs a walk over all the cores and tasks.
    if crate::sched::sched_cpus().count_ones() < 2 { return; }
    let mut w = snapshot();
    // Step 0 is self-healing: on a dedicated core there is no place for anybody but the owner.
    // Somebody might not have managed to move away at the moment of the dedication (it was executing) — we take it
    // now. Without that "a dedicated core" would be a promise rather than a property.
    let n = crate::sched::task_count().min(NTASKS);
    for c in 0..MAXCPU {
        let owner = DEDICATED[c].load(Ordering::SeqCst);
        if owner == 0 { continue; }
        for t in 0..n {
            if t as u32 + 1 == owner || crate::sched::cpu_of(t) != c || !crate::sched::task_alive(t) { continue; }
            if PINNED[t].load(Ordering::SeqCst) { continue; }
            let dst = pick_in_pool(&w, node_of(c), pool_of_task(t));
            if dst != c && crate::sched::migrate_idle_task(t, dst) {
                MIGRATIONS.fetch_add(1, Ordering::Relaxed);
                LAST_MOVE[t].store(tick, Ordering::SeqCst);
                return; // not more than one rearrangement per pass
            }
        }
    }
    // THE RESETTLEMENT GOES IN A BATCH WHILE THERE IS A SKEW.
    //
    // "Not more than one rearrangement per pass" was right as a protection against
    // chattering and wrong as a speed: seven threads that settled on one core
    // would resettle over seven windows in a row — 224 ms, while the workload's working phase
    // is shorter. The skew meanwhile IS RECOMPUTED after every
    // rearrangement (the snapshot is corrected in place), so a batch stops
    // by itself as soon as the threshold stops being reached — it has nothing to chatter on.
    for _ in 0..MOVES_PER_PASS {
    // The levelling goes FOR EVERY pool separately, starting with the common one: a skew
    // between the pools is not a skew but the operator's decision, and it must not be touched.
    let npool = NPOOL.load(Ordering::SeqCst);
    let mut chosen = None;
    if let Some(p) = imbalance_in_pool(&w, POOL_NONE) { chosen = Some((POOL_NONE, p)); }
    else {
        for i in 0..npool.min(MAXPOOLS as u8) {
            if let Some(p) = imbalance_in_pool(&w, i) { chosen = Some((i, p)); break; }
        }
    }
    let (bal_pool, (from, to)) = match chosen { Some(x) => x, None => return };
    BAL_SEEN.fetch_add(1, Ordering::Relaxed);
    {   // the maximum of the observed skew — so that "the skew is small" and "there is no skew"
        // do not look the same in the report
        let d = w.weight[from].saturating_sub(w.weight[to]);
        let mut m = BAL_MAXW.load(Ordering::Relaxed);
        while d as u64 > m {
            match BAL_MAXW.compare_exchange_weak(m, d as u64, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break, Err(v) => m = v }
        }
    }
    // The candidate: alive, not pinned by a rule, not executing right now and
    // cooled down after the previous move. Of the suitable ones we take the one whose demand is closest
    // to HALF of the difference: moving the greediest one means throwing
    // the skew onto another core and coming here again at the next pass, only
    // mirrored. Half of the difference is the only weight that closes it.
    let want = (w.weight[from] - w.weight[to]) / 2;
    let n = crate::sched::task_count().min(NTASKS);
    let mut best = usize::MAX; let mut best_err = u32::MAX;
    for t in 0..n {
        if crate::sched::cpu_of(t) != from || !crate::sched::task_alive(t) || crate::sched::task_pending(t) { continue; }
        if PINNED[t].load(Ordering::SeqCst) { continue; }
        if pool_of_task(t) != bal_pool { continue; } // we do not level a foreign pool with this step
        if tick.wrapping_sub(LAST_MOVE[t].load(Ordering::SeqCst)) < per_ns(COOLDOWN_NS) { continue; }
        if crate::sched::is_running(t) { continue; }
        let x = RUNW[t].load(Ordering::Relaxed);
        let err = x.abs_diff(want);
        if err < best_err { best_err = err; best = t; }
    }
    if best == usize::MAX { return; }                      // there is nobody to move — there is nowhere further
    if !crate::sched::migrate_idle_task(best, to) {
        // IT DID NOT GO — WHICH MEANS IT IS BUSY rather than "there is nothing more to resettle". Previously
        // the whole pass ended here, and one thread that clung to a core left
        // the skew for another 32 ms. We mark it as cooled down (the same as after
        // a move) and take the next one in this same batch.
        LAST_MOVE[best].store(tick, Ordering::SeqCst);
        continue;
    }
    LAST_MOVE[best].store(tick, Ordering::SeqCst);
    MIGRATIONS.fetch_add(1, Ordering::Relaxed);
    BALANCED.fetch_add(1, Ordering::Relaxed);
    // We correct the snapshot in place: the next iteration must see the ALREADY
    // moved task, otherwise a batch would carry everything off one core in a row.
    let wt = RUNW[best].load(Ordering::Relaxed);
    w.weight[from] = w.weight[from].saturating_sub(wt);
    w.weight[to] = w.weight[to].saturating_add(wt);
    w.load[from] = w.load[from].saturating_sub(1);
    w.load[to] = w.load[to].saturating_add(1);
    }
}

/// The task has died — return its dedicated core into circulation.
pub fn release(t: usize) {
    for c in 0..MAXCPU {
        if DEDICATED[c].load(Ordering::SeqCst) == t as u32 + 1 { DEDICATED[c].store(0, Ordering::SeqCst); }
    }
    if t < NTASKS { PINNED[t].store(false, Ordering::SeqCst); }
}

/// The console's `cores`: who lives where.
pub fn dump() {
    crate::println!("cores: the scheduler's cores {:#x}, migrations {} (of them by the balancing {}), refusals of a dedicated core {}",
        crate::sched::sched_cpus(), MIGRATIONS.load(Ordering::Relaxed),
        BALANCED.load(Ordering::Relaxed), DED_DENIED.load(Ordering::Relaxed));
    let n = crate::sched::task_count();
    for c in 0..MAXCPU {
        let r = role(c);
        if r == ROLE_OFF { continue; }
        crate::print!("cores:   the core {} the node {} {}", c, node_of(c),
            match r { ROLE_DEDICATED => "dedicated", _ => "shared" });
        // For a dedicated core we do not print the pool: it is given to a thread entirely, and
        // belonging to a pool means nothing for it.
        if r != ROLE_DEDICATED {
            let cp = pool_of_cpu(c);
            if cp != POOL_NONE { crate::print!(" the pool "); puts(pool_name(cp)); } else { crate::print!(" (common)"); }
        }
        crate::print!(":");
        for t in 0..n.min(NTASKS) {
            if crate::sched::cpu_of(t) != c || !crate::sched::task_alive(t) { continue; }
            crate::print!(" t{}", t);
            let name = crate::sched::comm(t);
            let l = crate::sched::comm_len(t);
            if l > 0 {
                crate::print!("(");
                puts(&name[..l]);
                crate::print!(")");
            }
            if PINNED[t].load(Ordering::SeqCst) { crate::print!("*"); }
        }
        crate::println!("");
    }
}

// ── the policy's regression without hardware (`talaria.test=affinity`) ──────

/// A self-check of the policy's decisions on synthetic snapshots. It catches exactly what
/// would otherwise be checked only on a machine with four nodes and fifty cores:
/// the choice of a core, the dedication, the locality by node and the convergence of the levelling.
pub fn selftest() -> ! {
    let mut fail = 0usize;
    let mut check = |cond: bool, what: &str| {
        if cond { crate::println!("affinity: OK   {}", what); }
        else { crate::println!("affinity: FAILURE {}", what); fail += 1; }
    };

    // The machine: 8 cores, two NUMA nodes (0: the cores 0..3, 1: the cores 4..7).
    let mk = || {
        let mut w = World::empty();
        w.pool = 0xFF;
        for c in 0..8 { w.node[c] = if c < 4 { 0 } else { 1 }; }
        w
    };

    // 1. The spreading: an empty machine -> the first free core of the home node.
    let w = mk();
    check(pick_shared_in(&w, 1) == 4, "the spreading puts a thread on its own node (the node 1 -> the core 4)");
    check(pick_shared_in(&w, 255) == 0, "without NUMA — the pool's first core");

    // 2. The dedication: a free core of one's own node, core 0 (the control plane) is not given away.
    let mut w = mk();
    w.load[0] = 1; // the main thread lives on the control plane
    check(pick_dedicated_in(&w, 0) == Some(1), "a dedicated core is taken on its own node and NOT from core 0");
    for c in 1..8 { w.ded[c] = true; }
    check(pick_dedicated_in(&w, 0).is_none(), "there are no free cores -> a refusal (rather than a silent substitution)");

    // 3. A busy core is not taken for a dedication WHILE there are free ones.
    let mut w = mk();
    w.load[1] = 1;
    check(pick_dedicated_in(&w, 0) == Some(2), "while there is a free core, we do not touch a busy one");

    // 3b. There are no free ones (the spreading managed to lay the threads out everywhere) — the rule is still
    //     senior to the spreading: we take the core with the smallest number of resettlers.
    let mut w = mk();
    w.load[0] = 4;
    for c in 1..8 { w.load[c] = 3; }
    w.load[6] = 1;
    check(pick_dedicated_in(&w, 255) == Some(6), "there are no free ones -> the core with the minimum of resettlers");

    // 3c. A core where a thread placed BY A RULE sits is not taken away: two rules
    //     are not entitled to cancel each other silently.
    let mut w = mk();
    for c in 0..8 { w.load[c] = 1; }
    for c in 1..8 { w.pinned[c] = 1; }
    check(pick_dedicated_in(&w, 255).is_none(), "cores with pinned threads are not taken for a dedication");
    w.pinned[5] = 0;
    check(pick_dedicated_in(&w, 255) == Some(5), "the only core without pinned threads is the one we take");

    // 4. The home node gives in only at a skew greater than NUMA_SLACK.
    let mut w = mk();
    w.load[4] = 1; w.load[5] = 1; w.load[6] = 1; w.load[7] = 1; // the node 1 is loaded
    check(pick_shared_in(&w, 1) == 4, "a skew of one slot does not break the locality");
    for c in 4..8 { w.load[c] = 2; }
    check(pick_shared_in(&w, 1) == 0, "a skew greater than NUMA_SLACK -> the thread moves to the neighbouring node");

    // 4b. THE DEMAND MUST REACH THE THRESHOLD WITHIN ONE DECISION WINDOW.
    //
    // Here everything else broke. The demand is a running average of the readiness, and its
    // time constant is set by THE NUMBER of updates; the skew's threshold is set in the
    // same units. While the demand was refreshed once per pass of the balancer, ten updates
    // accumulated over a run, the demand of an always-ready thread reached a third
    // of the limit, and a skew was NEVER found — the balancer was on and
    // did nothing (`stat: balancing: passes 12, a skew found 0`).
    // The statement ties three constants at once: the refresh frequency, the filter's constant
    // and the threshold. Breaking any of them separately is now impossible.
    {
        let n = demand_per_pass();
        let up = demand_after(n);
        let down = demand_decay(n);
        println!("affinity: the demand over a decision window ({} updates): a ready one {} out of {}, a fallen-asleep one {}",
            n, up, W_FULL, down);
        check(2 * up >= imbalance_threshold(),
              "the demand of TWO always-ready threads over one window reaches the skew's threshold");
        check(up <= W_FULL, "the demand does not exceed the limit (the filter does not diverge)");
        check(down * 8 < W_FULL, "the demand of a fallen-asleep thread over the same window falls almost to zero");
        check(imbalance_threshold() > W_FULL && imbalance_threshold() < 2 * W_FULL,
              "the skew's threshold lies STRICTLY between one and two ready threads");
    }

    // 5. The levelling: it is computed by the DEMAND. An even demand -> there is nothing to move.
    //    (`busy(w, c, n)` — n always-ready threads on the core c.)
    let busy = |w: &mut World, c: usize, n: u8| { w.load[c] = n; w.weight[c] = n as u32 * W_FULL; };
    let mut w = mk();
    for c in 0..8 { busy(&mut w, c, 3); }
    check(imbalance_in(&w).is_none(), "an even load -> ZERO migrations");
    busy(&mut w, 3, 4);
    check(imbalance_in(&w).is_none(), "a skew of one task -> ZERO migrations (the hysteresis)");
    busy(&mut w, 3, 5);
    check(imbalance_in(&w) == Some((3, 0)), "a skew of two tasks -> the rearrangement 3 -> 0");

    // 5b. What a count BY HEADS decided wrongly: on the core 3 there are five threads, but
    //     four of them sleep on a socket, while on the core 0 there are two — and both work.
    //     The busy core here is the core 0, and there is no reason to touch the core 3.
    let mut w = mk();
    for c in 0..8 { busy(&mut w, c, 1); }
    w.load[3] = 5; w.weight[3] = W_FULL;      // five threads, the demand of one
    w.load[0] = 2; w.weight[0] = 2 * W_FULL;  // two threads, both work
    check(imbalance_in(&w).is_none(), "sleeping threads do not make a core loaded (by heads there would be a false skew)");
    w.load[0] = 4; w.weight[0] = 4 * W_FULL;
    check(imbalance_in(&w).map(|p| p.0) == Some(0), "the donor is chosen by the demand rather than by the number of threads");

    // 5c. The only task is not taken off a core: that is not a levelling but a transfer
    //     of the skew onto a neighbour.
    let mut w = mk();
    for c in 0..8 { w.load[c] = 0; w.weight[c] = 0; }
    w.load[2] = 1; w.weight[2] = 4 * W_FULL;  // one very greedy thread
    check(imbalance_in(&w).is_none(), "the only task is not taken off a core (there is nowhere to transfer the skew)");

    // 6. The convergence: 19 always-ready threads on 8 cores are levelled within a finite
    //    number of steps and STOP moving. And that is "loaded equally with
    //    a minimum of migrations", expressed checkably.
    let mut w = mk();
    busy(&mut w, 0, 19);
    let mut steps = 0;
    while let Some((a, b)) = imbalance_in(&w) {
        let (la, lb) = (w.load[a] - 1, w.load[b] + 1);
        busy(&mut w, a, la);
        busy(&mut w, b, lb);
        steps += 1;
        if steps > 1000 { break; }
    }
    let (mut lo, mut hi) = (255u8, 0u8);
    for c in 0..8 { lo = lo.min(w.load[c]); hi = hi.max(w.load[c]); }
    crate::println!("affinity: 19 threads on 8 cores: {} rearrangements, it became [{}..{}] ({} {} {} {} {} {} {} {})",
        steps, lo, hi, w.load[0], w.load[1], w.load[2], w.load[3], w.load[4], w.load[5], w.load[6], w.load[7]);
    check(steps < 1000 && hi - lo <= 1, "the load converged to an even one (the spread <= 1 task)");
    check(steps <= 19, "there are no more rearrangements than there are threads");

    // 7. The dedicated cores do not get into the common pool.
    let mut w = mk();
    w.ded[0] = false; busy(&mut w, 0, 5);
    for c in 1..7 { w.ded[c] = true; busy(&mut w, c, 1); }
    busy(&mut w, 7, 1);
    check(pick_shared_in(&w, 255) == 7, "the common pool goes around the dedicated cores");
    check(imbalance_in(&w) == Some((0, 7)), "the skew is computed only by the cores of the common pool");

    // 7b. THE NAMED POOLS: the placement and the levelling go INSIDE a pool.
    //     The cores 0..3 are common, 4..7 are the pool 0.
    let mut w = mk();
    for c in 4..8 { w.cpool[c] = 0; }
    busy(&mut w, 0, 1);
    check(pick_in_pool(&w, 255, 0) == 4, "a pool's thread sits on a core OF THE POOL rather than on the freest one in general");
    check(pick_in_pool(&w, 255, POOL_NONE) == 1, "a thread of the common pool goes around the named one's cores");
    busy(&mut w, 4, 5); busy(&mut w, 5, 1); busy(&mut w, 6, 1); busy(&mut w, 7, 1);
    busy(&mut w, 0, 9); // the common pool is skewed more, but that is NOT a reason to touch the named one
    check(imbalance_in_pool(&w, 0) == Some((4, 5)), "a skew inside a pool is corrected inside the pool");
    check(imbalance_in_pool(&w, POOL_NONE).map(|p| p.0) == Some(0), "the common pool's skew is computed by its own cores");
    check(imbalance_in_pool(&w, POOL_NONE).map(|p| p.1).map(|c| c < 4) == Some(true),
        "a core of the named pool cannot become the recipient in the common pool");

    // 7c. The named pool's cores are not given for DEDICATED ones: the operator has already said
    //     whom they belong to.
    let mut w = mk();
    for c in 1..8 { w.cpool[c] = 0; }
    check(pick_dedicated_in(&w, 255).is_none(), "a dedicated core is not taken from a named pool");
    w.cpool[6] = POOL_NONE;
    check(pick_dedicated_in(&w, 255) == Some(6), "the only core outside the pools is taken");

    // 8. The rules: the same parsing that is applied to live threads.
    let m = crate::cellman::parse(b"binary=/app\npin=main:0\npin=md_*:dedicated\npin=*:shared\n");
    check(m.pinc == 3 && m.pin_bad == 0, "three rules have been parsed");
    check(m.match_pin(b"whatever", true).map(|r| r.target) == Some(0), "the main thread is caught by the rule main");
    check(m.match_pin(b"md_feed", false).map(|r| r.target) == Some(PIN_DEDICATED), "md_* -> a dedicated core");
    check(m.match_pin(b"gc", false).map(|r| r.target) == Some(PIN_SHARED), "the trailing rule * catches the rest");

    // 9. The pools in the manifest. THE ORDER OF THE LINES DOES NOT MATTER: a rule stands ABOVE the declaration
    //    of the pool it refers to, and must be parsed all the same.
    let q = crate::cellman::parse(
        b"binary=/app\npin=w*:fast\npin=io*:@slow\npool=fast:2,3\npool=slow:4-6,9\npin=x*:nosuchpool\n");
    check(q.poolc == 2, "two pools have been parsed");
    check(q.pool_index(b"fast").map(|i| q.pool(i).mask) == Some(0b1100), "pool=fast:2,3 -> the mask 0xc");
    check(q.pool_index(b"slow").map(|i| q.pool(i).mask) == Some(0b10_0111_0000), "pool=slow:4-6,9 -> a range + a single core");
    check(q.match_pin(b"worker", false).and_then(|r| crate::cellman::pin_pool_index(r.target)) == q.pool_index(b"fast"),
        "a rule REFERS to a pool declared LOWER in the file");
    check(q.match_pin(b"io1", false).and_then(|r| crate::cellman::pin_pool_index(r.target)) == q.pool_index(b"slow"),
        "the form with @ works the same way");
    check(q.match_pin(b"xyz", false).is_none() && q.pin_bad == 1, "a reference to a NON-EXISTENT pool — the rule is discarded and counted");

    if fail == 0 { crate::println!("affinity: ALL CHECKS PASSED"); }
    else { crate::println!("affinity: THERE ARE FAILURES ({})", fail); }
    crate::suite::park()
}
