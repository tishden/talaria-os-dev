//! Atomic kernel counters for observability/validation (A2.1).
//! They are cheap (a Relaxed add) — good even for the hot path as a post-mortem check,
//! but they are NOT called on the zero-syscall fast path (there are no syscalls/faults there at all).
use core::sync::atomic::{AtomicU64, Ordering};

static SYSCALLS: AtomicU64 = AtomicU64::new(0);
static PAGE_FAULTS: AtomicU64 = AtomicU64::new(0);   // all #PFs (COW included)
static COW_FAULTS: AtomicU64 = AtomicU64::new(0);    // of them — COW copies
static FRAME_ALLOCS: AtomicU64 = AtomicU64::new(0);  // calls of alloc_contig
static FRAMES_GIVEN: AtomicU64 = AtomicU64::new(0);  // the sum of the frames handed out

// ── THE SAME PER TASK (`top`/`ps`) ───────────────────────────────────────────
//
// The total sum answers the question "is it a lot", but not the question "who". A thread
// that silently makes 200 thousand syscalls per second is indistinguishable in the total sum
// from ten threads making twenty thousand each, and that is cured by the opposite.
//
// THE PRICE. The task's index is read on EVERY syscall anyway (`LAST_SYS` in
// `rust_syscall`), so exactly one Relaxed add per event is added here.
// On the zero-syscall fast path there are no syscalls at all — there the price is zero by construction.
const NT: usize = crate::sched::NTASKS;
static SYS_BY_TASK: [AtomicU64; NT] = [const { AtomicU64::new(0) }; NT];
static PF_BY_TASK: [AtomicU64; NT] = [const { AtomicU64::new(0) }; NT];
/// The memory frames handed out by the allocator while this task was on the core. Both the
/// workload's lazy faults and the frames the kernel took while SERVING it land here
/// (the FS block cache, the buffers) — and that is exactly "how much memory this thread costs".
static FRAMES_BY_TASK: [AtomicU64; NT] = [const { AtomicU64::new(0) }; NT];

/// The syscall counter is kept WITH THE TASK NUMBER: a total without a "who" answers
/// half of the question, and `rust_syscall` reads the task number nearby anyway.
#[inline] pub fn inc_syscall_of(t: usize) {
    SYSCALLS.fetch_add(1, Ordering::Relaxed);
    if t < NT { SYS_BY_TASK[t].fetch_add(1, Ordering::Relaxed); }
}
#[inline] pub fn inc_page_fault() {
    PAGE_FAULTS.fetch_add(1, Ordering::Relaxed);
    let t = crate::sched::current_id();
    if t < NT { PF_BY_TASK[t].fetch_add(1, Ordering::Relaxed); }
}
/// The page faults of the task `t` so far (the server's turnaround takes their difference).
pub fn faults_of(t: usize) -> u64 { if t < NT { PF_BY_TASK[t].load(Ordering::Relaxed) } else { 0 } }
#[inline] pub fn inc_cow_fault() { COW_FAULTS.fetch_add(1, Ordering::Relaxed); }
#[inline] pub fn add_frames(n: u64) {
    FRAME_ALLOCS.fetch_add(1, Ordering::Relaxed);
    FRAMES_GIVEN.fetch_add(n, Ordering::Relaxed);
    let t = crate::sched::current_id();
    if t < NT { FRAMES_BY_TASK[t].fetch_add(n, Ordering::Relaxed); }
}

pub fn syscalls_of(t: usize) -> u64 { if t < NT { SYS_BY_TASK[t].load(Ordering::Relaxed) } else { 0 } }
pub fn page_faults_of(t: usize) -> u64 { if t < NT { PF_BY_TASK[t].load(Ordering::Relaxed) } else { 0 } }
pub fn frames_of(t: usize) -> u64 { if t < NT { FRAMES_BY_TASK[t].load(Ordering::Relaxed) } else { 0 } }
/// Zero the slot's counters: it went to a NEW task, a foreign tail in `top`
/// would look like "a thread was born and immediately made a million syscalls".
pub fn forget_task(t: usize) {
    if t >= NT { return; }
    SYS_BY_TASK[t].store(0, Ordering::Relaxed);
    PF_BY_TASK[t].store(0, Ordering::Relaxed);
    FRAMES_BY_TASK[t].store(0, Ordering::Relaxed);
}

pub fn syscalls() -> u64 { SYSCALLS.load(Ordering::Relaxed) }
pub fn page_faults() -> u64 { PAGE_FAULTS.load(Ordering::Relaxed) }
pub fn cow_faults() -> u64 { COW_FAULTS.load(Ordering::Relaxed) }
pub fn frame_allocs() -> u64 { FRAME_ALLOCS.load(Ordering::Relaxed) }
pub fn frames_given() -> u64 { FRAMES_GIVEN.load(Ordering::Relaxed) }

/// A snapshot for the enforcement window (A2.2): return (syscalls, page_faults).
pub fn snapshot() -> (u64, u64) { (syscalls(), page_faults()) }

/// Print a summary of the counters (the console's `stat`).
pub fn dump() {
    {
        use core::sync::atomic::Ordering::Relaxed;
        let st = crate::sched::HINT_SET.load(Relaxed);
        let cl = crate::sched::HINT_CLOBBER.load(Relaxed);
        let (sa, sl) = (crate::sched::HINT_SAME.load(Relaxed), crate::sched::HINT_STALE.load(Relaxed));
        let (us, pk) = (crate::sched::HINT_USED.load(Relaxed), crate::sched::HINT_PICKUP.load(Relaxed));
        if st != 0 {
            // "Overwritten by a foreign one" is only a LOSS (another task, still
            // applicable). A repeat of the same one and the overwriting of a stale one stand next to it
            // as separate numbers: previously all three were added into one, and it
            // showed 85 % where the losses were single percent.
            crate::println!("stat: hints to the scheduler {}: overwritten by A LIVE FOREIGN one {} ({}%), by the same one {} ({}%), stale {} ({}%); applied by \
                the scheduler {}, by the pickup loop {}",
                st, cl, cl * 100 / st.max(1), sa, sa * 100 / st.max(1), sl, sl * 100 / st.max(1), us, pk);
        }
    }
    {   // THE "WOKEN -> RAN" LATENCY — what was missing in order to judge
        // the scheduler's changes: "it waited 5 ms" does not tell our latency from
        // a lawful wait on a predicate, while this histogram does.
        use core::sync::atomic::Ordering::Relaxed;
        let b: [u64; 8] = core::array::from_fn(|i| crate::sync::WAKE_LAT[i].load(Relaxed));
        let n: u64 = b.iter().sum();
        if n != 0 {
            crate::println!("stat: wakeup->cpu, us: <5:{} <20:{} <50:{} <100:{} <200:{} <500:{} <1000:{} 1000+:{}",
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]);
            crate::println!("stat:   {} wakeups in total, {} us on average, the worst {} us",
                n, crate::sync::WAKE_LAT_SUM.load(Relaxed) / n, crate::sync::WAKE_LAT_MAX.load(Relaxed));
        }
    }
    {   // READY BUT NOT RUNNING — the whole interval from readiness to landing on a core,
        // including THE PREEMPTED ones (their wakeup does not see it). That is exactly the answer to
        // "are the threads waiting for each other or for the processor" (ClickHouse, session 25).
        use core::sync::atomic::Ordering::Relaxed;
        let b: [u64; 8] = core::array::from_fn(|i| crate::sched::RQ_HIST[i].load(Relaxed));
        let n: u64 = b.iter().sum();
        if n != 0 {
            crate::println!("stat: ready->cpu, us: <5:{} <20:{} <50:{} <100:{} <200:{} <500:{} <1000:{} 1000+:{}",
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]);
            let w = crate::sched::RQ_MAX_TASK.load(Relaxed);
            let ctx = crate::sched::RQ_MAX_CTX.load(Relaxed);
            // The worst one is SINCE THE PREVIOUS stat BLOCK (session 32): the start of mongod gives
            // 17 ms on the fifth second and covered up the worst one under load.
            crate::println!("stat:   {} intervals in total, {} ms altogether, {} us on average, the worst since the previous block {} us (task {} on core {}, \
                it ended at {} ms; at the moment of readiness the core was executing {}{}, core switches during the wait {})",
                n, crate::sched::RQ_SUM_US.load(Relaxed) / 1000, crate::sched::RQ_SUM_US.load(Relaxed) / n, crate::sched::RQ_MAX_US.load(Relaxed),
                w as u32, w >> 32, crate::sched::RQ_MAX_AT_MS.load(Relaxed),
                if ctx & 0xFFFF == 0xFFFF { "nobody (hlt)" } else { "t" }, TaskNo(ctx & 0xFFFF), ctx >> 16);
            // The tail of the decisions of the waiter's core before the end of the worst interval (see `sched::RQ_MAX_EV`).
            let got = crate::sched::RQ_MAX_EV_N_GOT.load(Relaxed);
            let (n_ev, t_end) = ((got & 0xFF) as usize, got >> 8);
            if n_ev != 0 {
                let hz = crate::tsc::calibrate_cached().max(1);
                let occ = if ctx & 0xFFFF == 0xFFFF { crate::obs::comm(usize::MAX) } else { crate::obs::comm((ctx & 0xFFFF) as usize) };
                crate::print!("stat:   the occupant t{} \"{}\"; the core's decisions {} before the end of the wait:",
                    TaskNo(ctx & 0xFFFF), occ.as_str(), w >> 32);
                for j in 0..n_ev.min(crate::sched::RQ_MAX_EV_N) {
                    let e = unsafe { crate::sched::RQ_MAX_EV[j] };
                    let us = t_end.wrapping_sub(e.tsc).saturating_mul(1_000_000) / hz;
                    let why = match e.why & 0x7f { 1 => "hint", 2 => "a foreign hint->rotation", 3 => "rotation", 4 => "stay:lock", 5 => "stay:nobody",
                        6 => "stay:interception", 7 => "steal", 8 => "stay:it ran little", 9 => "stay:a request in progress", 10 => "stay:young", _ => "?" };
                    crate::print!(" [-{} {} t{}->t{} {}", us, if e.why & 0x80 != 0 { "tick" } else { "yield" }, e.from, e.to, why);
                    if e.hint != 255 { crate::print!(" h:t{}", e.hint); }
                    if e.why & 0x80 != 0 { if e.rip == 3 { crate::print!(" @r3"); } else if e.rip != 0 { crate::print!(" @{:#x}", e.rip); } }
                    else if e.rip != 0 { crate::print!(" from:{}", crate::sched::YO_NAMES[(e.rip as usize).min(crate::sched::YO_NAMES.len() - 1)]); }
                    crate::print!("]");
                }
                crate::println!();
            }
            crate::sched::RQ_MAX_US.store(0, Relaxed);
            crate::sched::RQ_MAX_EV_N_GOT.store(0, Relaxed);
            print_waiters("stat:  ");
        }
    }
    /// The task number after a "t"; 0xFFFF (the core was idle) — empty.
    struct TaskNo(u64);
    impl core::fmt::Display for TaskNo {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { if self.0 == 0xFFFF { Ok(()) } else { write!(f, "{}", self.0) } }
    }
    {   use core::sync::atomic::Ordering::Relaxed;
        let c = crate::syscall::MREMAP_CALLS.load(Relaxed);
        if c != 0 {
            crate::println!("stat: mremap: calls {}, in place {}, with a move {}, refusals {}",
                c, crate::syscall::MREMAP_INPLACE.load(Relaxed),
                crate::syscall::MREMAP_MOVED.load(Relaxed), crate::syscall::MREMAP_FAIL.load(Relaxed));
        }
    }
    {   // "ONE POLLS, THE REST SLEEP": how many times a network waiter fell asleep as
        // a NON-polling one. On a single-threaded server (redis, nginx) the waiter is one and
        // it is the poller too — here there must be a ZERO. On a server with a process per
        // connection (PostgreSQL) this number is exactly the work the machine
        // no longer spends on shuffling ready tasks around.
        use core::sync::atomic::Ordering::Relaxed;
        let d = crate::netsys::DISOWNED.load(Relaxed);
        if d != 0 { crate::println!("stat: net: the ownership was dropped at close {}", d); }
        let n = crate::netsys::NET_PARK_EXTRA.load(Relaxed);
        if n != 0 {
            // THE SLEEP'S OUTCOME IS THE MAIN NUMBER. "It fell asleep" by itself says
            // nothing: was it woken by a delivery (the mechanism works) or by the backstop of
            // 5 ms (the mechanism does not work, and the workload pays milli-
            // seconds for it) — those are two different diagnoses with the same first number.
            crate::println!("stat: net: waiters fell asleep as NON-polling ones {} (woken by a delivery {}, by the backstop {})",
                n, crate::netsys::NET_PARKX_WAKE.load(Relaxed), crate::netsys::NET_PARKX_TMO.load(Relaxed));
            crate::println!("stat: net: the poller left while waiters were alive {} times, the pass was picked up {}",
                crate::netsys::NET_POLLER_LEFT_BUSY.load(Relaxed), crate::netsys::NET_POLLER_HANDOFF.load(Relaxed));
        }
    }
    {
        use core::sync::atomic::Ordering::Relaxed;
        let (sp, so) = (crate::netsys::NET_MIX_SPINS[0].load(Relaxed), crate::netsys::NET_MIX_SPINS[1].load(Relaxed));
        if sp | so != 0 {
            crate::println!("stat: net: mixed-set waits spun: alone {}, beside another waiter {} (netmix {} ns parks the latter)",
                sp, so, crate::netsys::netmix_get());
        }
        let (st, lo) = (crate::netsys::NET_MIX_STEALS.load(Relaxed), crate::netsys::NET_MIX_LOST.load(Relaxed));
        if st | lo != 0 { crate::println!("stat: net: the slot taken by traffic {} times, holders that saw it gone {}", st, lo); }
        {   // the network waits' time by phase, summed over the tasks (`netsys::NW_PH_T`): a phase
            // owns the time to the next one recorded, so "the exit" runs on into ring 3 and the
            // next syscall. The stat block's difference is what a rung's waits cost by phase —
            // the tick cannot see it, the waits run with the interrupts masked (MEASUREMENTS §10.19).
            let mut ph = [0u64; 16];
            for t in 0..crate::sched::NTASKS { for k in 0..16 { ph[k] += crate::netsys::NW_PH_T[t][k].load(Relaxed); } }
            if ph.iter().any(|&v| v != 0) {
                let us = |c: u64| c / (crate::tsc::hz() / 1_000_000).max(1);
                crate::print!("stat: net: the waits by phase, us:");
                for k in 1..16 { if ph[k] != 0 { crate::print!(" {}={}", k, us(ph[k])); } }
                crate::println!(" (1 the entry 2 the top 3 the poll 4 the pass 5 the lock 6 the scan 7 the window 8 the spin 9 the yield 10 back 11 NIC 12 parked 13 the exit 14 read 15 write)");
            }
        }
        // Who held the polling slot (the top four by time): the one a mixed set leaves spinning.
        let hz = crate::tsc::hz().max(1);
        let mut top = [(0u64, usize::MAX); 4];
        let open = crate::netsys::poll_holder();
        for t in 0..crate::sched::NTASKS {
            let mut c = crate::netsys::POLL_HOLD_CYC[t].load(Relaxed);
            if let Some((h, cy)) = open { if h == t { c += cy; } }
            if c == 0 { continue; }
            let mut i = 4;
            while i > 0 && top[i - 1].0 < c { i -= 1; }
            if i < 4 { let mut j = 3; while j > i { top[j] = top[j - 1]; j -= 1; } top[i] = (c, t); }
        }
        for &(c, t) in top.iter() {
            if t == usize::MAX { break; }
            let n = crate::sched::comm(t); let l = crate::sched::comm_len(t);
            crate::println!("stat: net: the slot t{} proc {} ({}): held {} ms, mixed spins {}", t, crate::sched::proc_of(t),
                core::str::from_utf8(&n[..l]).unwrap_or("?"), c / (hz / 1000).max(1), crate::netsys::POLL_MIX_SPIN[t].load(Relaxed));
        }
        let m = crate::netsys::NET_MIX_PARKS.load(Relaxed);
        if m != 0 {
            crate::println!("stat: net: mixed-set waiters parked {} (woken {}, by the ceiling {}); non-socket kicks that woke one {}",
                m, crate::netsys::NET_MIX_WAKE.load(Relaxed), crate::netsys::NET_MIX_TMO.load(Relaxed), crate::netsys::NET_MIX_KICKS.load(Relaxed));
        }
        let (h, x) = (crate::netsys::NET_MIX_SPIN_HIT.load(Relaxed), crate::netsys::NET_MIX_SPIN_MISS.load(Relaxed));
        if h + x != 0 {
            crate::println!("stat: net: mixed-set watches before a park (netmixspin): the word moved {}, ran out or yielded {}", h, x);
        }
    }
    if crate::sched::RUNHIST_ON.load(core::sync::atomic::Ordering::Relaxed) {
        let mut line = [0u64; 9];
        for i in 0..9 { line[i] = crate::sched::RUNHIST[i].load(core::sync::atomic::Ordering::Relaxed); }
        crate::println!("stat: ready tasks at a tick: 0:{} 1:{} 2:{} 3:{} 4:{} 5:{} 6:{} 7:{} 8+:{}",
            line[0], line[1], line[2], line[3], line[4], line[5], line[6], line[7], line[8]);
        for i in 0..9 { line[i] = crate::sched::OCCHIST[i].load(core::sync::atomic::Ordering::Relaxed); }
        crate::println!("stat: CORES busy under them:    0:{} 1:{} 2:{} 3:{} 4:{} 5:{} 6:{} 7:{} 8+:{}",
            line[0], line[1], line[2], line[3], line[4], line[5], line[6], line[7], line[8]);
        let m = |i: usize| crate::sched::MISPLACED[i].load(core::sync::atomic::Ordering::Relaxed);
        crate::println!("stat: a bad layout: ticks {} (2+ ready on a core while a neighbour is empty), extra ready {}, of them pinned {}",
            m(0), m(1), m(2));
        let w = |i: usize| crate::sched::MIS_WHY[i].load(core::sync::atomic::Ordering::Relaxed);
        crate::println!("stat:   an extra ready one for an empty neighbour: pinned {}, executing {}, asleep {}, the mask {}, the balance of the assigned {}, \
            hot {}, would have stolen {}",
            w(0), w(1), w(2), w(3), w(4), w(5), w(6));
        let v = |i: usize| crate::affinity::STEAL_MIS[i].load(core::sync::atomic::Ordering::Relaxed);
        crate::println!("stat:   steal attempts under a bad layout {}: pinned {}, asleep {}, the mask {}, the balance of the assigned {}, hot {}, would have \
            stolen {}",
            v(0), v(1), v(2), v(3), v(4), v(5), v(6));
    }
    if crate::sched::STEAL_PUSH.load(core::sync::atomic::Ordering::Relaxed) {
        let p = |i: usize| crate::sched::PUSH_STAT[i].load(core::sync::atomic::Ordering::Relaxed);
        crate::println!("stat: the push: moves {}, the neighbour was woken by an IPI {}, it backed off {}, there is no candidate {}", p(0), p(1), p(2), p(3));
    }
    // ASKED / DID NOT FIND / REFUSED — three different diagnoses in one line.
    crate::println!("stat: stealing: asked {}, did not find a candidate {}, refused by the balance {}, cache-hot {}",
        crate::affinity::STEAL_TRIES.load(core::sync::atomic::Ordering::Relaxed),
        crate::affinity::STEAL_NOCAND.load(core::sync::atomic::Ordering::Relaxed),
        crate::affinity::STEAL_BUSY.load(core::sync::atomic::Ordering::Relaxed),
        crate::affinity::STEAL_HOT.load(core::sync::atomic::Ordering::Relaxed));
    {
        let sk = |i: usize| crate::affinity::STEAL_SKIP[i].load(core::sync::atomic::Ordering::Relaxed);
        crate::println!("stat: stealing, the rejection of candidates: executing {}, asleep {}, the mask {}, the donor has no queue {}, pinned {} (threads that \
            pinned themselves with a mask: {})",
            sk(0), sk(1), sk(2), sk(3), sk(4), crate::affinity::SELFPIN.load(core::sync::atomic::Ordering::Relaxed));
    }
    crate::println!("stat: work steals={}, transplants at a wakeup={}",
        crate::sched::STOLEN.load(core::sync::atomic::Ordering::Relaxed),
        crate::affinity::WAKE_MOVED.load(core::sync::atomic::Ordering::Relaxed));
    crate::println!("stat: 2 MB huge pages={} (reservations {}, broken up {}, none found in a row {})",
        crate::paging::HUGE_GIVEN.load(core::sync::atomic::Ordering::Relaxed),
        crate::paging::HUGE_RESERVED.load(core::sync::atomic::Ordering::Relaxed),
        crate::paging::HUGE_SPLIT.load(core::sync::atomic::Ordering::Relaxed),
        crate::paging::HUGE_NOMEM.load(core::sync::atomic::Ordering::Relaxed));
    // THE HARDWARE COUNTERS next to the rest. They are needed when one and the same workload on
    // one and the same core gives DIFFERENT numbers from boot to boot: the scheduler meanwhile
    // behaves identically (zero steals, an equal number of transplants), and telling "we compute
    // the same amount, but slower" from "we compute more" is possible only by IPC and
    // cache misses.
    if crate::pmu::available() {
        // The counters are taken FROM THE WORKING cores, not only from the ticking ones: see `refresh_all`.
        crate::pmu::refresh_all();
        for c in 0..crate::percpu::MAXCPU {
            let (sm, seq) = crate::pmu::last(c);
            if seq == 0 || sm.cycles == 0 { continue; }
            crate::println!("stat: core {}: cycles {} instructions {} IPC {}.{:02} LLC misses {}",
                c, sm.cycles, sm.instrs, sm.ipc100() / 100, sm.ipc100() % 100, sm.llc_miss);
        }
    }
    // The price of a cross-core TLB flush — next to the page counters rather than only in
    // `kstat`: for a workload without a console (ClickHouse, the JVM) there is nobody to ask
    // `kstat`, while waiting for an acknowledgement falls inside munmap/mprotect, that is,
    // straight into the request's latency.
    {   use core::sync::atomic::Ordering::Relaxed as R;
        let n = crate::paging::SHOOT_WAITS.load(R);
        if n != 0 {
            let hz = crate::tsc::calibrate_cached().max(1);
            let us = |c: u64| c.saturating_mul(1_000_000) / hz;
            crate::println!("stat: TLB flushes to neighbours {} (we waited {} ms, the worst {} us; longer than 100 us {}, 1 ms {}, did not acknowledge {})",
                n, us(crate::paging::SHOOT_WAIT_TSC.load(R)) / 1000,
                us(crate::paging::SHOOT_MAX_TSC.load(R)),
                crate::paging::SHOOT_OVER[1].load(R), crate::paging::SHOOT_OVER[2].load(R),
                crate::paging::SHOOT_TIMEOUT.load(R));
            crate::println!("stat: calls not made (the neighbour is in a foreign AS) {}, (the neighbour is in hlt, it will flush by itself) {}, flushes by the \
                mark at a wakeup {}; acknowledgements were not waited for (the neighbour was TAKEN OFF THE CORE by the host) {}, (the neighbour STANDS IN hlt) \
                {}",
                crate::paging::SHOOT_SKIPPED.load(R), crate::paging::SHOOT_LAZY.load(R), crate::paging::LAZY_FLUSHES.load(R),
                crate::paging::SHOOT_HOSTFLUSH.load(R), crate::paging::SHOOT_HALTED.load(R));
        }
    }
    // The Contiguous hint (arm64; `talaria.cont=0` — off): 64 KB groups the loader folded, the
    // writes that had to unfold one, the TLB conflict aborts resolved meanwhile.
    if crate::paging::cont_on() || crate::paging::cont_ever() {
        use core::sync::atomic::Ordering::Relaxed as R;
        crate::print!("stat: tlb: contiguous 64 KB groups folded {}, unfolded by a write {}, TLB conflicts resolved {}; the unfolds by line:",
            crate::paging::CONT_FOLDS.load(R), crate::paging::CONT_UNFOLDS.load(R), crate::paging::CONT_CONFLICTS.load(R));
        for i in 0..crate::paging::CU_N {
            let h = crate::paging::CU_HITS[i].load(R);
            if h != 0 { crate::print!(" {}:{}", crate::paging::CU_LINE[i].load(R), h); }
        }
        crate::println!();
    }
    // The breakdown of the price of removing a mapping: the walk / the local flush / the acknowledgements.
    {   use core::sync::atomic::Ordering::Relaxed as R;
        let n = crate::paging::UNMAP_CALLS.load(R);
        if n != 0 {
            let hz = crate::tsc::calibrate_cached().max(1);
            let ns = |c: u64| c.saturating_mul(1_000_000_000 / 1000) / (hz / 1000).max(1);
            let hit = crate::paging::UNMAP_HIT.load(R).max(1);
            {   // the full TLB flushes by the caller's line in paging.rs
                use crate::paging::{FF_LINE, FF_HITS, FF_N, FF_OTHER};
                let tot: u64 = (0..FF_N).map(|k| FF_HITS[k].load(R)).sum::<u64>() + FF_OTHER.load(R);
                if tot != 0 {
                    crate::print!("stat: tlb: full flushes {}, by the caller's line in paging.rs:", tot);
                    for k in 0..FF_N { let h = FF_HITS[k].load(R); if h != 0 { crate::print!(" {}:{}", FF_LINE[k].load(R), h); } }
                    crate::println!(" other {}; mmap reservations without a flush {}", FF_OTHER.load(R),
                        crate::paging::LAZY_MAP_NOFLUSH.load(R));
                }
            }
            crate::println!("stat: unmap: calls {} ({} with a removal), pages {} ({} per call); the walk {} ns/call, the flush {} ns, the acknowledgements {} \
                ns (per removal)",
                n, crate::paging::UNMAP_HIT.load(R), crate::paging::UNMAP_PAGES.load(R),
                crate::paging::UNMAP_PAGES.load(R) / n,
                ns(crate::paging::UNMAP_WALK_TSC.load(R) / n),
                ns(crate::paging::UNMAP_FLUSH_TSC.load(R) / hit),
                ns(crate::paging::UNMAP_SHOOT_TSC.load(R) / hit));
        }
    }
    // The fate of the connections: how many were accepted, closed and how many backlog slots
    // died before accept (see netsys::reap_backlog — previously they piled up silently).
    {   use core::sync::atomic::Ordering::Relaxed as R;
        let (a, c, d) = (crate::netsys::ACCEPTED.load(R), crate::netsys::CLOSED.load(R), crate::netsys::BACKLOG_DEAD.load(R));
        let (hi, no) = (crate::netsys::SLOTS_HIGH.load(R), crate::netsys::SLOTS_NONE.load(R));
        if a + c + d + hi != 0 {
            crate::println!("stat: tcp: accepted {}, closed {}, the backlog died before accept and was returned {}", a, c, d);
            // THE TABLE'S CEILING is the machine's only hard limit on the number of
            // connections, and after the fact nothing reveals it: by the time of the analysis the slots
            // are already free. The high-water mark answers "did we come close to the ceiling", while
            // the layout NOW answers "what is it busy with". Without this line a refusal to accept
            // after an overload on the stand was indistinguishable from a refusal of the network.
            let (used, lis, bl, conn, cl, unb) = crate::netsys::census();
            crate::println!("stat: tcp slots: now {} of {} (listeners {}, backlog {}, connections {}, closed but not untied {}, created but not tied {}), the \
                high-water mark {}, slot refusals {}",
                used, crate::netsys::NS, lis, bl, conn, cl, unb, hi, no);
        }
    }
    // The delivery of a frame to the socket's owner: to whom the processor went (see netsys::deliver_ready).
    {   use core::sync::atomic::Ordering::Relaxed as R;
        let (a, b, c, d) = (crate::netsys::DELIV_NOOWNER.load(R), crate::netsys::DELIV_SELF.load(R),
                            crate::netsys::DELIV_SAMECPU.load(R), crate::netsys::DELIV_OTHERCPU.load(R));
        if a + b + c + d != 0 {
            crate::println!("stat: the socket's delivery: without an owner {}, already mine {}, to a neighbour on this core {}, on a foreign core {}", a, b, c,
                d);
        }
    }
    // DELIVERY -> THE READER BY THE READER'S STATE (session 32): in what state the socket's
    // owner was when the data landed in the buffer, and for how long it did not pick them up.
    // The class that has the tail also names the hypothesis: see `netsys::DLC_NAMES`.
    {   use core::sync::atomic::Ordering::Relaxed as R;
        let ba: [u64; 8] = core::array::from_fn(|i| crate::netsys::DLA_HIST[i].load(R));
        let na: u64 = ba.iter().sum();
        if na != 0 {
            crate::println!("stat: the ring->the pass (an upper estimate, passes with frames {}), us: <5:{} <10:{} <20:{} <50:{} <100:{} <200:{} <500:{} \
                500+:{} the worst {}",
                na, ba[0], ba[1], ba[2], ba[3], ba[4], ba[5], ba[6], ba[7], crate::netsys::DLA_MAX_US.load(R));
        }
        {   // session 32: the gap between passes with frames and the latency of completing a transmission
            let g0: [u64; 8] = core::array::from_fn(|i| crate::netsys::RXGAP_HIST[0][i].load(R));
            let g1: [u64; 8] = core::array::from_fn(|i| crate::netsys::RXGAP_HIST[1][i].load(R));
            let fr: [u64; 3] = core::array::from_fn(|i| crate::netsys::RXGAP_FRAMES[i].load(R));
            if g0.iter().sum::<u64>() + g1.iter().sum::<u64>() != 0 {
                crate::println!("stat: the gap between passes with frames, us: <50:{} <100:{} <150:{} <200:{} <250:{} <300:{} <500:{} 500+:{}; frames per pass \
                    1:{} 2:{} 3+:{}",
                    g0[0] + g1[0], g0[1] + g1[1], g0[2] + g1[2], g0[3] + g1[3], g0[4] + g1[4], g0[5] + g1[5], g0[6] + g1[6], g0[7] + g1[7], fr[0], fr[1],
                        fr[2]);
                crate::println!("stat:   of them a pass by the timers' debt: <50:{} <100:{} <150:{} <200:{} <250:{} <300:{} <500:{} 500+:{}",
                    g1[0], g1[1], g1[2], g1[3], g1[4], g1[5], g1[6], g1[7]);
            }
            let tc: [u64; 8] = core::array::from_fn(|i| crate::ena::TXC_HIST[i].load(R));
            if tc.iter().sum::<u64>() != 0 {
                crate::println!("stat: the completion of a transmission (the posting -> the completion), us: <5:{} <10:{} <20:{} <50:{} <100:{} <150:{} \
                    <200:{} 200+:{} the worst {}",
                    tc[0], tc[1], tc[2], tc[3], tc[4], tc[5], tc[6], tc[7], crate::ena::TXC_MAX_US.load(R));
                crate::println!("stat: the card under a pass (session 32): a call waited for the holder {} times, gave up {}; the receive waited {} times, \
                    gave up {}",
                    crate::l1::FLUSH_WAITED.load(R), crate::l1::FLUSH_GAVEUP.load(R), crate::l1::RX_TAKE_WAITED.load(R), crate::l1::RX_TAKE_GAVEUP.load(R));
            }
        }
        let mut hdr = false;
        for c in 0..crate::netsys::DLC_N {
            let b: [u64; 8] = core::array::from_fn(|i| crate::netsys::DL_HIST[c][i].load(R));
            let n: u64 = b.iter().sum();
            if n == 0 { continue; }
            if !hdr { crate::println!("stat: delivery->the reader by the reader's state at the delivery, us: <5 <10 <20 <50 <100 <200 <500 500+"); hdr = true; }
            let ctx = crate::netsys::DL_MAX_CTX[c].load(R);
            crate::println!("stat:   {:<22} {:>8}: {} {} {} {} {} {} {} {}  avg {} the worst {} (on the reader's core {}{}, switches {}), the reader's core \
                stood empty {}",
                crate::netsys::DLC_NAMES[c], n, b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
                crate::netsys::DL_SUM_US[c].load(R) / n, crate::netsys::DL_MAX_US[c].load(R),
                if ctx & 0xFFFF == 0xFFFF { "hlt" } else { "t" }, TaskNo(ctx & 0xFFFF), ctx >> 16,
                crate::netsys::DL_IDLE[c].load(R));
            let occ: [u64; 3] = core::array::from_fn(|i| crate::netsys::DLS_OCC[c][i].load(R));
            let sw: [u64; 4] = core::array::from_fn(|i| crate::netsys::DLS_SW[c][i].load(R));
            let ns: u64 = occ.iter().sum();
            if ns != 0 {
                crate::println!("stat:     slow ones (>={} us) {}: on the reader's core hlt {} / a network waiter {} / other {}; core switches during the wait \
                    0:{} 1:{} 2:{} 3+:{}; the reader is invisible to the scheduler {}",
                    crate::netsys::DL_SLOW_US, ns, occ[0], occ[1], occ[2], sw[0], sw[1], sw[2], sw[3],
                    crate::netsys::DLS_HIDDEN[c].load(R));
                let ran: [u64; 4] = core::array::from_fn(|i| crate::netsys::DLS_RAN[c][i].load(R));
                let osw: [u64; 4] = core::array::from_fn(|i| crate::netsys::DLS_OWNSW[c][i].load(R));
                let np: [u64; 4] = core::array::from_fn(|i| crate::netsys::DLS_PASS[c][i].load(R));
                crate::println!("stat:       the reader during the wait: ran <10%:{} <50%:{} <90%:{} >=90%:{}; taken off the core 0:{} 1:{} 2:{} 3+:{}; passes \
                    from read in vain 0:{} 1-2:{} 3-10:{} 11+:{}",
                    ran[0], ran[1], ran[2], ran[3], osw[0], osw[1], osw[2], osw[3], np[0], np[1], np[2], np[3]);
            }
        }
        if hdr {
            // Who occupied the reader's core during the slow deliveries — per task.
            let mut any = false;
            for t in 0..crate::sched::NTASKS {
                let n = crate::netsys::DLS_TASK[t].load(R);
                if n == 0 { continue; }
                if !any { crate::print!("stat:   slow deliveries — who was on the reader's core:"); any = true; }
                crate::print!(" t{}:{}", t, n);
            }
            if any { crate::println!(); }
            // THE SERVER'S TURNAROUND ON A SOCKET (see `netsys::tr_close`).
            {
                use crate::netsys::{TR_HIST, TR_N, TR_ST, TR_ST_NAMES, TR_SUM, TR_SLOW_SUM, TR_SLOW_N, TR_SLOW_TOP, TR_EX, TR_EX_N, TR_MID_SUM, TR_MID_N,
                    TR_MID_TOP};
                let h: [u64; TR_N] = core::array::from_fn(|i| TR_HIST[i].load(R));
                let n: u64 = h.iter().sum();
                if n != 0 {
                    crate::println!("stat:   the server's turnaround on a socket (the delivery -> the answer handed to the card) {}, us: <20:{} <40:{} <60:{} \
                        <80:{} <100:{} <150:{} <200:{} 200+:{}",
                        n, h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
                    crate::print!("stat:     the average intervals, us:");
                    for k in 0..TR_ST { crate::print!(" {} {}", TR_ST_NAMES[k], TR_SUM[k].load(R) / n); }
                    crate::println!();
                    {   // the same since the previous block, ns: one block = one rung
                        use crate::netsys::{TR_SUM_NS, TR_TOT_NS};
                        static PREV: [AtomicU64; TR_ST + 2] = [const { AtomicU64::new(0) }; TR_ST + 2];
                        let cur: [u64; TR_ST + 2] = core::array::from_fn(|k| match k {
                            0 => n, 1 => TR_TOT_NS.load(R), _ => TR_SUM_NS[k - 2].load(R) });
                        let d: [u64; TR_ST + 2] = core::array::from_fn(|k| cur[k].wrapping_sub(PREV[k].swap(cur[k], R)));
                        if d[0] != 0 {
                            crate::print!("stat:     since the previous block {} turnarounds, ns on average: the whole {}", d[0], d[1] / d[0]);
                            for k in 0..TR_ST { crate::print!(" {} {}", TR_ST_NAMES[k], d[k + 2] / d[0]); }
                            crate::println!();
                        }
                    }
                    {   // the delivery->read split since the previous block (netsys::dr_close, talaria.netdr=1)
                        use crate::netsys::{DR_SUM, DR_CNT, DR_PARKED, DR_SAMECORE, DR_NOEP, DR_NOEP_NS, DR_N_SEG, DR_SEG_NAMES};
                        const M: usize = DR_N_SEG + 5;
                        static PREV: [AtomicU64; M] = [const { AtomicU64::new(0) }; M];
                        let cur: [u64; M] = core::array::from_fn(|k| match k {
                            0 => DR_CNT.load(R), 1 => DR_PARKED.load(R), 2 => DR_SAMECORE.load(R), 3 => DR_NOEP.load(R),
                            4 => DR_NOEP_NS.load(R), _ => DR_SUM[k - 5].load(R) });
                        let d: [u64; M] = core::array::from_fn(|k| cur[k].wrapping_sub(PREV[k].swap(cur[k], R)));
                        if d[0] != 0 {
                            crate::print!("stat:     the delivery->the read split since the previous block {} (a parked wake {}, delivered by the reader's core {}; \
                                without an epoll {} of {} ns), ns on average:", d[0], d[1], d[2], d[3], d[4] / d[3].max(1));
                            for k in 0..DR_N_SEG { crate::print!(" {} {}", DR_SEG_NAMES[k], d[k + 5] / d[0]); }
                            crate::println!();
                        }
                    }
                    let mn = TR_MID_N.load(R);
                    if mn != 0 {
                        crate::print!("stat:     average ones (60..99 us) {}: the average intervals", mn);
                        for k in 0..TR_ST { crate::print!(" {} {}", TR_ST_NAMES[k], TR_MID_SUM[k].load(R) / mn); }
                        crate::print!("; the largest interval:");
                        for k in 0..TR_ST { crate::print!(" {} {}", TR_ST_NAMES[k], TR_MID_TOP[k].load(R)); }
                        crate::println!();
                    }
                    {   // the read->write split (netsys::TR_SPLIT): on a core / ready off a core / asleep
                        use crate::netsys::{TR_SPLIT, TR_SPLIT_N, TR_SPLIT_NAMES};
                        let k = TR_SPLIT_N.load(R);
                        if k != 0 {
                            let (mn, sn) = (TR_MID_N.load(R).max(1), TR_SLOW_N.load(R).max(1));
                            crate::print!("stat:     the read->the write split, us on average (all {} / average ones / slow ones):", k);
                            for j in 0..4 { crate::print!(" {} {}/{}/{}", TR_SPLIT_NAMES[j], TR_SPLIT[0][j].load(R) / k,
                                TR_SPLIT[1][j].load(R) / mn, TR_SPLIT[2][j].load(R) / sn); }
                            crate::print!(" page faults per 100 {}/{}/{}", TR_SPLIT[0][4].load(R) * 100 / k,
                                TR_SPLIT[1][4].load(R) * 100 / mn, TR_SPLIT[2][4].load(R) * 100 / sn);
                            crate::println!();
                        }
                    }
                    let sn = TR_SLOW_N.load(R);
                    if sn != 0 {
                        crate::print!("stat:     slow ones (>=100 us) {}: the average intervals", sn);
                        for k in 0..TR_ST { crate::print!(" {} {}", TR_ST_NAMES[k], TR_SLOW_SUM[k].load(R) / sn); }
                        crate::print!("; the largest interval:");
                        for k in 0..TR_ST { crate::print!(" {} {}", TR_ST_NAMES[k], TR_SLOW_TOP[k].load(R)); }
                        crate::println!();
                        for k in 0..TR_EX_N {
                            let e = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(TR_EX[k])) };
                            if e.us == 0 { continue; }
                            crate::println!("stat:     the worst turnaround {} us: the reader t{} ({}), core {}, delivered by core {}, on the reader's core \
                                there was t{}; the intervals {} {} / {} {} / {} {} / {} {} / {} {}",
                                e.us, e.t, crate::netsys::DLC_NAMES[(e.c as usize).min(crate::netsys::DLC_N - 1)], e.cpu, e.by, e.ot,
                                TR_ST_NAMES[0], e.st[0], TR_ST_NAMES[1], e.st[1], TR_ST_NAMES[2], e.st[2], TR_ST_NAMES[3], e.st[3], TR_ST_NAMES[4], e.st[4]);
                        }
                    }
                }
            }
            // THE OUTCOMES OF THE YIELD WINDOW OF NETWORK WAITERS (the sum per task, see `netsys::NW_WHY`).
            crate::print!("stat:   the yield window of network waiters:");
            for k in 0..crate::netsys::WHY_N {
                let mut n: u64 = 0;
                for t in 0..crate::sched::NTASKS { n += crate::netsys::NW_WHY[t][k].load(R) as u64; }
                crate::print!(" {} {}", crate::netsys::WHY_NAMES[k], n);
            }
            crate::println!();
            // THE ANALYSIS OF INDIVIDUAL SLOW DELIVERIES: the decisions of the scheduler of the reader's
            // core since the moment of the delivery (see `netsys::DlDump`).
            let hz = crate::tsc::hz().max(1);
            for k in 0..crate::netsys::DL_DUMP_N {
                let d = unsafe { &mut *core::ptr::addr_of_mut!(crate::netsys::DL_DUMP[k]) };
                if !d.used.load(core::sync::atomic::Ordering::Acquire) { continue; }
                crate::println!("stat:   the analysis: the reader t{} ({}), {} us, core {}, delivered by core {}, on the reader's core there was t{} ({}){}, \
                    the reader ran {} us, the core switched {} times; the core's decisions {}:",
                    d.t, crate::netsys::DLC_NAMES[(d.c as usize).min(crate::netsys::DLC_N - 1)], d.us, d.cpu, d.by,
                    d.ot, match d.occ & 3 { 0 => "hlt", 1 => "a network waiter", _ => "other" }, if d.occ & 4 != 0 { " (the reader is invisible)" } else { "" },
                        d.ran_us, d.sw, d.cpu);
                crate::print!("stat:     ");
                for j in 0..(d.n as usize).min(crate::netsys::DL_DUMP_EV) {
                    let e = d.ev[j];
                    let dt = e.tsc.wrapping_sub(d.dl);
                    let (sg, us) = if dt < (1u64 << 62) { ("+", dt.saturating_mul(1_000_000) / hz) } else { ("-",
                        d.dl.wrapping_sub(e.tsc).saturating_mul(1_000_000) / hz) };
                    let why = match e.why & 0x7f { 1 => "hint", 2 => "a foreign hint->rotation", 3 => "rotation", 4 => "stay:lock", 5 => "stay:nobody",
                        6 => "stay:interception", 7 => "steal", 8 => "stay:it ran little", 9 => "stay:a request in progress", 10 => "stay:young", _ => "?" };
                    crate::print!("[{}{} {} t{}->t{} {}", sg, us, if e.why & 0x80 != 0 { "tick" } else { "yield" }, e.from, e.to, why);
                    if e.hint != 255 { crate::print!(" h:t{}", e.hint); }
                    // Where the tick caught the interrupted task: ring 3 or an address in the kernel.
                    if e.why & 0x80 != 0 { if e.rip == 3 { crate::print!(" @r3"); } else if e.rip != 0 { crate::print!(" @{:#x}", e.rip); } }
                    else if e.rip != 0 { crate::print!(" from:{}", crate::sched::YO_NAMES[(e.rip as usize).min(crate::sched::YO_NAMES.len() - 1)]); }
                    crate::print!("]");
                }
                crate::println!();
                // THE WAITER'S PHASES (see `netsys::NW_PH`): the reader's and those of the one occupying its core.
                let ph_line = |who: &str, t: u8, ev: &[crate::netsys::PhEv], n: usize| {
                    if n == 0 { return; }
                    crate::print!("stat:     the phases of {} t{}:", who, t);
                    for j in 0..n.min(crate::netsys::DL_DUMP_PH) {
                        let e = ev[j];
                        let dt = e.tsc.wrapping_sub(d.dl);
                        let (sg, us) = if dt < (1u64 << 62) { ("+", dt.saturating_mul(1_000_000) / hz) } else { ("-",
                            d.dl.wrapping_sub(e.tsc).saturating_mul(1_000_000) / hz) };
                        crate::print!(" [{}{} {}", sg, us, crate::netsys::PH_NAMES[(e.ph as usize).min(crate::netsys::PH_NAMES.len() - 1)]);
                        if e.n > 1 { crate::print!("x{}", e.n); }
                        crate::print!("]");
                    }
                    crate::println!();
                };
                ph_line("the reader", d.t, &d.pr, d.npr as usize);
                ph_line("the occupant", d.ot, &d.po, d.npo as usize);
                // WHERE THE OCCUPANT SPENT THE WAIT FOR THE READER: us per phase (the difference of the
                // accumulators `NW_PH_T`) and the outcomes of the yield window (`NW_WHY`).
                if d.ot != 255 && d.ot != d.t {
                    crate::print!("stat:     the occupant t{} during the wait, us per phase:", d.ot);
                    for k in 0..16 { if d.opt[k] != 0 { crate::print!(" {} {}", crate::netsys::PH_NAMES[k], d.opt[k]); } }
                    crate::print!("; the yield window:");
                    for k in 0..crate::netsys::WHY_N { if d.owhy[k] != 0 { crate::print!(" {} {}", crate::netsys::WHY_NAMES[k], d.owhy[k]); } }
                    crate::println!();
                }
                d.used.store(false, core::sync::atomic::Ordering::Release);
            }
            let m = crate::netsys::DL_DUMP_MISSED.load(R);
            let f = crate::netsys::DL_DUMP_SKIP_FLUSH.swap(0, R);
            if m != 0 || f != 0 { crate::println!("stat:   analyses that did not fit: {}, skipped while the block was being flushed: {}", m, f); }
        }
    }
    // HOW MUCH TIME THE HOST TOOK. Without this line any pause of its own reads as
    // our latency (see smp::steal_enable_here).
    if crate::smp::steal_on() {
        for c in 0..crate::percpu::MAXCPU {
            if crate::percpu::ONLINE.load(core::sync::atomic::Ordering::SeqCst) & (1 << c) == 0 { continue; }
            let ns = crate::smp::steal_ns(c);
            crate::println!("stat: core {}: the host took {} ms{}", c, ns / 1_000_000,
                if crate::smp::vcpu_preempted(c) { " (it is taken off the core right now)" } else { "" });
        }
    }
    {   // THE NETWORK STACK'S LOCK GOES INTO THE `stat:` BLOCK, NOT ONLY INTO `kstat`.
        //
        // On EC2 there is no console at all, and everything that is not in this block is
        // invisible on the stand. The stack's lock is ONE per machine, and the question "are those
        // percent of requests that pay the surcharge not in it" cannot be settled without these numbers.
        use core::sync::atomic::Ordering::Relaxed as R;
        let t = crate::netsys::NET_TAKES.load(R);
        if t != 0 {
            let hz = crate::tsc::calibrate_cached().max(1);
            let ms = |c: u64| c / (hz / 1000).max(1);
            let us = |c: u64| c.saturating_mul(1_000_000) / hz;
            let c = crate::netsys::NET_CONT.load(R);
            let h = &crate::netsys::NET_HELD_HIST;
            crate::println!("stat: the stack's lock: takes {}, with a wait {} ({}%), we waited {} ms, we held {} ms (the worst {} us); holds <10 us {}, <100 \
                us {}, <1 ms {}, >=1 ms {}",
                t, c, c * 100 / t, ms(crate::netsys::NET_WAIT_TSC.load(R)),
                ms(crate::netsys::NET_HELD_TSC.load(R)), us(crate::netsys::NET_HELD_MAX.load(R)),
                h[0].load(R), h[1].load(R), h[2].load(R), h[3].load(R));
            // by site (netsys::NET_SITE), cumulative: takes / held us / waited us
            crate::print!("stat: the stack's lock by site (takes/held us/waited us):");
            for e in crate::netsys::NET_SITE.iter() {
                let k = e.0.load(R);
                if k == u32::MAX { continue; }
                let base = k & !crate::netsys::NET_SITE_PASS;
                let pass = if k & crate::netsys::NET_SITE_PASS != 0 { "+pass" } else { "" };
                match base {
                    0x1_0000 => crate::print!(" tick{}", pass),
                    0x2_0000 => crate::print!(" line{}", pass),
                    0xffff => crate::print!(" notask{}", pass),
                    n => crate::print!(" sys{}{}", n, pass),
                }
                crate::print!("={}/{}/{}", e.1.load(R), us(e.2.load(R)), us(e.3.load(R)));
            }
            crate::println!();
            // who held the lock while a site waited (netsys::NET_WAITBY, talaria.netdr=1), us since the previous block
            {
                use crate::netsys::{NET_SITE, NET_SITE_N, NET_SITE_PASS, NET_WAITBY};
                static PREV: [[AtomicU64; NET_SITE_N + 1]; NET_SITE_N] = [const { [const { AtomicU64::new(0) }; NET_SITE_N + 1] }; NET_SITE_N];
                let name = |r: usize| -> (u32, &'static str) {
                    if r >= NET_SITE_N { return (0, "unpublished"); }
                    let k = NET_SITE[r].0.load(R);
                    let pass = if k & NET_SITE_PASS != 0 { "+pass" } else { "" };
                    (k & !NET_SITE_PASS, pass)
                };
                let pr = |r: usize| {
                    let (k, p) = name(r);
                    if r >= NET_SITE_N { crate::print!("{}", p); return; }
                    match k { 0x1_0000 => crate::print!("tick{}", p), 0x2_0000 => crate::print!("line{}", p),
                        0xffff => crate::print!("notask{}", p), n => crate::print!("sys{}{}", n, p) }
                };
                let mut hdr = false;
                for w in 0..NET_SITE_N {
                    let d: [u64; NET_SITE_N + 1] = core::array::from_fn(|h| { let c = NET_WAITBY[w][h].load(R); c.wrapping_sub(PREV[w][h].swap(c, R)) });
                    if d.iter().all(|&x| x == 0) { continue; }
                    if !hdr { crate::println!("stat: the lock's wait by the holder's site since the previous block (the waiter: the holder=us)"); hdr = true; }
                    crate::print!("stat:   "); pr(w); crate::print!(":");
                    for h in 0..=NET_SITE_N { if d[h] != 0 { crate::print!(" "); pr(h); crate::print!("={}", us(d[h])); } }
                    crate::println!();
                }
            }
            // the write's pass by phase (netsys::WR_PH), cumulative ns
            let w = &crate::netsys::WR_PH;
            let ns = |c: u64| c.saturating_mul(1_000) / (hz / 1_000_000).max(1);
            crate::println!("stat: the write's pass (cumulative): writes {} ns: the copy {} the preamble {} the poll {} the flush {} the tail {}; frames received {}",
                w[0].load(R), ns(w[1].load(R)), ns(w[2].load(R)), ns(w[3].load(R)), ns(w[4].load(R)), ns(w[5].load(R)), w[6].load(R));
            let x = &crate::netsys::WR_TX;
            crate::println!("stat: the write's transmit (cumulative): frames {} ns: the slot {} the formatting {} the record {} (reaping {} the copy {} the fence {})",
                x[0].load(R), ns(x[1].load(R)), ns(x[2].load(R)), ns(x[3].load(R)), ns(x[4].load(R)), ns(x[5].load(R)), ns(x[6].load(R)));
        }
    }
    // Cancelled idle passes of the stack and the trust in the "the card is silent" sign.
    {   use core::sync::atomic::Ordering::Relaxed as R;
        let sk = crate::netsys::POLL_SKIPPED.load(R);
        if sk != 0 {
            crate::println!("stat: idle passes of the stack cancelled {} (by the timers' debt {}), the ring lied {}",
                sk, crate::netsys::POLL_HARD_RUNS.load(R), crate::netsys::RING_LIED.load(R));
        }
        // A CORE WITHOUT WORK DROVE THE STACK — and how many times that brought a frame.
        // The second number is exactly the mechanism's benefit: these are the frames that would otherwise
        // have waited for the tick of the core where their waiter sits (see `netsys::idle_poll`).
        // THE REJECTION OF THE SET SCAN: how many iterations of the waiting loop went WITHOUT
        // the machine lock. That is the direct price of waiting: before the rejection there were 169 of them per
        // request (991 million over a mysql run).
        let (sd, ss) = (crate::netsys::SCAN_DONE.load(R), crate::netsys::SCAN_SKIPPED.load(R));
        if sd + ss != 0 {
            crate::println!("stat: the set scan by a waiter: under the lock {}, rejected without the lock {} ({}%), of them a mixed set {}",
                sd, ss, ss.saturating_mul(100) / (sd + ss).max(1), crate::netsys::SCAN_SKIPPED_MIX.load(R));
        }
        let ip = crate::netsys::IDLE_POLLS.load(R);
        if ip != 0 {
            let rx = crate::netsys::IDLE_POLL_RX.load(R);
            crate::println!("stat: an idle pass by an empty core: windows {}, of them with a frame {} ({}%)",
                ip, rx, rx.saturating_mul(100) / ip.max(1));
        }
    }
    // Is the card's line alive. For a workload without a console there is nobody to ask `kstat`, and
    // without this number there is nothing to analyse the waiter's parking with: "a delivery woke it"
    // and "an interrupt arrived" are different statements, and the second one is the more primary.
    {   use core::sync::atomic::Ordering::Relaxed as R;
        let n = crate::netsys::NIC_IRQS.load(R);
        let g = crate::netsys::NETIRQ_GSI.load(R);
        if n != 0 || g != u32::MAX {
            crate::println!("stat: interrupts from the card {} (the stack was busy {}), the line {}",
                n, crate::netsys::NIC_IRQ_BUSY.load(R), crate::netsys::netirq_line_name());
        }
    }
    // The parking of a waiter on a socket: with what exactly the waits ended. With a delivery —
    // that means a frame woke the owner (the card's line works); with the ceiling — that means
    // there was nobody to wake it and the waiter slept through the insurance (see netsys::wait_frame).
    crate::netprof::dump_app();   // `talaria.appprof=1`: the read->write stage per thread
    crate::umark::dump();         // the slow intervals a workload marked (prctl PR_TALARIA_SLOW)
    // The pull of a ready thread by a waiter (`talaria.wakepull=1`): how many readers
    // moved to the spinner, and why the rest were not taken.
    {   use core::sync::atomic::Ordering::Relaxed as R;
        let (ok, lost, w, pin, run) = (crate::netsys::PULL_OK.load(R), crate::netsys::PULL_LOST.load(R),
            crate::netsys::PULL_SKIP_WAITER.load(R), crate::netsys::PULL_SKIP_PIN.load(R), crate::netsys::PULL_SKIP_RUN.load(R));
        if crate::netsys::WAKEPULL.load(R) || ok + lost + w + pin + run != 0 {
            crate::println!("stat: the pull of a ready thread by a waiter: took {} (of them for the sake of the balance {}), backed off {} (the scheduler had \
                already entered), did not take: on its core there is a network waiter {}, the affinity {}, already executing {}",
                ok, crate::netsys::PULL_BAL.load(R), lost, w, pin, run);
        }
    }
    crate::sched::dump_cores();   // the cores right now and the starving ready ones (see sched.rs)
    {   use core::sync::atomic::Ordering::Relaxed as R;
        let p = crate::netsys::NET_PARKS.load(R);
        if p != 0 {
            crate::println!("stat: parkings of a waiter {}, woken by a delivery {}, by the ceiling {} ({}%), calls from a delivery {}",
                p, crate::netsys::NET_PARK_WAKE.load(R), crate::netsys::NET_PARK_TMO.load(R),
                crate::netsys::NET_PARK_TMO.load(R) * 100 / p, crate::netsys::NET_DELIV_WAKE.load(R));
        }
        // "99 % BY THE CEILING" BY ITSELF means nothing: the ceiling is fed by
        // idle waiters. The defect is this one: we left by the ceiling (no matter whether by the
        // parking one or by the backstop), WHILE THE DATA WERE ALREADY THERE — the delivery did not
        // call the one whose frame had arrived. IT IS PRINTED SEPARATELY from the parking
        // block: with `talaria.netsolo=1` the waiters go along A DIFFERENT branch, there are zero parkings,
        // and the line hid exactly where it is needed most.
        let (late, idle) = (crate::netsys::NET_PARK_LATE.load(R), crate::netsys::NET_PARK_IDLE_TMO.load(R));
        if late + idle != 0 {
            crate::println!("stat: we left by the ceiling WITH DATA in hand {} ({}%), in vain {} — the first one is exactly the delivery that did not call",
                late, late * 100 / (late + idle), idle);
        }
    }
    {   let (h, m) = crate::ext2::cache_stats();
        if h + m != 0 {
            crate::println!("stat: the FS block cache: hits {}, misses {} ({}% past it), the size {} MB",
                h, m, m * 100 / (h + m), crate::ext2::cache_mb());
        }
    }
    // THE DISK'S PRICE BY KIND. Waiting for the device goes synchronously and UNDER THE FS LOCK,
    // so it is exactly this breakdown that answers the question "what are the
    // milliseconds of holding the lock made of".
    {   use core::sync::atomic::Ordering::Relaxed as R;
        let hz = crate::tsc::calibrate_cached().max(1);
        let ms = |c: u64| c / (hz / 1000).max(1);
        let us = |c: u64| c.saturating_mul(1_000_000) / hz;
        let (rn, wn, fln) = (crate::fs::DEV_RD_N.load(R), crate::fs::DEV_WR_N.load(R), crate::fs::DEV_FL_N.load(R));
        if rn + wn + fln != 0 {
            let dt = crate::fs::DEV_TAKES.load(R);
            if dt != 0 {
                // Waiting for THE DEVICE is separated from waiting for a state: what used to stand
                // on the FS lock has moved here (see ext2::dev_barrier).
                crate::println!("stat: the device's lock: takes {}, with a wait {} ({}%), we waited {} ms, longer than a second {}, bypasses from a panic {}, \
                    foreign ones on the owner's core {}, the owner moved {}, the waiter gave up the core {}; the FS lock was released to the disk {} times",
                    dt, crate::fs::DEV_CONT.load(R), crate::fs::DEV_CONT.load(R) * 100 / dt,
                    ms(crate::fs::DEV_WAIT_TSC.load(R)),
                    crate::fs::DEV_TIMEOUT.load(R), crate::fs::DEV_BYPASS.load(R),
                    crate::fs::DEV_REENTER_ALIEN.load(R), crate::fs::DEV_RECUR_MIGRATED.load(R), crate::fs::DEV_LOCK_YIELDS.load(R),
                    crate::ext2::FS_DEV_RELEASED.load(R));
            }
            {   // A non-zero here is a defect rather than statistics: the ring is held for nanoseconds,
                // a barrier no longer than the barrier itself, a lost command is a lost I/O.
                let (rt, rb, ft, fb, lost, fl) = (crate::fs::RING_TIMEOUT.load(R), crate::fs::RING_BYPASS.load(R),
                    crate::fs::FLUSH_TIMEOUT.load(R), crate::fs::FLUSH_BYPASS.load(R),
                    crate::fs::IO_LOST.load(R), crate::fs::FLUSH_LOST.load(R));
                let wf = crate::fs::WRITE_FAILED.load(R);
                if rt | rb | ft | fb | lost | fl | wf != 0 {
                    crate::println!("stat: A DISK DEFECT: the ring's lock longer than a second {} (bypasses {}), a barrier longer than 30 s {} (bypasses {}), \
                        commands lost {}, of them barriers {}, refusals of a write/barrier {} (the FS is read-only)",
                        rt, rb, ft, fb, lost, fl, wf);
                }
            }
            crate::println!("stat: the disk: reads {} ({} ms, {} us each), writes {} ({} ms, {} us), barriers {} ({} ms, {} us, the worst {} us)",
                rn, ms(crate::fs::DEV_RD_TSC.load(R)), us(crate::fs::DEV_RD_TSC.load(R)) / rn.max(1),
                wn, ms(crate::fs::DEV_WR_TSC.load(R)), us(crate::fs::DEV_WR_TSC.load(R)) / wn.max(1),
                fln, ms(crate::fs::DEV_FL_TSC.load(R)), us(crate::fs::DEV_FL_TSC.load(R)) / fln.max(1),
                us(crate::fs::DEV_FL_MAX.load(R)));
            crate::println!("stat: the disk: commands submitted {}, the greatest queue depth {}, the queue was full {} times",
                crate::fs::IO_SUBMITS.load(R), crate::fs::IO_DEPTH_MAX.load(R), crate::fs::IO_QFULL.load(R));
            {   // A DURABLE WRITE INSTEAD OF A SECOND BARRIER (the journal's commit block).
                // The number must coincide with the number of transactions: if it is zero
                // while there are transactions, it means the device has no FUA (virtio-blk) and
                // there are still two barriers per transaction.
                let fua = crate::fs::DEV_FUA_N.load(R);
                let nc = crate::fs::DEV_FL_NOCACHE.load(R);
                if fua != 0 { crate::println!("stat: the disk: durable FUA writes {} (instead of the transaction's second barrier; {} us each, the worst {} \
                    us)", fua,
                    us(crate::fs::DEV_FUA_TSC.load(R)) / fua, us(crate::fs::DEV_FUA_MAX.load(R))); }
                let plain = crate::fs::DEV_DUR_PLAIN_N.load(R);
                if plain != 0 { crate::println!("stat: the disk: durable writes by an ORDINARY write {} (VWC=0 — FUA is superfluous)", plain); }
                // The barriers that were not needed: the device declared that it has
                // no volatile write cache (VWC=0), and a Flush in that
                // case is a command that is guaranteed to carry nothing.
                if nc != 0 { crate::println!("stat: the disk: barriers were not needed {} (the device has no volatile write cache)", nc); }
            }
            crate::println!("stat: the disk: requests for a barrier merged with a foreign one {} (out of {} — {}%), we waited for a foreign barrier {} times",
                crate::fs::DEV_FL_MERGED.load(R), crate::fs::DEV_FL_MERGED.load(R) + fln,
                crate::fs::DEV_FL_MERGED.load(R) * 100 / (crate::fs::DEV_FL_MERGED.load(R) + fln).max(1),
                crate::fs::FLUSH_WAITS.load(R));
        }
    }
    {   let (wr, runs, flush) = crate::ext2::wr_stats();
        let (rd, _) = crate::ext2::stats();
        if wr + rd != 0 {
            // We count the device's barriers SEPARATELY from the blocks: a write's price is in
            // blocks, a barrier's is in the disk's round-trip latency, and one cannot be seen through
            // the other by the number of blocks.
            let (ra_n, ra_extra) = crate::ext2::ra_stats();
            crate::println!("stat: the FS: {} blocks read, {} written ({} ranges), device barriers {}, readaheads {} (+{} blocks); reads from a hole (as \
                zeroes) {}, shortened by the end of the file {}, past the end {}",
                rd, wr, runs, flush, ra_n, ra_extra,
                crate::ext2::RD_HOLE.load(core::sync::atomic::Ordering::Relaxed), crate::ext2::RD_SHORT.load(core::sync::atomic::Ordering::Relaxed),
                    crate::ext2::RD_PAST_EOF.load(core::sync::atomic::Ordering::Relaxed));
        }
    }
    {   let (takes, cont, wait_us, held_us, to, alien, yields) = crate::ext2::lock_stats();
        if takes != 0 {
            // "Foreign entries" and "moves of the owner" must be ZEROES, and
            // they mean one and the same event from different sides: an ext2 critical
            // section WAS PREEMPTED and the owner was carried to another core. While the
            // recursion was recognised by the core number, the first one was an entry into ext2
            // WITHOUT the lock, and the second one an eternal wait for oneself (see `fs_lock`).
            crate::println!("stat: the FS lock: takes {}, with a wait {} ({}%), we waited {} ms, we held {} ms; longer than a second {}, foreign entries {}, \
                moves of the owner {}, gave up the core {}",
                takes, cont, cont * 100 / takes, wait_us / 1000, held_us / 1000, to, alien,
                crate::ext2::FS_RECUR_MIGRATED.load(core::sync::atomic::Ordering::Relaxed), yields);
            // The flush of dirty blocks WITHOUT the FS lock (§8.6-8.7): portions are the departures to
            // the device with the lock released, the balance is how many times a write
            // flushed by itself (more than a quarter of the cache is dirty), we waited for the device is
            // how many times the device was busy with a foreign core at the moment of the flush.
            let (chunks, bal, devwait) = crate::ext2::wb_stats();
            crate::println!("stat: the flush without the lock: portions {}, the balance {}, we waited for the device {}, handoffs {}", chunks, bal, devwait,
                crate::fs::dev_handoffs());
            // §8.14 THE EVICTION OF A DIRTY SLOT. A miss whose candidate is dirty
            // costs a synchronous write under the FS lock — that is the half-millisecond
            // scale on which `insert` loses (class B of scoreboard 35).
            // Here one can see both how many of them there are, and what they cost, and whether they glue
            // into batches (`talaria.evictbatch=`): "blocks per request" = 1 means
            // that every eviction is a separate round trip to the disk.
            {
                use core::sync::atomic::Ordering::Relaxed;
                let (blk, runs, tsc, mx) = crate::ext2::evict_stats();
                let meta = crate::ext2::EVICT_META.load(Relaxed);
                if runs > 0 || meta > 0 {
                    let hz = crate::tsc::calibrate_cached().max(1);
                    let us = |c: u64| c.saturating_mul(1_000_000) / hz;
                    crate::println!("stat: the eviction of slots: blocks {} over {} requests ({} blocks per request), {} us in total ({} us per request, the \
                        worst {}); commits for the metadata {}",
                        blk, runs, if runs > 0 { blk / runs } else { 0 }, us(tsc),
                        if runs > 0 { us(tsc) / runs } else { 0 }, us(mx), meta);
                }
            }
            let (tx, ck, b) = crate::ext2::barrier_stats();
            crate::println!("stat: the journal: transactions {}, checkpoints {} (copies from the journal {}); barriers: the data {}, the transaction {}, the \
                checkpoint {}, the journal's superblock {}, the recover flag {}, others {}; journal bypasses {}, we waited for a foreign transaction longer \
                than the diagnosis threshold {}",
                tx, ck, crate::ext2::ckpt_old(), b[0], b[1], b[2], b[3], b[4], b[5], crate::ext2::JRN_BYPASS.load(core::sync::atomic::Ordering::Relaxed),
                    crate::ext2::JRN_LONGWAIT.load(core::sync::atomic::Ordering::Relaxed));
            // STEP 2 OF SESSION 39: THE INSTRUMENT BEFORE THE KNOB. Before introducing a pre-emptive
            // flush BY THE AGE of the dirty data (like `dirty_expire_centisecs`), one must
            // know two numbers: how many data blocks there are per fsync call
            // (and how many the file has in total) and of what AGE the dirty data are by the moment of
            // the flush. An age in single milliseconds means that Linux's background
            // writeback would not have carried this block away either — and the explanation
            // "with Linux the dirty data left in advance" is wrong together with the knob.
            {
                let (fc, fd, ff) = crate::ext2::fsd_stats();
                if fc > 0 {
                    crate::println!("stat: the data per fsync(fd) call: calls {}, dirty blocks {} per call, the file has {} blocks (they arrived clean {} %)",
                        fc, fd / fc, ff / fc, if ff > 0 { 100 - (fd * 100 / ff).min(100) } else { 0 });
                }
                // STEP 1 OF SESSION 40: THE AGE BY POPULATION. The "2.7-3.2 s" of session
                // 39 are averaged over everyone who carries dirty data away; the knob "the flush by
                // age" exists exactly for the `fsync(fd)` population — if
                // the seconds belong to the `write` threshold or to eviction, there is nothing for `fsync`
                // to pre-empt. The final line (the sum) is kept so that
                // session 40's run compares with session 39's run directly.
                for i in 0..crate::ext2::WBC_N {
                    let (an, ab, asum, amax) = crate::ext2::wbage_stats_of(i);
                    if ab == 0 { continue; }
                    crate::println!("stat: the age of the dirty data at the flush, the population {}: portions {}, blocks {}, the average {} us, the worst {} \
                        us",
                        crate::ext2::WBC_NAMES[i], an, ab, asum / ab, amax);
                }
                // STEP 1 OF SESSION 41: the smearing must be judged by the WORST flush rather
                // than by the average one: the price is paid by the thread that got under the kick.
                let (af, ablk, amx, ask) = crate::ext2::age_stats();
                if af > 0 || ask > 0 {
                    crate::println!("stat: the pre-emptive flush by age: {} times, {} blocks ({} per time, THE WORST {}), kicks without a pass over the cache \
                        {}",
                        af, ablk, ablk / af.max(1), amx, ask);
                }
                let (an, ab, asum, amax) = crate::ext2::wbage_stats();
                if ab > 0 {
                    crate::println!("stat: the age of the dirty data at the flush, ALL populations: portions {}, blocks {}, the average {} us, the worst {} us",
                        an, ab, asum / ab, amax);
                }
            }
            // A FOREIGN TRANSACTION — WITH THE NAME OF THE HOLDER'S PHASE (session 39). The phase
            // "a foreign transaction" cost 238 us per call with three events over
            // two minutes, that is, one stop is hundreds of milliseconds, and that is
            // the mechanics of p99.9. "The journal was busy" does not become a change; "94 % of
            // the wait fell on the commit" does.
            {
                let hz = crate::tsc::calibrate_cached().max(1);
                let us = |c: u64| c.saturating_mul(1_000_000) / hz;
                let mut any = false;
                for i in 0..crate::ext2::JSTAGE_NAMES.len() {
                    let (n, t, mx) = crate::ext2::jwait_stage(i);
                    if n == 0 { continue; }
                    if !any { crate::print!("stat: we waited for a foreign transaction at its phase:"); any = true; }
                    crate::print!(" {} — {} episodes, {} us (the worst {});", crate::ext2::JSTAGE_NAMES[i], n, us(t), us(mx));
                }
                if any { crate::println!(""); }
            }
            // The breakdown of fsync by phases: whose price is in the tail — the data, the superblock,
            // the descriptor, the commit or the checkpoint (see `ext2::FSYNC_PH`).
            {
                use core::sync::atomic::Ordering::Relaxed;
                let hz = crate::tsc::calibrate_cached().max(1);
                let us = |c: u64| c.saturating_mul(1_000_000) / hz;
                let mut any = false;
                for (i, ph) in crate::ext2::FSYNC_PH.iter().enumerate() {
                    let n = ph.n.load(Relaxed);
                    if n == 0 { continue; }
                    if !any { crate::print!("stat: fsync by phases:"); any = true; }
                    crate::print!(" {} {} ({} us each, the worst {});", crate::ext2::FSYNC_PH_NAMES[i], n, us(ph.tsc.load(Relaxed)) / n,
                        us(ph.max.load(Relaxed)));
                }
                if any { crate::println!(""); }
                // WHO IS THE CRITICAL PATH IN THE JOURNAL'S BATCH — THE DATA OR THE COPIES.
                // The price of the change "a commit without a second round" depends on that:
                // an async commit takes the copies off the chain, but NOT the data (ordered).
                // The gain is min(commit, the copies - the data), and if the data
                // are acknowledged last, the change does not exist.
                let aw = crate::fs::ABSW_N.load(Relaxed);
                if aw > 0 {
                    let (dt, ot) = (crate::fs::ABSW_DATA_TSC.load(Relaxed), crate::fs::ABSW_OWN_TSC.load(Relaxed));
                    crate::println!("stat: the journal's batch waited for two {} times: the deferred DATA {} us (the worst {}), its own COPIES {} us (the \
                        worst {}) — the critical path is {}",
                        aw, us(dt) / aw, us(crate::fs::ABSW_DATA_MAX.load(Relaxed)),
                        us(ot) / aw, us(crate::fs::ABSW_OWN_MAX.load(Relaxed)),
                        if dt > ot { "THE DATA (an async commit will not help)" } else { "the copies (an async commit will take them off the chain)" });
                }
                // §8.13: how many portions of data left without a wait and who waited for them.
                let dn = crate::fs::DEFER_N.load(Relaxed);
                if dn > 0 {
                    crate::println!("stat: fsync: deferred portions of data {} (ext2 {}), waited for by the next write {}, by an explicit wait {}",
                        dn, crate::ext2::WB_DEFERRED.load(Relaxed), crate::fs::DEFER_ABSORBED.load(Relaxed), crate::fs::DEFER_WAITED.load(Relaxed));
                }
                let fw = crate::ext2::FALLOC_WIN.load(Relaxed);
                if fw > 0 {
                    crate::println!("stat: a long write: preemption windows {}, of them the core was given to a ready thread {}", fw,
                        crate::ext2::FALLOC_YIELD.load(Relaxed));
                }
                // The interrupt window while waiting for the device (`talaria.iowin=`): how many
                // times a tick managed to arrive instead of a spin with IF=0 and how many times the window was
                // not needed (the interrupts are already open).
                let iw = crate::fs::IO_WIN_N.load(Relaxed);
                if iw > 0 || crate::fs::IO_WIN_SPIN.load(Relaxed) > 0 {
                    crate::println!("stat: waiting for the device: interrupt windows {} (mode {}), steps without a window {}",
                        iw, crate::fs::IO_WIN_MODE.load(Relaxed), crate::fs::IO_WIN_SPIN.load(Relaxed));
                }
            }
        }
    }
    crate::println!("stat: syscalls={} page_faults={} (cow={}) frame_allocs={} frames_given={}",
        syscalls(), page_faults(), cow_faults(), frame_allocs(), frames_given());
    top_syscalls();
    top_tasks();
    top_code();
    {   // with what the waits on a futex end
        use core::sync::atomic::Ordering::Relaxed;
        let c = crate::sync::W_CALLS.load(Relaxed);
        if c != 0 {
            crate::println!("stat: futex waits {}: straight past {}, woken {}, the word itself {}, a timeout {}, a signal {}",
                c, crate::sync::W_EAGAIN.load(Relaxed), crate::sync::W_WOKEN.load(Relaxed),
                crate::sync::W_PRED.load(Relaxed), crate::sync::W_TIMEOUT.load(Relaxed),
                crate::sync::W_SIG.load(Relaxed));
            crate::println!("stat:   exits {}, of them having slept {}, by \"the task is dead\" {}; an exact deadline: under a tick {}, an LVT loan {}",
                crate::sync::W_EXIT.load(Relaxed), crate::sync::W_SLEPT.load(Relaxed), crate::sync::W_DEAD.load(Relaxed),
                crate::sync::W_DL_TICK.load(Relaxed), crate::sync::W_DL_BORROW.load(Relaxed));
            crate::println!("stat:   the delivery of wakeups: by a write {}, its own core {}, an IPI reschedule (a sleeping one {}, the threshold {}, a long \
                tick {}), a kick {}, the BSP {}, by a call {}",
                crate::sync::WK_STORE.load(Relaxed), crate::sync::WK_SAME.load(Relaxed),
                crate::sync::WK_RESCHED_IDLE.load(Relaxed), crate::sync::WK_RESCHED_BUSY.load(Relaxed),
                crate::sync::WK_RESCHED_LONG.load(Relaxed), crate::sync::WK_KICK.load(Relaxed), crate::sync::WK_BSP.load(Relaxed),
                    crate::sync::WK_BELL.load(Relaxed));
            let young = crate::sync::WK_RESCHED_YOUNG.load(Relaxed);
            if young != 0 {
                crate::println!("stat:   deferred preemptions: wakeups against a young occupant {}, the scheduler stayed for the threshold {}, ticks pulled {}",
                    young, crate::sched::DEFER_STAY.load(Relaxed), crate::smp::TICK_PULLS.load(Relaxed));
            }
            let dl = crate::sync::DL_PULLS.load(Relaxed);
            if dl != 0 {
                crate::println!("stat:   ticks pulled to a sleeper's deadline {} (a timed sleep behind a running neighbour ends at its term, not at the core's next tick)", dl);
            }
        }
        {   // the holders of a core a wakeup only kicked (the top four)
            let mut top = [(0u64, 0usize); 4];
            for t in 0..crate::sched::NTASKS {
                let k = crate::sync::KICK_ON[t].load(Relaxed);
                if k == 0 { continue; }
                if let Some(i) = (0..4).find(|&i| k > top[i].0) { top.copy_within(i..3, i + 1); top[i] = (k, t); }
            }
            crate::println!("stat:   kicks: the woken task's own core {}, a core running another task {} (of them asleep {})",
                crate::sync::KICK_SELF.load(Relaxed), top.iter().map(|x| x.0).sum::<u64>(), crate::sync::KICK_ON_SLEEPING.load(Relaxed));
            if top[0].0 > 0 {
                crate::print!("stat:   a kick found the core running another task:");
                for &(k, t) in top.iter().filter(|x| x.0 > 0) {
                    let c = crate::sched::comm(t);
                    let n = c.iter().position(|&b| b == 0).unwrap_or(16);
                    crate::print!(" t{} {} {} (ran {} us on average)", t, core::str::from_utf8(&c[..n]).unwrap_or("?"), k,
                        crate::sync::KICK_ON_RAN[t].load(Relaxed) / k / 1000);
                }
                crate::println!("");
            }
        }
        let names = ["WAIT", "WAKE", "FD", "REQUEUE", "CMP_REQUEUE", "WAKE_OP", "LOCK_PI",
                     "UNLOCK_PI", "TRYLOCK_PI", "WAIT_BITSET", "WAKE_BITSET", "?11", "?12", "?13", "?14", "?15"];
        let mut any = false;
        for o in 0..16usize {
            let v = crate::syscall::FUTEX_OPS[o].load(Relaxed);
            if v == 0 { continue; }
            if !any { crate::print!("stat: futex by operations:"); any = true; }
            crate::print!(" {}={}", names[o], v);
        }
        if any { crate::println!(""); }
    }
    // THE SHM TABLES: how many times a creator had to WAIT for them. Without this number
    // the gate `shmrace` cannot tell "the lock works" from "the threads never met" — a
    // race that did not happen proves nothing about the lock.
    {
        let w = crate::syscall::SHM_WAITS.load(core::sync::atomic::Ordering::Relaxed);
        if w > 0 { crate::println!("stat: shm: creators waited for the tables {} times (the lock of shm_open/shmget)", w); }
    }
    // The observability gate (`talaria.test=obslive`): the numbers are checked exactly
    // here — at the only moment when the workload has already finished, while the counters
    // have not been reset yet.
    if crate::selftest::obs_live_armed() { crate::selftest::obs_live_report(); }
    if crate::selftest::rqwait_armed() { crate::selftest::rqwait_report(); }
}

/// The top ten pages of CODE on which the tick caught the workload (`talaria.prof=1`).
/// The symbols are resolved on the host: `addr2line -e <binary> <address>`.
pub fn top_code() {
    let n = crate::sched::prof_len();
    let mut total = 0u64;
    for i in 0..n { total += crate::sched::prof_slot(i).1; }
    if total == 0 { total = 1; }
    {   // the kernel goes as a separate list: the address is resolved by the kernel's own symbols
        let mut kt = 0u64;
        for i in 0..n { kt += crate::sched::profk_slot(i).1; }
        if kt != 0 {
            crate::println!("stat: the profile by the KERNEL's code ({} samples):", kt);
            let mut shown = [false; 64];
            for _ in 0..8 {
                let mut best = usize::MAX; let mut bv = 0u64;
                for i in 0..n {
                    let (_, h) = crate::sched::profk_slot(i);
                    if h > bv && !shown[i] { bv = h; best = i; }
                }
                if best == usize::MAX || bv == 0 { break; }
                shown[best] = true;
                let (a, h) = crate::sched::profk_slot(best);
                crate::println!("stat:   {:#x}: {} ({}%)", a, h, h * 100 / kt);
            }
        }
    }
    crate::println!("stat: the profile by code ({} samples, pages of user code):", total);
    let mut shown = [false; 64];
    for _ in 0..10 {
        let mut best = usize::MAX; let mut bv = 0u64;
        for i in 0..n {
            let (_, h) = crate::sched::prof_slot(i);
            if h > bv && !shown[i] { bv = h; best = i; }
        }
        if best == usize::MAX || bv == 0 { break; }
        shown[best] = true;
        let (page, hits) = crate::sched::prof_slot(best);
        crate::println!("stat:   {:#x}: {} ({}%)", page, hits, hits * 100 / total);
    }
    {   // the kernel's masked time booked to ring 3 (sched::PROF_SYSTAIL)
        let mut all = 0u64;
        for nr in 0..512 { all += crate::sched::prof_systail(nr); }
        if all != 0 {
            let mut top = [(0usize, 0u64); 4];
            for k in 0..4 {
                for nr in 0..512 {
                    let v = crate::sched::prof_systail(nr);
                    if v > top[k].1 && !top[..k].iter().any(|e| e.0 == nr) { top[k] = (nr, v); }
                }
            }
            crate::println!("stat: of them on a syscall's return address {} ({}%), within 5 us of its exit {} — the kernel's masked time; \
                by syscall: #{} {} #{} {} #{} {} #{} {}", all, all * 100 / total, crate::sched::prof_systail_near(),
                top[0].0, top[0].1, top[1].0, top[1].1, top[2].0, top[2].1, top[3].0, top[3].1);
        }
    }
}

/// The top five tasks by the number of ticks on the processor (who ate the core).
pub fn top_tasks() {
    use core::sync::atomic::Ordering::Relaxed;
    let n = crate::sched::task_count().min(crate::sched::max_tasks());
    let mut total = 0u64;
    for t in 0..n { total += crate::sched::TICKS_ON[t].load(Relaxed); }
    if total == 0 { return; }
    {   // PER CORE first of all. "A task ate a quarter of the ticks" and "only
        // two cores out of four worked" look identical from the point of view of the task
        // list, while they are cured by the opposite. A tick on an idle core is
        // counted separately: a core without work and a core busy with waiting are also
        // different illnesses.
        let mut any = false;
        for c in 0..crate::percpu::MAXCPU {
            let (busy, idle) = crate::sched::cpu_ticks(c);
            if busy + idle == 0 { continue; }
            if !any { crate::println!("stat: ticks per core (busy/idle):"); any = true; }
            crate::println!("stat:   core {}: {} / {} (the role {}, the timer {})", c, busy, idle,
                match crate::affinity::role(c) { 0 => "off", 1 => "common", _ => "dedicated" },
                if crate::sched::cpu_timer_armed(c) { "armed" } else { "STOPPED" });
        }
        // THE TICK'S GAP (session 32): the worst distance between two ticks of a busy
        // core and who sat in it — see `sched::tick_gap_note`. The worst one is SINCE
        // THE PREVIOUS stat BLOCK: otherwise the start of mongod (fdatasync 235 ms under IF=0 on
        // the second second) covered up everything that happened under load.
        for c in 0..crate::percpu::MAXCPU {
            let (ns, end_ms, t, rip, sys, a0, sin, sout) = crate::sched::tick_gap(c);
            if ns == 0 { continue; }
            crate::sched::tick_gap_reset(c);
            let n = &crate::sched::GAP_N[c];
            crate::println!("stat:   the tick's gap on core {} since the previous block: the worst {} us (it ended at {} ms, on the core t{} \"{}\", the \
                tick's rip {:#x} ring{}); gaps in total >=1 ms {}, >=4 ms {}, >=16 ms {}",
                c, ns / 1000, end_ms, t, crate::obs::comm(t as usize).as_str(),
                rip & 0x7FFF_FFFF_FFFF_FFFF, if rip >> 63 != 0 { 3 } else { 0 },
                n[0].load(Relaxed), n[1].load(Relaxed), n[2].load(Relaxed));
            let ago = |x: u64| -> i64 { if x == u64::MAX { -1 } else { (x / 1000) as i64 } };
            crate::println!("stat:     the task's last syscall: #{} from rip {:#x} arg0={:#x}; it entered {} us before the end of the gap, it left {} us \
                before the end (-1 — there is no stamp){}",
                sys >> 48, sys & 0xFFFF_FFFF_FFFF, a0, ago(sin), ago(sout),
                if sin != u64::MAX && sin >= ns { " — THE SYSCALL COVERED THE GAP ENTIRELY (the kernel under IF=0)" } else { "" });
            // WHOSE GAP IT IS. "The syscall covered the gap entirely" is written even when
            // the vCPU in the middle of that syscall simply was not on the processor:
            // the syscall was going on all right. Only steal tells them apart (see `tick_gap_host`).
            if crate::smp::steal_on() {
                let (hns, hn, hsum) = crate::sched::tick_gap_host(c);
                crate::println!("stat:     of it the host took {} us ({}% of the gap); gaps >=1 ms where it took more than half: {}, taken in the gaps in \
                    total {} ms",
                    hns / 1000, hns.saturating_mul(100) / ns.max(1), hn, hsum / 1_000_000);
            }
        }
    }
    {   // THE TICK'S PRICE. The slice sets the frequency with which it is paid: at 0.5 ms
        // a tick of 30 us eats 6 % of the processor, and that is indistinguishable from "slow
        // hardware" until the price is measured.
        let n = crate::sched::TICK_N.load(Relaxed);
        if n != 0 {
            let hz = crate::tsc::calibrate_cached().max(1);
            let ns = |cy: u64| cy * 1000 / (hz / 1_000_000).max(1);
            crate::println!("stat: the tick's price: {} ns on average, at most {} ns ({} ticks)",
                ns(crate::sched::TICK_CY.load(Relaxed)) / n.max(2).saturating_sub(1),
                ns(crate::sched::TICK_MAX.load(Relaxed)), n);
            let pn = crate::console::POLL_N.load(Relaxed);
            if pn != 0 {
                crate::println!("stat: the console poll (COM1 from the tick): {} times, {} ns on average, at most {} ns",
                    pn, ns(crate::console::POLL_CY.load(Relaxed)) / pn, ns(crate::console::POLL_MAX.load(Relaxed)));
            }
            // THE INTERRUPT PATHS. On a stand without APICv an access to the LAPIC window costs
            // about ten microseconds of emulation, so what matters is not "how many
            // interrupts" but HOW MANY OF THEM went past the LAPIC.
            // THE TICK'S PERIOD IS NOT ONE PER MACHINE. On a core where someone waits for the network it is
            // shorter (see smp::want_period_ns): otherwise nobody would see a frame until
            // the end of the quantum of the computing neighbour. The number of reconfigurations shows
            // whether the sign chatters — each one costs a write into the LAPIC.
            // THE BREAKDOWN OF THE TICK'S PRICE BY PHASES (talaria.tickprof=1). The total number says
            // only "it is expensive"; the phases are cured differently.
            if crate::sched::TP_ON.load(Relaxed) {
                for ph in 0..crate::sched::TP_N {
                    let c = crate::sched::TP_CNT[ph].load(Relaxed);
                    if c == 0 { continue; }
                    crate::println!("stat:   the tick's phase \"{}\": {} ns x {}",
                        crate::sched::TP_NAMES[ph], ns(crate::sched::TP_SUM[ph].load(Relaxed)) / c, c);
                }
            }
            let rt = crate::smp::RETUNES.load(Relaxed);
            if rt != 0 {
                crate::println!("stat: the tick's period was reconfigured {} times (a short slice on a core with a network waiter: {} us)",
                    rt, crate::smp::NETSLICE_NS.load(Relaxed) / 1000);
            }
            crate::println!("stat: interrupts: LAPIC {}, EOI past PV {}, IPIs by a hypercall {} / through the ICR {}",
                if crate::smp::x2apic() { "x2APIC (MSR)" } else { "xAPIC (MMIO)" },
                crate::smp::PV_EOI_MISS.load(Relaxed),
                crate::smp::IPI_PV.load(Relaxed), crate::smp::IPI_ICR.load(Relaxed));
        }
    }
    // THE BALANCER: "enabled" and "works" are different things. The passes are counted
    // BY THE BSP's TICKS, and the tick's period is not a constant since this session: the line answers
    // the question "did it reach the skew at all" rather than "is it allowed to".
    crate::println!("stat: work stealing backed off (it would not have improved the balance) {} times", crate::affinity::STEAL_BUSY.load(Relaxed));
    for k in 0..crate::affinity::STEAL_N {
        let r = &crate::affinity::STEAL_REC[k];
        if r[0].load(Relaxed) == u64::MAX { break; }
        crate::println!("stat:   the steal {}: core {} <- t{} from core {} (its own task {}, the victim left {} us ago, the moment {} us)",
            k, r[0].load(Relaxed), r[1].load(Relaxed), r[2].load(Relaxed),
            r[3].load(Relaxed), r[4].load(Relaxed), r[5].load(Relaxed));
        crate::println!("stat:            tasks on the thief {}, on the donor {}", r[6].load(Relaxed), r[7].load(Relaxed));
    }
    #[cfg(target_arch = "aarch64")]
    {
        use crate::arch::mmu as m;
        crate::println!("stat: asid: {} (spaces given one {}, found the pool empty {}; root switches outside the scheduler's exit: kept the TLB {}, invalidated {})",
            if m::asid_on() { "on" } else { "OFF (talaria.asid=0)" }, m::ASID_GIVEN.load(Relaxed), m::ASID_EMPTY.load(Relaxed),
            m::SWITCH_KEPT.load(Relaxed), m::SWITCH_FLUSHED.load(Relaxed));
    }
    crate::println!("stat: balancing: passes {}, a skew found {}, rearranged {} (migrations in total {})",
        crate::affinity::BAL_PASS.load(Relaxed), crate::affinity::BAL_SEEN.load(Relaxed),
        crate::affinity::BALANCED.load(Relaxed), crate::affinity::MIGRATIONS.load(Relaxed));
    crate::println!("stat: balancing: a move backed off before the scheduler {} times",
        crate::sched::MIGRATE_LOST.load(Relaxed));
    crate::println!("stat: balancing: the greatest skew {} threads (the threshold {})",
        crate::affinity::BAL_MAXW.load(Relaxed) * 100 / crate::affinity::W_FULL as u64,
        crate::affinity::imbalance_threshold() * 100 / crate::affinity::W_FULL);
    crate::println!("stat: ticks on the processor in total {} — who ate the core:", total);
    for _ in 0..5 {
        let mut best = usize::MAX; let mut bv = 0u64;
        for t in 0..n {
            let v = crate::sched::TICKS_ON[t].load(Relaxed);
            if v > bv && !unsafe { core::ptr::read_volatile(core::ptr::addr_of!(SEEN[t])) } { bv = v; best = t; }
        }
        if best == usize::MAX { break; }
        unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(SEEN[best]), true); }
        // WITH THE CORE NUMBER: "two computing threads ate a quarter of the ticks each" and "two
        // computing threads sat down on ONE core" are different things, while by the ticks alone they
        // are indistinguishable. That is exactly what the bimodality of the speedup ends with.
        crate::println!("stat:   t{}: {} ticks ({}%), core {}", best, bv, bv * 100 / total, crate::sched::cpu_of(best));
    }
}
static mut SEEN: [bool; 192] = [false; 192];

/// NS PER CALL FROM CYCLES — IN u128, AND THAT IS NOT PEDANTRY (session 42).
///
/// It was `cycles * 1_000_000_000 / hz / c`, and the multiplier overflows u64 already at
/// 188 seconds of accumulated time: 5.45e11 cycles * 1e9 = 5.45e20 against a
/// ceiling of 1.8e19. In the snapshot of session 41's stand there stood "#202: 448 times, 188137 ms,
/// 8190622 ns", where 448 * 8.19 ms = 3669 ms rather than 188137 — that is, the instrument
/// printed a number contradicting its own neighbouring column. That is caught only by
/// adding two columns together, which nobody does; hence the arithmetic is moved out
/// into a function, and the function under the gate `sysns`.
pub fn ns_per_call(ticks: u64, hz: u64, c: u64) -> u64 {
    let (hz, c) = (hz.max(1) as u128, c.max(1) as u128);
    ((ticks as u128) * 1_000_000_000 / hz / c) as u64
}
/// The numbers of the syscalls to print INDEPENDENTLY of the top ten
/// (`talaria.sysrow=1,18,74`). A zero at the end means "nothing further".
pub static SYSROW: [core::sync::atomic::AtomicU64; 8] =
    [const { core::sync::atomic::AtomicU64::new(u64::MAX) }; 8];
/// Parse `talaria.sysrow=` (a comma-separated list of numbers). It returns
/// how many numbers were accepted: a knob that STAYS SILENT on a typo is more expensive
/// than a missing one (the rule of session 25).
pub fn sysrow_set(list: &str) -> usize {
    use core::sync::atomic::Ordering::SeqCst;
    let mut k = 0usize;
    for part in list.split(',') {
        if k >= SYSROW.len() { break; }
        match part.trim().parse::<u64>() {
            Ok(n) if n < 512 => { SYSROW[k].store(n, SeqCst); k += 1; }
            _ => {}
        }
    }
    for j in k..SYSROW.len() { SYSROW[j].store(u64::MAX, SeqCst); }
    k
}
/// The top ten syscalls by TOTAL time in the kernel (and how many times they were called).
/// It is printed after the workload: "fifty thousand syscalls" is not an answer, the answer is which ones.
///
/// PLUS A LINE ON REQUEST (`talaria.sysrow=`). The top ten is selected by FULL
/// time, and a cheap syscall never gets into it: under mongodb the first
/// ten are futex, nanosleep, recvmsg, epoll, sched_yield, fdatasync..., while
/// `write`/`pwrite64` are not visible at all. Meanwhile the pre-emptive flush by age
/// (`talaria.dirtyage=`) is paid for INSIDE `write` — and its price is visible only
/// there. Without this line the knob's price could be judged only by a nine-hour
/// scoreboard (that is how it stood in session 42's plan).
pub fn top_syscalls() {
    use core::sync::atomic::Ordering;
    let hz = crate::tsc::hz().max(1);
    let mut idx = [0usize; 10];
    let mut best = [0u64; 10];
    for n in 0..512usize {
        let t = crate::syscall::SYS_TICKS[n].load(Ordering::Relaxed);
        if t == 0 { continue; }
        let mut k = 10;
        while k > 0 && t > best[k - 1] { k -= 1; }
        if k < 10 {
            let mut j = 9;
            while j > k { best[j] = best[j - 1]; idx[j] = idx[j - 1]; j -= 1; }
            best[k] = t; idx[k] = n;
        }
    }
    if best[0] == 0 { return; }
    crate::println!("stat: syscalls by the time in the kernel (the number: calls, total ms, ns per call):");
    for k in 0..10 {
        if best[k] == 0 { break; }
        let n = idx[k];
        let c = crate::syscall::SYS_COUNT[n].load(Ordering::Relaxed).max(1);
        crate::println!("stat:   #{}: {} times, {} ms, {} ns",
            n, c, best[k] / (hz / 1000).max(1), ns_per_call(best[k], hz, c));
    }
    for k in 0..SYSROW.len() {
        let n = SYSROW[k].load(Ordering::Relaxed);
        if n == u64::MAX { break; }
        let n = n as usize;
        let t = crate::syscall::SYS_TICKS[n].load(Ordering::Relaxed);
        let c = crate::syscall::SYS_COUNT[n].load(Ordering::Relaxed);
        crate::println!("stat:   on request #{}: {} times, {} ms, {} ns",
            n, c, t / (hz / 1000).max(1), ns_per_call(t, hz, c));
    }
}

/// Print the summary per task at the workload's exit (`suite::park`): it is placed by
/// the self-checks that need a metric AFTER the workload rather than by a timer.
pub static WAITERS_AT_EXIT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// The summary at the exit (see `WAITERS_AT_EXIT`): once.
pub fn waiters_at_exit() {
    if !WAITERS_AT_EXIT.swap(false, core::sync::atomic::Ordering::Relaxed) { return; }
    crate::println!("summary: the tasks by their wait for the processor (the slots' counters are alive after the exit):");
    print_waiters("summary:  ");
}
/// WHO waited: the eight tasks with the greatest wait for the processor — the name, the core,
/// whether it is pinned (a pinned one is taken neither by stealing nor by the balancer), how much it
/// ran and how much it gave the core up to the device. Without this the histogram says
/// "somebody waited a quantum", but not "who and why". The counters live in the slot until it is
/// reused, so the summary for the tasks THAT HAVE LEFT is printed too —
/// the self-checks call this after the workload in order to judge the metric not by a random
/// periodic `stat:`, but by the summary (the gate rqwait on one core).
pub fn print_waiters(prefix: &str) {
    let mut top: [(usize, u64); 8] = [(usize::MAX, 0); 8];
    for t in 0..crate::sched::NTASKS {
        let w = crate::sched::rq_ns(t);
        if w == 0 { continue; }
        if let Some(k) = top.iter().position(|&(_, v)| w > v) {
            top.copy_within(k..7, k + 1);
            top[k] = (t, w);
        }
    }
    for &(t, w) in top.iter() {
        if t == usize::MAX { break; }
        // "It ran" includes sleeping in mwait on the core too (the task stays current):
        // 28 s for mongod's TTLMonitor — is that thousands of long sleeps or millions
        // of insurance wakeups? Only the number of switches tells them apart.
        crate::println!("{} t{} \"{}\" core {} {} it waited {} ms, it ran {} ms over {} intervals, it gave up to the device {} ms",
            prefix, t, crate::obs::comm(t).as_str(), crate::sched::cpu_of(t),
            if crate::affinity::is_pinned(t) { "pinned" } else { "free" },
            w / 1_000_000, crate::sched::run_ns(t) / 1_000_000, crate::sched::switches(t), crate::sched::ioy_ns(t) / 1_000_000);
    }
}
