//! The self-tests of the scheduler, of SMP and of the synchronisation tier (PLAN-SYNC).
//!
//! The name of the mode is compared AS A WHOLE (a `match` on the value of `talaria.test=`),
//! so a name that is a prefix of a foreign name can no longer start a foreign mode
//! — see `crate::cmdline`.

use core::sync::atomic::Ordering;

/// Run the self-test `name`, if it is one of this set.
/// Returns `true` if the name was recognised (the body may well not have returned — many
/// modes end with `suite::park()`).
// Many gates do not return at all (`suite::park()` or the start of a workload),
// so in the sets where ALL the modes do that, the trailing `true` is unreachable.
#[allow(unused_variables, unreachable_code)]
pub fn run(name: &str, cmdline: &str) -> bool {
    match name {
        "schedapi" => {
            crate::smp::start_from_cmdline(cmdline); // talaria.smp=N works here too
            // The scheduler's contract: a yield really does give away the core, a membarrier
            // really does place a barrier, the binding to cores goes there and back.
            // All of that is easy to "return zero for" and not do — and such a refusal is invisible.
            if crate::fs::init() { unsafe { crate::user::run_process_disk(b"/schedapi", &[b"schedapi"], &[b"HOME=/"]); } }
            crate::suite::park()
        }
        "affinity" => {
            // The placement decisions on synthetic snapshots: eight cores, two NUMA
            // nodes, a skew, a convergence. There is no two-socket machine at home and there
            // never will be — yet the policy has to be checked, and here it is, all of it.
            println!("talaria: the self-test of the placement of threads over cores (pinning/spread/NUMA/balancing)");
            crate::affinity::selftest();
        }
        "shmwake" => {
            // PLAN-SYNC stage 5: an inter-VM wake-up through ivshmem WITHOUT a virtio-kick/vmexit —
            // the pong VM mwaits on a word in the host's shared memory, the ping VM wakes it by a write.
            let (b, s) = crate::pci::ivshmem();
            if b == 0 { println!("SHMWAKE: FAIL — ivshmem was not found (pcie=on + ivshmem-plain are required)"); crate::suite::park() }
            crate::smp::lapic_enable(); // mask the LVT/LINT: an sti in the mwait loop must not catch garbage vectors
            println!("talaria: SHMWAKE ivshmem {:#x} ({} KB), the mode is {}", b, s / 1024, crate::sync::mode_name(crate::sync::mode()));
            let w = crate::mem::devptr::<u32>(b); let ack = crate::mem::devptr::<u32>(b + 64);
            if crate::cmdline::has(cmdline, "talaria.shmrole", "pong") {
                unsafe {
                    core::ptr::write_volatile(ack, 0);
                    let mut last = core::ptr::read_volatile(w);
                    println!("SHMWAKE pong: sleeping on the word (mwait), echoing into ack");
                    loop {
                        while core::ptr::read_volatile(w) == last { crate::sync::kwait_once(w, last); }
                        last = core::ptr::read_volatile(w);
                        core::ptr::write_volatile(ack, last); // the answering write wakes the ping-mwait
                    }
                }
            } else {
                unsafe {
                    // let the pong VM boot and fall asleep
                    let t0 = crate::tsc::rdtsc(); let hz = crate::tsc::calibrate_cached();
                    while crate::tsc::rdtsc() - t0 < hz * 3 { core::hint::spin_loop(); }
                    let n = 1000usize;
                    let mut lat = [0u64; 1000];
                    for i in 0..n {
                        for _ in 0..50_000 { core::hint::spin_loop(); } // pong manages to fall asleep
                        let v = (i + 1) as u32;
                        let t = crate::tsc::rdtsc();
                        core::ptr::write_volatile(w, v); // a write into the shared memory = the wake-up of a foreign VM
                        loop { let cur = core::ptr::read_volatile(ack); if cur == v { break; } crate::sync::kwait_once(ack, cur); }
                        lat[i] = crate::tsc::rdtsc() - t;
                    }
                    lat.sort_unstable();
                    let ns = |c: u64| c.saturating_mul(1_000_000_000) / hz;
                    println!("SHMWAKE: the inter-VM RTT through ivshmem ({}): min={} ns p50={} ns p99={} ns (N={}) -> OK",
                        crate::sync::mode_name(crate::sync::mode()), ns(lat[0]), ns(lat[n / 2]), ns(lat[n * 99 / 100]), n);
                }
            }
            crate::suite::park()
        }
        "dynsync" => {
            // PLAN-SYNC stage 3: the DYNAMIC tsynctest on a patched ld-musl (picked up automatically:
            // PT_INTERP -> /lib/ld-musl with tier 1 in libc's futex paths).
            println!("talaria: DYNSYNC — the dynamic tsynctest through a patched ld-musl");
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(2, Ordering::SeqCst);
            if !crate::smp::start_cpu(1) { println!("DYNSYNC: FAIL — the AP did not come up (-smp 2 is required)"); }
            if crate::fs::init() { unsafe { crate::user::run_process_disk(b"/tsynctest.dyn", &[b"/tsynctest.dyn"], &[b"HOME=/"]); } }
            crate::suite::park()
        }
        "fiber" => {
            // PLAN-SYNC stage 4: fibers (a coroutine-level switch under the mutex/cv API) + swtchto
            static FB: &[u8] = user_elf!("tsync/fiberbench");
            println!("talaria: FIBER — green threads + the kernel's swtchto ({} bytes)", FB.len());
            unsafe { crate::user::run_threaded(FB); }
        }
        "tsync" => {
            // PLAN-SYNC stage 1: the correctness of libtsync (mutex/MCS/condvar/SPSC/timedwait)
            static TT: &[u8] = user_elf!("tsync/tsynctest");
            println!("talaria: TSYNC — the correctness of libtsync on 2 cores ({} bytes)", TT.len());
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(2, Ordering::SeqCst);
            if !crate::smp::start_cpu(1) { println!("TSYNC: FAIL — the AP did not come up (-smp 2 is required)"); }
            unsafe { crate::user::run_threaded(TT); }
        }
        "syncbench" => {
            // PLAN-SYNC: the targeted benchmarks — an SPSC cv-sleep vs a spinning ring, the wait/wake RTT
            static SB2: &[u8] = user_elf!("tsync/syncbench");
            println!("talaria: SYNCBENCH — SPSC cv vs spin, the wait/wake RTT ({} bytes)", SB2.len());
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(2, Ordering::SeqCst);
            if !crate::smp::start_cpu(1) { println!("SYNCBENCH: FAIL — the AP did not come up (-smp 2 is required)"); }
            unsafe { crate::user::run_threaded(SB2); }
        }
        "futexlat" => {
            // PLAN-SYNC stage 2: the latency of a futex wake-up (an unmodified musl):
            // a same-core directed switch and a cross-core mwait-wake by a write.
            static FL: &[u8] = user_elf!("futexlat");
            println!("talaria: FUTEXLAT — a ping-pong of futex/condvar, same-core and cross-core ({} bytes)", FL.len());
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(2, Ordering::SeqCst);
            if !crate::smp::start_cpu(1) { println!("FUTEXLAT: FAIL — the AP did not come up (-smp 2 is required)"); }
            unsafe { crate::user::run_threaded(FL); }
        }
        "smp4" => {
            // 4 cores: three threads migrate onto AP1..AP3, the parallel speedup is > 2.5x,
            // sched_getaffinity gives out the online mask (the actor benchmark: the distribution over the cores).
            static SM4: &[u8] = user_elf!("smp4");
            println!("talaria: SMP4 — the migration of threads onto AP1..AP3 + parallelism ({} bytes)", SM4.len());
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(2, Ordering::SeqCst);
            // A10: the LOGICAL cores 1..3 (the map "logical -> APIC id" is built from the
            // MADT in crate::smp::topology_init). The workload asks for the cores by the same numbers.
            for cpu in 1..4usize {
                if !crate::smp::start_cpu(cpu) { println!("SMP4: FAIL — AP{} did not come up (-smp 4 is required)", cpu); }
            }
            unsafe { crate::user::run_threaded(SM4) };
        }
        "schedspread" => {
            // V1.9: eight unpinned threads onto four cores. The spread lays them
            // out evenly, and the cores of the common pool PRE-EMPT — without that the second thread
            // of every core would never have reached the end (that is what the test catches).
            static SS: &[u8] = user_elf!("schedspread");
            println!("talaria: SCHEDSPREAD — the spread of unpinned threads over the pool's cores ({} bytes)", SS.len());
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(2, Ordering::SeqCst);
            for cpu in 1..4usize {
                if !crate::smp::start_cpu(cpu) { println!("SPREAD: FAIL — AP{} did not come up (-smp 4 is required)", cpu); }
            }
            unsafe { crate::user::run_threaded(SS) };
        }
        "schedbal" => {
            // Session 20: the balancer was ON and did not work — its clock was
            // the BSP's ticks, and the tick's period had stopped being a constant, and the demand (the running
            // average of readiness) was refreshed at the same rate at which it was read,
            // that is, it measured nothing. The gate creates a skew by the rule
            // `pin=*:1` (all the nameless threads onto one core) and demands that over
            // the time of a long computation they be RESETTLED. The placement at creation and the stealing of
            // work have nothing to do with it here: the first one has already run, the second one does not touch
            // such a skew because of the rule "improve the balance".
            static SB: &[u8] = user_elf!("schedbal");
            println!("talaria: SCHEDBAL — the resettlement of threads that have settled on one core ({} bytes)", SB.len());
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(2, Ordering::SeqCst);
            for cpu in 1..4usize {
                if !crate::smp::start_cpu(cpu) { println!("BAL: FAIL — AP{} did not come up (-smp 4 is required)", cpu); }
            }
            unsafe { crate::user::run_threaded(SB) };
        }
        "stealbal" => {
            // Session 25: the stealing SEPARATES two computing threads while a pool of SLEEPING ones is alive.
            // The sleeping ones are not decoration here — they are exactly what broke the selection: the balance
            // was counted as the number of ASSIGNED tasks, and a core with twelve sleeping ones
            // looked more loaded than a core with two computing ones. The second defect is
            // the cooling threshold: two ready threads on one core replace each other every
            // slice and therefore NEVER cool down.
            // The balancer is switched off (`talaria.nobalance=1`) — otherwise it is
            // unclear who resettled them, and the gate is about the stealing.
            static SB: &[u8] = user_elf!("stealbal");
            println!("talaria: STEALBAL — the stealing separates the computing ones while a pool of sleeping ones is there ({} bytes)", SB.len());
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(2, Ordering::SeqCst);
            for cpu in 1..4usize {
                if !crate::smp::start_cpu(cpu) { println!("STEAL: FAIL — AP{} did not come up (-smp 4 is required)", cpu); }
            }
            unsafe { crate::user::run_threaded(SB) };
        }
        "rqwait" => {
            // Session 26: the metric "ready but not running" is judged by a workload with a
            // known answer — three computing threads. On ONE core (pin=*:1,
            // without stealing and balancing) two out of three always wait for the processor, and
            // the sum of the waiting is bound to be about two walls; on four cores
            // there is almost nothing to wait for. The share of the wall is printed by `rqwait_report` from
            // `metrics::dump` — the only point where the workload has already
            // run and the counters are still alive.
            static RQ: &[u8] = user_elf!("rqwait");
            println!("talaria: RQWAIT — the metric \"ready->processor\" on three computing threads ({} bytes)", RQ.len());
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(2, Ordering::SeqCst);
            for cpu in 1..4usize {
                if !crate::smp::start_cpu(cpu) { println!("RQWAIT: FAIL — AP{} did not come up (-smp 4 is required)", cpu); }
            }
            RQWAIT_T0.store(crate::tsc::mono_ns(), Ordering::SeqCst);
            unsafe { crate::user::run_threaded(RQ) };
        }
        "hintpick" => {
            // Session 28: AN EMPTY CORE IS BOUND TO APPLY THE HINT.
            //
            // A wake-up gives away the processor by address: the waker puts the task's number
            // into the hint slot of its core and rings. On a core standing in the pickup loop
            // (there is no task on it) that slot WAS NOT READ at all — the loop took the first
            // task of its core by number, without even asking whether it was ready.
            //
            // The gate's arrangement repeats that case with the minimal cast:
            //   t0 (A) — sleeps on a word that nobody writes: there is nothing to wake it with;
            //   t1 (X) — puts a hint about t2 and TERMINATES (the core goes into the
            //            pickup loop — the only way to get there);
            //   t2 (B) — the target of the hint, which has never yet run.
            // All three are on core 1, the numbers increase: without the fix the loop takes A
            // (the smaller number), it rechecks its word, parks again and
            // yields — TWO extra context switches with a reload of CR3,
            // and that is exactly the case which on the stand gives 89 % of the wake-ups in
            // the bucket "ready->processor 5-20 us".
            // What is judged is not the time (the difference is single microseconds, a gate
            // must not be held on them) but the NUMBER of switches of A: with the fix it leaves
            // the core ONCE (having yielded to X), without the fix — twice.
            println!("talaria: HINTPICK — the pickup loop of an empty core applies the hint (-smp 2 is required)");
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(2, Ordering::SeqCst);
            if !crate::smp::start_cpu(1) { println!("hintpick: FAILED — AP1 did not come up (-smp 2 is required)"); crate::suite::park() }
            {   // core 1 is bound to announce itself as a picker of tasks before the first move
                let t0 = crate::tsc::rdtsc(); let hz = crate::tsc::calibrate_cached();
                while !crate::sched::sched_cpu(1) && crate::tsc::rdtsc().wrapping_sub(t0) < hz { core::hint::spin_loop(); }
            }
            if !crate::sched::sched_cpu(1) { println!("hintpick: FAILED — core 1 did not stand in the pickup loop"); crate::suite::park() }
            // SLOT 0 IS OCCUPIED BY A STUB, AND THAT IS NOT DECORATION. The BSP's scheduler
            // considers the current task to be the number from `CURRENT`, and since the boot that is
            // zero: `is_running(0)` answers "it is running", and the move of task 0 onto
            // another core is rejected silently. The first version of the gate came out
            // green on exactly that, for nothing — task A stayed on the BSP, where
            // the timer is not wound up, and the pickup loop had nothing to choose from.
            unsafe {
                crate::sched::spawn(task_hp_idle, crate::mem::alloc_frame().unwrap() + 4096); // t0 — the stub
                crate::sched::spawn(task_hp_a, crate::mem::alloc_frame().unwrap() + 4096);    // t1 — A
                crate::sched::spawn(task_hp_x, crate::mem::alloc_frame().unwrap() + 4096);    // t2 — X
                crate::sched::spawn(task_hp_b, crate::mem::alloc_frame().unwrap() + 4096);    // t3 — B
            }
            // THE TIMER VECTOR GOES TO THE SCHEDULER. Without that the `int 0x41` from
            // `yield_now` lands in the stub (acknowledge the interrupt and leave),
            // and giving away the core does exactly nothing: A spun in the waiting
            // loop, and X and B stood ready on the same core.
            crate::sched::install();
            // A leaves first and alone: the core will take it (the smaller number) and will fall asleep
            // together with it on the word. Only then do X and B arrive on the core.
            if !crate::sched::migrate_idle_task(1, 1) { println!("hintpick: FAILED — A did not move onto core 1"); crate::suite::park() }
            {   let t0 = crate::tsc::rdtsc(); let hz = crate::tsc::calibrate_cached();
                while HP_A_PARKED.load(Ordering::SeqCst) == 0 && crate::tsc::rdtsc().wrapping_sub(t0) < hz { core::hint::spin_loop(); } }
            if HP_A_PARKED.load(Ordering::SeqCst) == 0 { println!("hintpick: FAILED — A did not get core 1 within a second"); crate::suite::park() }
            crate::sched::migrate_idle_task(2, 1);
            crate::sched::migrate_idle_task(3, 1);
            // WAKE A UP SO THAT IT GIVES THE CORE TO X. There is no tick on this core:
            // without a manifest the AP's role is "dedicated" (run-to-completion), and
            // there is nobody to take the core away from A. The woken A rechecks the word,
            // sees ready neighbours next to it and yields by itself — that is the regular path
            // of a waiter, not a prop for the gate.
            HP_WORD.store(1, Ordering::SeqCst);
            crate::sync::wake_addr_kernel(crate::mem::vaddr(HP_WORD.as_ptr()), 1);
            crate::suite::park()
        }
        "ioyacct" => {
            // Session 26: giving away the core while waiting for a DEVICE (`fs::io_wait_step`)
            // must not be counted as waiting for the PROCESSOR — otherwise the metric
            // "ready->processor" ascribes the whole barrier to the one waiting for the barrier
            // (WTJourn.Flusher "waited 10.7 s" under the gp3 throttle). This cannot be judged
            // by a workload with a disk: the length of a barrier is a property of the STAND (on
            // tmpfs the barrier is instantaneous, there is nothing to yield), and the gate on fsyncconc
            // was now green, now red depending on where the image lies. Here it is
            // pure arithmetic of the accounting on one core: task A for half a second
            // gives away the core under the io_yield marker, task B computes. For A
            // "was yielding to the device" is bound to be almost the whole wall, "was waiting" —
            // almost zero, and the three terms are bound to add up to the wall.
            println!("talaria: IOYACCT — the accounting of a yield to the device separately from waiting for the processor (one core, two ring 0 tasks)");
            unsafe {
                crate::sched::spawn(task_ioy_yielder, crate::mem::alloc_frame().unwrap() + 4096); // t0
                crate::sched::spawn(task_ioy_spinner, crate::mem::alloc_frame().unwrap() + 4096); // t1
                crate::smp::lapic_enable();
                crate::smp::lapic_timer_init(crate::smp::SLICE_NS);
                crate::sched::install();
                // Without an sti — as in user::run_threaded: IF=1 will be given to the tasks by the iretq
                // out of the spawn frame, whereas a tick caught BEFORE start() would have saved
                // the boot context over the frame of task 0.
                crate::sched::start();
            }
        }
        "smtpack" => {
            // Session 35, step 2: the order of the logical cores decides HOW MANY
            // PHYSICAL cores `talaria.smp=N` occupies — and on that depends whether
            // the host is left anywhere to run (session 34: steal 249 ms against
            // 2 ms). There is no machine with such a layout at home, so what is judged is
            // the permutation on synthetic layouts (as with `affinity`).
            println!("talaria: SMTPACK — the permutation of the logical cores by the hyperthreads of one physical one");
            crate::smp::selftest_pack();
            crate::suite::park()
        }
        "iowin" => {
            // SESSION 35: a waiter for a disk command that must not be given the core away
            // (it holds the FS lock, or there is nobody to give it to) spun with IF=0 — and
            // the tick did not arrive at the core FOR THE WHOLE TIME OF THE COMMAND. The instrument "the tick's hole"
            // named `#18 pwrite64` with holes of 16-18 ms at ZERO time of the host.
            // The gate judges the mechanism itself, not the disk (the length of a command is a property
            // of the stand): a ring 0 task stands in `fs::io_wait_step` with the interrupts
            // closed for 50 ms, and with `talaria.iowin=1` there ARE ticks on its core,
            // whereas with 0 (the former behaviour) there is not one.
            println!("talaria: IOWIN — the window of interrupts while waiting for the device (the tick reaches a core standing in io_wait_step)");
            unsafe {
                crate::sched::spawn(task_iowin, crate::mem::alloc_frame().unwrap() + 4096);
                crate::smp::lapic_enable();
                crate::smp::lapic_timer_init(crate::smp::SLICE_NS);
                crate::sched::install();
                crate::sched::start();   // without an sti — see ioyacct
            }
        }
        "conprint" => {
            // Session 26: a `println!` with arguments is SEVERAL write_str calls
            // (a literal, an argument, a literal...), and the output lock was taken on every
            // piece. A neighbouring core inserted its own line in the middle of a foreign one — in
            // every run of mongodb four splices per seven hundred lines were found.
            // The analysis of the log is an instrument, and a splice makes it a liar.
            // The gate: four ring 0 threads print formatted lines all at once,
            // the host counts WHOLE lines (there are bound to be exactly 4*60 of them) and looks for
            // a line inside which a foreign one started.
            println!("talaria: CONPRINT — a line is printed into the console AS A WHOLE (the lock is on a line, not on a piece of a format)");
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(2, Ordering::SeqCst);
            for cpu in 1..4usize {
                if !crate::smp::start_cpu(cpu) { println!("conprint: FAIL — AP{} did not come up (-smp 4 is required)", cpu); }
            }
            unsafe {
                for _ in 0..4 { crate::sched::spawn(task_conprint, crate::mem::alloc_frame().unwrap() + 4096); }
                crate::smp::lapic_timer_init(crate::smp::SLICE_NS);
                crate::sched::install();
                crate::sched::start();   // without an sti — see ioyacct
            }
        }
        "schedpin" => {
            // V1.9: a rule by the thread's name. The thread names itself — the kernel moves it
            // onto a dedicated core, whose number it has chosen itself.
            static SP: &[u8] = user_elf!("schedpin");
            println!("talaria: SCHEDPIN — the pinning of threads by name (the pin= rules)  ({} bytes)", SP.len());
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(2, Ordering::SeqCst);
            for cpu in 1..4usize {
                if !crate::smp::start_cpu(cpu) { println!("PIN: FAIL — AP{} did not come up (-smp 4 is required)", cpu); }
            }
            unsafe { crate::user::run_threaded(SP) };
        }
        "smp2" => {
            // PLAN-SYNC stage 0: a thread migrates onto an AP (sched_setaffinity), syscalls from the AP,
            // a parallel speedup of ~2x. An AP = the pickup loop of pinned tasks (crate::sched::ap_loop).
            static SM: &[u8] = user_elf!("smp2");
            println!("talaria: SMP2 — the migration of a thread onto an AP + parallelism ({} bytes)", SM.len());
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(2, Ordering::SeqCst);
            if !crate::smp::start_cpu(1) { println!("SMP2: FAIL — the AP did not come up (-smp 2 is required)"); }
            unsafe { crate::user::run_threaded(SM); }
        }
        "sync" => {
            // PLAN-SYNC stage 0: a cross-core wake-up by a write into a word (ring 0).
            // The AP sleeps in kwait_once (an mwait on the line / hlt+IPI), the BSP writes and measures.
            println!("talaria: the sync self-test — the AP sleeps on a word, the BSP wakes it by a write");
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(1, Ordering::SeqCst);
            if !crate::smp::start_cpu(1) { println!("SYNC: FAIL — the AP did not come up (-smp 2 is required)"); crate::suite::park() }
            crate::sync::selftest_bsp(1000);
            crate::suite::park()
        }
        _ => return false,
    }
    true
}

/// The moment the `rqwait` gate started (ns of the monotonic clock); zero — the gate is not armed.
static RQWAIT_T0: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub fn rqwait_armed() -> bool { RQWAIT_T0.load(Ordering::Relaxed) != 0 }

/// The report of the `rqwait` gate: the total waiting for the processor by ready tasks
/// against the wall of the whole run. The wall is counted from the arming to the end of the workload —
/// the loading of the binary is in it too, so the share on one core is a little below 200 %,
/// and the gate's threshold is 100 %.
pub fn rqwait_report() {
    let wall_ms = (crate::tsc::mono_ns() - RQWAIT_T0.load(Ordering::Relaxed)) / 1_000_000;
    let sum_ms = crate::sched::RQ_SUM_US.load(Ordering::Relaxed) / 1000;
    let share = if wall_ms == 0 { 0 } else { sum_ms * 100 / wall_ms };
    println!("RQWAIT: the ready ones waited for the processor for a total of {} ms out of {} ms of the wall — the share of the waiting is {}%, the worst is {} \
        us",
        sum_ms, wall_ms, share, crate::sched::RQ_MAX_US.load(Ordering::Relaxed));
}

/// The `ioyacct` gate, task A: for half a second it "waits for the device" — a short spin and
/// a yield of the core under the io_yield marker, as in `fs::io_wait_step`. Then the report.
extern "C" fn task_ioy_yielder() -> ! {
    let me = crate::sched::current_id();
    let t0 = crate::tsc::mono_ns();
    let wall = 500_000_000u64;
    let hz = crate::tsc::calibrate_cached().max(1);
    while crate::tsc::mono_ns().wrapping_sub(t0) < wall {
        // 50 us of "polling the device", then a yield — as after 64 spins in io_wait_step.
        let s = crate::tsc::rdtsc();
        while crate::tsc::rdtsc().wrapping_sub(s) < hz / 20_000 { core::hint::spin_loop(); }
        crate::sched::io_yield_begin();
        crate::sched::yield_now();
        crate::sched::io_yield_end();
    }
    let wall_ms = crate::tsc::mono_ns().wrapping_sub(t0) / 1_000_000;
    let (ioy, rq, run) = (crate::sched::ioy_ns(me) / 1_000_000, crate::sched::rq_ns(me) / 1_000_000, crate::sched::run_ns(me) / 1_000_000);
    let sum = ioy + rq + run;
    println!("ioyacct: the wall is {} ms; A: was yielding to the device {} ms, was waiting for the processor {} ms, was running {} ms, the sum is {} ms",
        wall_ms, ioy, rq, run, sum);
    crate::metrics::print_waiters("ioyacct:  ");
    let ok = ioy * 10 >= wall_ms * 6            // the yield is the greater part of the wall
        && rq * 10 <= wall_ms                   // the waiting for the processor is crumbs (pre-emption by a tick inside the spin)
        && sum + 50 >= wall_ms && sum <= wall_ms + 50; // the terms add up to the wall
    println!("ioyacct: {}", if ok { "ALL OK" } else { "FAILED — the yield to the device was counted as waiting for the processor, or was lost" });
    crate::suite::park()
}
/// The `hintpick` gate, the word of task A: nobody writes it — A sleeps on it forever.
static HP_WORD: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// A has announced itself (has gone to sleep), B has run, the numbers of the tasks.
static HP_A_PARKED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static HP_B_RAN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The `hintpick` gate, task A: sleeps on a word that nobody writes.
extern "C" fn task_hp_a() -> ! {
    loop {
        let v = HP_WORD.load(Ordering::SeqCst);
        HP_A_PARKED.store(1, Ordering::SeqCst);
        crate::sync::wait_on(HP_WORD.as_ptr() as *const u32, v, None);
    }
}
/// The `hintpick` gate, task X: hint task B to the core and leave the core entirely.
extern "C" fn task_hp_x() -> ! {
    crate::sched::hint_set_on(1, 3);
    crate::sched::exit_current()
}
/// The `hintpick` gate, the stub of slot 0: neither A nor B must land in it.
extern "C" fn task_hp_idle() -> ! { crate::suite::park() }
/// The `hintpick` gate, task B: the target of the hint. It also prints the verdict — by its
/// first tick everything the gate judges has already happened.
extern "C" fn task_hp_b() -> ! {
    HP_B_RAN.store(1, Ordering::SeqCst);
    let sw = crate::sched::switches(1); // A — slot 1
    let pick = crate::sched::HINT_PICKUP.load(Ordering::Relaxed);
    println!("hintpick: A left the core {} time(s), hints applied by the pickup loop {}, B has run", sw, pick);
    let ok = sw == 1 && pick >= 1;
    println!("hintpick: {}", if ok { "ALL OK" } else { "FAILED — the pickup loop took the task by number, not by the hint" });
    crate::suite::park()
}
/// The `iowin` gate: 50 ms in the device-waiting step with the interrupts closed —
/// first with the window (`talaria.iowin=1`), then without it (0). It counts the ticks of ITS OWN
/// core, and that says whether the tick was reaching it.
extern "C" fn task_iowin() -> ! {
    use core::sync::atomic::Ordering::SeqCst;
    // The scheduler is up (`spawn`+`install`+`start`), that is, there is already something to yield and
    // pre-empt with: that is what the window requires (`io_window`).
    crate::syscall::THREADED.store(true, SeqCst);
    let cpu = crate::percpu::cpu_id();
    let hz = crate::tsc::calibrate_cached().max(1);
    let mut ticks = [0u64; 2];
    let mut wins = [0u64; 2];
    for (i, mode) in [1u32, 0].iter().enumerate() {
        crate::fs::IO_WIN_MODE.store(*mode, SeqCst);
        let w0 = crate::fs::IO_WIN_N.load(SeqCst);
        unsafe { crate::arch::cpu::irq_disable(); } // as in a syscall
        let (b0, i0) = crate::sched::cpu_ticks(cpu);
        let t0 = crate::tsc::rdtsc();
        let mut spins = 0u32;
        while crate::tsc::rdtsc().wrapping_sub(t0) < hz / 20 { crate::fs::io_wait_step(&mut spins); }
        let (b1, i1) = crate::sched::cpu_ticks(cpu);
        unsafe { crate::arch::cpu::irq_enable(); }
        ticks[i] = (b1 + i1) - (b0 + i0);
        wins[i] = crate::fs::IO_WIN_N.load(SeqCst) - w0;
    }
    crate::fs::IO_WIN_MODE.store(1, SeqCst);
    println!("iowin: 50 ms in io_wait_step with IF=0: with the window ticks {} (windows {}), without the window ticks {} (windows {})",
        ticks[0], wins[0], ticks[1], wins[1]);
    let ok = ticks[0] >= 10 && wins[0] > 0 && ticks[1] == 0 && wins[1] == 0;
    println!("iowin: {}", if ok { "ALL OK" } else { "FAILED — the window does not open (there is no tick) or it opens with iowin=0" });
    crate::suite::park()
}

/// The `ioyacct` gate, task B: computes, waits for nothing.
extern "C" fn task_ioy_spinner() -> ! {
    loop { core::hint::spin_loop(); }
}

/// The `conprint` gate: prints formatted lines racing with the neighbours.
/// There are several arguments on purpose — each of them is a separate piece for `write!`.
const CP_LINES: u64 = 60;
static CP_DONE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
extern "C" fn task_conprint() -> ! {
    let me = crate::sched::current_id();
    for i in 0..CP_LINES {
        println!("conprint: t{} n{} {} {} {} tail", me, i, i * 3, i * 7, i * 11);
    }
    if CP_DONE.fetch_add(1, Ordering::SeqCst) + 1 == 4 {
        println!("conprint: threads 4, lines per thread {}", CP_LINES);
        println!("conprint: ALL OK");
    }
    crate::suite::park()
}
