//! The workload supervisor: a declarative launch of a cell from disk + a restart on
//! a crash. The key part is recovery after a ring-3 fault (setjmp/longjmp): a fault in
//! the workload does not bring the kernel down but returns control to the supervisor, which
//! restarts the workload (isolating a workload failure from the control plane).
use core::sync::atomic::{AtomicU64, AtomicBool, Ordering};

#[repr(C)]
pub struct JmpBuf { regs: [u64; crate::arch::setjmp::JMPBUF_WORDS] } // the callee-saved set, sp and the return address of the machine
pub static mut RECOVERY: JmpBuf = JmpBuf { regs: [0; crate::arch::setjmp::JMPBUF_WORDS] };
pub static RECOVERY_ARMED: AtomicBool = AtomicBool::new(false);
pub static CRASH_RIP: AtomicU64 = AtomicU64::new(0);
pub static CRASH_VEC: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" { pub fn tal_setjmp(buf: *mut JmpBuf) -> u64; pub fn tal_longjmp(buf: *mut JmpBuf, val: u64) -> !; }
pub static EXIT_CODE_LAST: AtomicU64 = AtomicU64::new(0);

// Memory control between restarts: free_frames after EVERY state reset; the drift
// is the difference from the previous restart (the first restart includes the one-off allocations of the boot path
// — the workload's stack and heap — and is printed without a drift). A steady drift >0 = a leak per restart.
static LAST_FREE: AtomicU64 = AtomicU64::new(0);
fn mem_report_restart() {
    let now = crate::mem::free_total_frames(); // bump + free list: what is really free
    let prev = LAST_FREE.swap(now, Ordering::SeqCst);
    if prev == 0 {
        println!("supervise: memory: free={}MB (the baseline after the first restart)", now * 4 / 1024);
    } else {
        let drift_kb = (prev as i64 - now as i64) * 4;
        println!("supervise: memory: free={}MB drift={}KB", now * 4 / 1024, drift_kb);
    }
}

/// A crash of a multithreaded workload that reached the supervisor by a roundabout path:
/// the threads are marked dead on the core where the exception happened, while control
/// is returned to the supervisor by the BSP ("all tasks finished", longjmp 2). The reason
/// is kept separately from `CRASH_VEC`: the "all finished" path zeroes it.
static CRASH_PEND: AtomicU64 = AtomicU64::new(0);      // sig << 8 | vec, 0 — none
static CRASH_PEND_RIP: AtomicU64 = AtomicU64::new(0);
pub fn crashed(rip: u64, vec: u64, sig: u64) {
    CRASH_PEND_RIP.store(rip, Ordering::SeqCst);
    CRASH_PEND.store((sig << 8) | (vec & 0xFF), Ordering::SeqCst);
}

/// The `exit_group` of a multithreaded workload: the threads are stopped, but control
/// is returned to the supervisor by the BSP (longjmp 2) — the thread that called `exit_group` could
/// have been on any AP, and jumping onto the supervisor's stack from there would carry the AP away.
static EXIT_PEND: AtomicU64 = AtomicU64::new(0); // the code + 1, 0 — none
pub fn exit_pending(code: i32) {
    EXIT_CODE_LAST.store(code as u64, Ordering::SeqCst);
    EXIT_PEND.store(code as u64 + 1, Ordering::SeqCst);
}
/// A restart by the operator/watchdog: the threads are killed (`sched::kill_all`), and the supervisor
/// gets control once they have all stopped.
static RESTART_PEND: AtomicBool = AtomicBool::new(false);

/// The process finished normally (exit/exit_group). Under the supervisor — return control
/// to it (longjmp, val=3: a normal exit); otherwise — halt the kernel.
pub fn process_exited(code: i32) -> ! {
    EXIT_CODE_LAST.store(code as u64, Ordering::SeqCst);
    if RECOVERY_ARMED.load(Ordering::SeqCst) {
        RECOVERY_ARMED.store(false, Ordering::SeqCst);
        CRASH_VEC.store(0, Ordering::SeqCst);
        unsafe { tal_longjmp(core::ptr::addr_of_mut!(RECOVERY), 3); }
    }
    crate::suite::park(); // the end of the workload = the end of a suite item, not a hang
}

// ── Housekeeping on top of the running workload (from the scheduler tick, IF=0) ────
pub static WATCHDOG_SECS: AtomicU64 = AtomicU64::new(0);   // 0 = off; otherwise: no rx for N s -> a restart
pub static STATUS_SECS: AtomicU64 = AtomicU64::new(10);    // the status line's period (0 = off)
static RESTART_REQ: AtomicBool = AtomicBool::new(false);
static LAST_STATUS: AtomicU64 = AtomicU64::new(0);
static LAST_RX: AtomicU64 = AtomicU64::new(0);
static LAST_RX_TSC: AtomicU64 = AtomicU64::new(0);
pub static START_TSC: AtomicU64 = AtomicU64::new(0);
/// Request a restart of the workload (the console/the operator): it happens on the next tick.
pub fn request_restart() { RESTART_REQ.store(true, Ordering::SeqCst); }
/// The scheduler tick: the status line, the watchdog on rx progress, the console, a requested restart.
pub fn sched_tick() {
    let hz = crate::tsc::calibrate_cached(); if hz == 0 { return; }
    let now = crate::tsc::rdtsc();
    crate::console::poll_tick();
    let (rx, tx, live) = crate::netsys::totals();
    if rx != LAST_RX.load(Ordering::Relaxed) { LAST_RX.store(rx, Ordering::Relaxed); LAST_RX_TSC.store(now, Ordering::Relaxed); }
    let st = STATUS_SECS.load(Ordering::Relaxed);
    if st > 0 && now.wrapping_sub(LAST_STATUS.load(Ordering::Relaxed)) >= st * hz {
        LAST_STATUS.store(now, Ordering::Relaxed);
        let up = now.wrapping_sub(START_TSC.load(Ordering::Relaxed)) / hz;
        println!("cell: uptime={}s conns={} rx={} tx={} syscalls={} faults={} free={}MB low={}MB khigh={} restarts={} threads={}",
            up, live, rx, tx, crate::metrics::syscalls(), crate::metrics::page_faults(),
            crate::mem::free_total_frames() * 4 / 1024, crate::mem::free_low_frames() * 4 / 1024,
            crate::mem::ANY_FROM_HIGH.load(Ordering::Relaxed), crate::telemetry::restarts(), crate::sched::task_count());
    }
    let wd = WATCHDOG_SECS.load(Ordering::Relaxed);
    let stalled = wd > 0 && LAST_RX_TSC.load(Ordering::Relaxed) != 0 && now.wrapping_sub(LAST_RX_TSC.load(Ordering::Relaxed)) >= wd * hz;
    if stalled { println!("watchdog: no incoming data for {} s — STALL -> restarting the workload", wd); LAST_RX_TSC.store(now, Ordering::Relaxed); }
    if (stalled || RESTART_REQ.swap(false, Ordering::SeqCst)) && RECOVERY_ARMED.load(Ordering::SeqCst)
        && !RESTART_PEND.swap(true, Ordering::SeqCst) {
        // Jumping into the supervisor FROM HERE is not allowed: the workload's threads run on APs, and
        // `sched::reset()` would give away their stacks and address space while they are
        // executing on them (that is how a restart on four cores ended in a "KVM
        // internal error"). The threads are killed, finish what they started in the kernel and
        // stop; control is returned by the BSP once they have all stopped.
        crate::sched::kill_all(0);
    }
}

/// The supervisor of a MULTITHREADED disk workload (mode=cell): a launch under the scheduler
/// with argv/envp; a return on a fault (longjmp 1), "all threads finished" (2) or exit (3).
/// Between restarts the kernel's per-process state (sockets/fds/the scheduler) is reset.
pub fn supervise_threaded(path: *const u8, args: &[&[u8]], envs: &[&[u8]], max_restarts: u32) -> ! {
    let mut attempt: u32 = 0;
    loop {
        let r = unsafe { tal_setjmp(core::ptr::addr_of_mut!(RECOVERY)) };
        if r == 0 {
            RECOVERY_ARMED.store(true, Ordering::SeqCst);
            let now = crate::tsc::rdtsc();
            START_TSC.store(now, Ordering::SeqCst); LAST_STATUS.store(now, Ordering::SeqCst); LAST_RX_TSC.store(now, Ordering::SeqCst);
            println!("supervise: starting the workload (attempt {}/{}, multithreaded)", attempt + 1, max_restarts + 1);
            unsafe { crate::user::run_disk_threaded(path, args, envs); }
        }
        // the longjmp lands here: 1 — a fault, 2 — all threads finished, 3 — exit/exit_group
        RECOVERY_ARMED.store(false, Ordering::SeqCst);
        unsafe { crate::arch::cpu::irq_disable(); }
        crate::smp::lapic_timer_stop();
        let code = EXIT_CODE_LAST.load(Ordering::SeqCst);
        let pend = CRASH_PEND.swap(0, Ordering::SeqCst);
        let exitp = EXIT_PEND.swap(0, Ordering::SeqCst);
        let restartp = RESTART_PEND.swap(false, Ordering::SeqCst);
        let r = if r == 2 && pend != 0 {
            CRASH_VEC.store(pend & 0xFF, Ordering::SeqCst);
            CRASH_RIP.store(CRASH_PEND_RIP.load(Ordering::SeqCst), Ordering::SeqCst);
            println!("supervise: the workload's threads were killed by signal {} (one thread crashing = the process crashing)", pend >> 8);
            1
        } else if r == 2 && exitp != 0 { 3 }
        else if r == 2 && restartp { 4 }
        else { r };
        let code = if r == 3 { EXIT_CODE_LAST.load(Ordering::SeqCst) } else { code };
        match r {
            1 => println!("supervise: THE WORKLOAD CRASHED (exception #{} rip={:#x}) — the kernel is intact",
                          CRASH_VEC.load(Ordering::SeqCst), CRASH_RIP.load(Ordering::SeqCst)),
            3 => println!("supervise: the workload finished normally (code {})", code),
            4 => println!("supervise: a restart on request (the watchdog/the operator)"),
            _ => println!("supervise: all the workload's threads finished"),
        }
        crate::fr::record(crate::fr::RESTART, attempt as u64);
        crate::telemetry::inc_restart();
        let stuck = crate::sched::wait_aps_off_tasks(1000);
        if stuck > 0 { println!("supervise: {} cores did not leave the crashed instance's tasks within 1 s", stuck); }
        crate::syscall::reset_process_state();
        crate::sched::reset();
        mem_report_restart();
        if crate::acpi::shutdown_requested() {
            println!("supervise: a normal machine shutdown — the workload exited (code {}), there will be no restart", code);
            crate::suite::park();
        }
        if r == 3 && code == 0 { println!("supervise: code 0 — no restart is needed, halting"); crate::suite::park(); }
        attempt += 1;
        if attempt > max_restarts {
            println!("supervise: the restart limit ({}) is exhausted — halting", max_restarts);
            crate::suite::park();
        }
        println!("supervise: restarting...");
    }
}

/// Start a workload from disk under supervision. path/args are as in run_disk.
pub fn supervise(path: *const u8, args: &[&[u8]], max_restarts: u32) -> ! {
    let mut attempt: u32 = 0;
    loop {
        let recovered = unsafe { tal_setjmp(core::ptr::addr_of_mut!(RECOVERY)) };
        if recovered == 0 {
            // the first pass: arm the recovery and start the workload
            RECOVERY_ARMED.store(true, Ordering::SeqCst);
            println!("supervise: starting the workload (attempt {}/{})", attempt + 1, max_restarts + 1);
            unsafe { crate::user::run_disk(path, args); } // does not return normally
        } else {
            // a longjmp from the fault handler lands here — the workload crashed
            RECOVERY_ARMED.store(false, Ordering::SeqCst);
            let rip = CRASH_RIP.load(Ordering::SeqCst);
            let vec = CRASH_VEC.load(Ordering::SeqCst);
            println!("supervise: THE WORKLOAD CRASHED (exception #{} rip={:#x}) — the kernel is intact", vec, rip);
            crate::fr::record(crate::fr::RESTART, attempt as u64);
            crate::telemetry::inc_restart(); // the restart counter
            crate::syscall::reset_process_state(); // the instance's fds/network/mmap regions (run_disk's stack is static)
            mem_report_restart();
            attempt += 1;
            if attempt > max_restarts {
                println!("supervise: the restart limit ({}) is exhausted — halting", max_restarts);
                crate::suite::park();
            }
            println!("supervise: a restart through memory recovery (free-list reclaim)...");
            // (the restart: the next loop iteration does setjmp+run_disk again)
        }
    }
}

// `tal_setjmp`/`tal_longjmp` are `arch::setjmp` (x86-64 asm); the declarations are above.
