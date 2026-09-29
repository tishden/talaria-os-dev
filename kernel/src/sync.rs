//! PLAN-SYNC: cheap synchronisation without a busy spin. The detection of the hardware
//! wait primitives (WAITPKG/MONITORX/MONITOR) and a kernel wait on a cache line:
//! the sleeper arms the monitor and falls asleep (mwait), the waker pays nothing —
//! an ordinary write into the word wakes the core without a syscall and without an IPI. The fallback without
//! MONITOR (KVM without -overcommit cpu-pm=on): sti;hlt + an IPI kick from the waker.
//!
//! # Safety
//!
//! A missed wakeup is this module's worst defect: nothing crashes, a thread simply
//! stops, and once it took a whole CORE out of rotation (a waiter preempted
//! between "the LVT timer is borrowed" and "it is returned" left its core without
//! a periodic tick for good). So the `unsafe` here rests on four invariants, named
//! W1..W4 at the sites.
//!
//! - **W1. The word waited on belongs to the WORKLOAD, and it is read, never
//!   trusted.** A sleeper passes an address the syscall layer validated
//!   (`uaccess.rs`, the `futex`/`upark` signatures), so the load cannot fault and
//!   cannot reach the kernel. Its VALUE may change under us at any moment — that
//!   is the point of the wait — so every read is `read_volatile` and the decision
//!   is made by `waitpred`, which both sides share.
//! - **W2. A `static mut` here is INDEXED BY TASK** (`WADDR`/`WVAL`/`WKIND`/
//!   `WSLEEP`/`WOKEN`...): a task writes its own row before it sleeps and reads it
//!   after it wakes; a waker writes another task's row only through the atomics.
//!   The task index comes from the scheduler, so two cores never share a row.
//! - **W3. `monitor`/`mwait` and the LAPIC are THIS core's machine state.** The
//!   monitor must be armed on the same core that then sleeps, and between arming
//!   and `mwait` the predicate is re-read — otherwise the write that should have
//!   woken us happens in the gap. A borrowed TSC-deadline timer belongs to the
//!   CORE, not to the task, so whoever takes the core returns it
//!   (`demote_preempted_sleeper`).
//! - **W4. `sti`/`hlt` windows are one instruction wide.** The fallback path opens
//!   interrupts only around the `hlt`, after the predicate has been checked: a
//!   wider window loses the wakeup, a narrower one never wakes.
// This module is done (every `unsafe` carries its invariant), so the lint the crate
// switches off in `main.rs` is switched back ON here: from now on a new unsafe operation
// inside an `unsafe fn` of this file has to say so out loud.
#![warn(unsafe_op_in_unsafe_fn)]
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering, fence};
use crate::arch::cpu;

/// The wait mode (in decreasing order of desirability). On the i7-3820 stand (Sandy Bridge-E)
/// there is no WAITPKG/MONITORX — the working mode is MWAIT (tier 2 of PLAN-SYNC §7).
pub const MODE_HLT: u8 = 0;     // sti;hlt + an IPI wakeup (without MONITOR in the guest)
pub const MODE_MWAIT: u8 = 1;   // ring0 MONITOR/MWAIT — a write wakes without an IPI
pub const MODE_MWAITX: u8 = 2;  // AMD user MONITORX/MWAITX (tier 1 in ring 3)
pub const MODE_WAITPKG: u8 = 3; // Intel user UMONITOR/UMWAIT (tier 1 in ring 3)

static MODE: AtomicU8 = AtomicU8::new(MODE_HLT);
pub fn mode() -> u8 { MODE.load(Ordering::Relaxed) }
pub fn mode_name(m: u8) -> &'static str {
    match m { MODE_WAITPKG => "waitpkg(umwait)", MODE_MWAITX => "mwaitx", MODE_MWAIT => "mwait(ring0)", _ => "hlt+ipi" }
}

fn cpuid(leaf: u32, sub: u32) -> (u32, u32, u32, u32) { cpu::cpuid_count(leaf, sub) }

/// The TSC-deadline LAPIC timer (for the timeouts of sleep on an AP without a periodic tick).
static TSC_DL: AtomicBool = AtomicBool::new(false);
/// `talaria.sync=hlt|mwait` — 0 is "as detected", otherwise the tier+1 (A12).
pub static FORCE_MODE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Set the wait tier from the outside. The cmdline parsing happens LATER than `init`, so
/// the tier is set here too — otherwise the knob would silently be late (exactly that class of
/// "the option is parsed after it has been applied" which cost a session).
pub fn force_mode(m: u8) {
    FORCE_MODE.store(m as usize + 1, Ordering::Relaxed);
    MODE.store(m, Ordering::SeqCst);
    crate::println!("sync: the wait tier is set from the outside -> {}", mode_name(m));
}

/// The detection at boot: pick the best available mode, allow a long umwait.
pub fn init() {
    let (_, _, c1, _) = cpuid(1, 0);
    let monitor = c1 & (1 << 3) != 0; // CPUID.1:ECX.MONITOR
    TSC_DL.store(c1 & (1 << 24) != 0, Ordering::SeqCst); // CPUID.1:ECX.TSC-Deadline
    let (_, _, c7, _) = cpuid(7, 0);
    let waitpkg = c7 & (1 << 5) != 0; // CPUID.7.0:ECX.WAITPKG
    let (_, _, c8, _) = cpuid(0x8000_0001, 0);
    let mwaitx = c8 & (1 << 29) != 0; // CPUID.80000001:ECX.MONITORX (AMD)
    let mut m = if waitpkg { MODE_WAITPKG } else if mwaitx { MODE_MWAITX } else if monitor { MODE_MWAIT } else { MODE_HLT };
    // A12: `talaria.sync=hlt|mwait` — a forced wait tier.
    //
    // What for. On `c6i.metal` the production mode does not start preempting: the timer
    // is programmed and counting, the ISR is clean, and the interrupts do not arrive at all.
    // Under a hypervisor the same path works — and there the tier is a different one (`hlt+ipi`,
    // MONITOR/MWAIT is not given to the guest). Telling "it is about the wait tier" from "it is about
    // something else" costs ONE run if the tier can be chosen from the outside; without
    // the knob it would cost a kernel change and a full image build cycle.
    if let Some(force) = FORCE_MODE.load(Ordering::Relaxed).checked_sub(1) {
        m = force as u8;
        crate::println!("sync: the wait tier IS SET from the outside (talaria.sync=)");
    }
    MODE.store(m, Ordering::SeqCst);
    if waitpkg {
        // IA32_UMWAIT_CONTROL: [31:2] the max TSC quanta (0 = no limit), bit0 = the prohibition of C0.2 (0 = allowed)
        // SAFETY: W3 — IA32_UMWAIT_CONTROL (0xE1) of THIS core: the ceiling of a user
        // `umwait` is cleared so that tier 1 does not wake by a timeout of its own.
        unsafe { cpu::wrmsr(0xE1, 0); }
    }
    crate::println!("sync: the wait mode is {} (monitor={} waitpkg={} mwaitx={})",
        mode_name(m), monitor as u8, waitpkg as u8, mwaitx as u8);
}

/// One falling asleep until a write into the word addr (or any interrupt/a false wake).
/// The caller spins the loop `while *addr == val && !brk() { kwait_once(...) }`.
/// The arm/check race is closed by the order: MONITOR -> a recheck -> MWAIT (a write
/// between them raises the armed flag and MWAIT returns at once). The interrupts
/// are allowed for the duration of the sleep (sti;mwait / sti;hlt), then IF is restored.
///
/// # Safety
/// W1 + W3: `addr` must be mapped and readable in the CURRENT address space (a
/// syscall's validated argument, or a kernel word), and the caller must be on the
/// core that will sleep. The order inside is the contract: arm the monitor, THEN
/// re-read the word — checking first and arming after loses the write that happens
/// in between, which is a wakeup lost for ever.
#[inline]
pub unsafe fn kwait_once(addr: *const u32, val: u32) {
    if MODE.load(Ordering::Relaxed) >= MODE_MWAIT {
        // SAFETY: W3 — MONITOR arms THIS core on the line of `addr` (rax=the line, ecx/edx=0).
        unsafe { cpu::monitor(addr); }
        // SAFETY: W1 — the re-read AFTER the arming: the workload may have written the word
        // between the two instructions, and then we must not sleep at all.
        if unsafe { core::ptr::read_volatile(addr) } != val { return; }
        crate::paging::halt_enter();
        // SAFETY: W3 + W4 — the interrupt window is exactly the `mwait`: a wakeup may come as
        // a write to the armed line OR as an interrupt, and `cli` closes the window at once.
        // `halt_enter/exit` tell the scheduler that this core's time is SLEEP, not work
        // (without that the sleep was charged to the thread as CPU%).
        unsafe { cpu::irq_enable_mwait_disable(); }
        crate::paging::halt_exit();
    } else {
        crate::paging::halt_enter();
        // SAFETY: W4 — without MONITOR the only wakeup is an interrupt, so the window is the
        // `hlt` itself; the waker sends an IPI kick (`wake_addr`).
        unsafe { cpu::irq_enable_halt_disable(); }
        crate::paging::halt_exit();
    }
}

/// kwait with "the WOKEN gate": like kwait_once, but between MONITOR and MWAIT it also checks
/// the task's WOKEN flag. It is needed by the pair with the IPI skip in wake_addr: the waker decides
/// "the store has already woken it" by the word's instantaneous value, but the word could have managed
/// to return to val (the ABA of a reused stack node of a musl condvar) — then the
/// monitor will be armed anew and there will be no store wakeup; WOKEN (SeqCst,
/// set BEFORE the decision to skip) is visible to any LATE check — there is no sleep.
///
/// # Safety
/// As `kwait_once`, plus W2: `woken` must be THIS task's flag — the one the waker sets
/// before it decides to skip the IPI.
#[inline]
pub unsafe fn kwait_once_wchk(addr: *const u32, val: u32, woken: &AtomicU8) {
    if MODE.load(Ordering::Relaxed) >= MODE_MWAIT {
        // SAFETY: W3 — as `kwait_once`: the monitor is armed on this core first.
        unsafe { cpu::monitor(addr); }
        // SAFETY: W1 + W2 — two checks between the arming and the sleep. The word may have
        // returned to `val` (a reused node of a musl condvar), and then only `WOKEN` — set
        // SeqCst before the waker decided to skip the IPI — says the wakeup already happened.
        if unsafe { core::ptr::read_volatile(addr) } != val { return; }
        if woken.load(Ordering::SeqCst) != 0 { return; }
        crate::paging::halt_enter();
        // SAFETY: W3 + W4 — as in `kwait_once`.
        unsafe { cpu::irq_enable_mwait_disable(); }
        crate::paging::halt_exit();
    } else {
        crate::paging::halt_enter();
        // SAFETY: W4 — as in `kwait_once`.
        unsafe { cpu::irq_enable_halt_disable(); }
        crate::paging::halt_exit();
    }
}

/// §7.5: A BOUNDED sleep until a write into the word addr — like kwait_once, but with a ceiling
/// in nanoseconds: before the sleep a TSC-deadline LAPIC timer is armed on a harmless
/// vector (the handler = EOI), so a "missed" wakeup costs max_ns rather
/// than an eternity. It is needed where we sleep on a device: under KVM/Nitro MONITOR/MWAIT
/// is not given to the guest (the hlt+ipi mode), and a HLT is woken only by an interrupt.
///
/// # Safety
/// W1 + W3: as `kwait_once`, and the LAPIC must be enabled (`lapic_enable`). The
/// borrowed TSC-deadline timer belongs to the CORE: it is disarmed on the way out, and
/// if this task is preempted in between, whoever takes the core disarms it
/// (`demote_preempted_sleeper`) — a core left without a periodic tick stops rotating
/// for good.
#[inline]
pub unsafe fn kwait_bounded(addr: *const u32, val: u32, max_ns: u64) {
    arm_tsc_deadline(crate::tsc::mono_ns() + max_ns);
    // SAFETY: W1 + W3 — the caller's contract, passed straight through; the timer armed
    // above is returned below whatever wakes us.
    unsafe { kwait_once(addr, val); }
    disarm_tsc_deadline();
}

/// An IPI kick of the core apic_id (for MODE_HLT and the deep tier 3: a sleeper in hlt will not
/// see the write — it is woken by the WAKE vector, the handler = EOI+iret).
/// Is the task `t` sleeping on its core's doorbell (WSLEEP=3): it is woken by a write into the doorbell.
pub fn sleeps_on_bell(t: usize) -> bool { t < NTASKS && WSLEEP[t].load(Ordering::SeqCst) == 3 }
/// A10: the argument is the LOGICAL core number (as in CPU_OF/ONLINE), the APIC id is taken
/// from the topology map. Before A10 the APIC id came here, and on a sparse bare-metal topology
/// the wakeup went to the wrong core (or into nowhere).
pub fn kick(cpu: u32) { crate::smp::send_ipi(crate::smp::apic_of(cpu as usize), crate::idt::WAKE_VECTOR as u8); }

// ── The registry of futex waiters (PLAN-SYNC tiers 2–3) ─────────────────────
// The task t sleeps IN THE KERNEL on the user word WADDR[t]: in mwait mode the monitor is armed
// right on the futex word — a write BY THE WAKER IN USER SPACE wakes the core without a syscall
// and without an IPI (even with an unmodified musl: the store before FUTEX_WAKE already wakes it).
// FUTEX_WAKE sets WOKEN and finishes the job: same-core BSP — a directed switch (yield_to),
// cross-core — an IPI reschedule of the BSP / a kick of the AP (the insurance of the hlt mode).

use crate::sched::NTASKS;
static WADDR: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS];
static WOKEN: [AtomicU8; NTASKS] = [const { AtomicU8::new(0) }; NTASKS];
static WDEADLINE: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS];
static WSKIP: [AtomicU32; NTASKS] = [const { AtomicU32::new(0) }; NTASKS];
// For the targeted skip of an IPI in wake_addr (the mwait mode: the waker's store has ALREADY
// woken the sleeper in kwait — a kick/an IPI reschedule is not needed): the expected value of the
// word, the kind predicate and the sleep's phase (0 none / 1 a kwait on the word / 2 a yield parking).
static WVAL: [AtomicU32; NTASKS] = [const { AtomicU32::new(0) }; NTASKS];
static WKIND: [AtomicU8; NTASKS] = [const { AtomicU8::new(0) }; NTASKS];
static WSLEEP: [AtomicU8; NTASKS] = [const { AtomicU8::new(0) }; NTASKS];
/// The task t is RIGHT NOW holding the LVT timer of its core in TSC-deadline mode (a wait
/// by time on an AP). It is needed so that the preemption of this task returns the timer to
/// the scheduler: the LVT is a register OF THE CORE, not of the task (see `demote_preempted_sleeper`).
static WARMED: [AtomicBool; NTASKS] = [const { AtomicBool::new(false) }; NTASKS];
/// How many times a waiter was preempted with a borrowed timer (the diagnostics: before
/// the fix this was an eternal stop of the core without a tick).
pub static ARMED_PREEMPT: AtomicU64 = AtomicU64::new(0);
static WSTATE: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS]; // the user state word (for the demotion)
static WEXTRA: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS]; // kind2: the pair {idx_ptr, target}

/// Is the predicate of the waiter t fulfilled — by what is written in the registry.
///
/// It is needed by the waker. With an ordinary futex (kind 0) there is no question: whom to wake is decided by
/// the application, and the woken one will recheck by itself. But with our protocols (kind 1 —
/// "until the index has grown up to ITS OWN threshold", kind 2 — the door plus the index) every
/// waiter has its own threshold, and several of them may wait on ONE word.
/// To wake the first one by number means to wake the one for whom it is still too early: it will
/// recheck, lie back down, and the one whose threshold has already been passed will stay asleep
/// forever. That is exactly what was caught: in the snapshot a waiter with the mark "THE PREDICATE IS
/// FULFILLED" and woken=0, while a neighbour on the same word received wakeup after
/// wakeup.
fn waiter_ready(t: usize) -> bool {
    let a = WADDR[t].load(Ordering::SeqCst);
    if a == 0 { return true; }
    let kind = WKIND[t].load(Ordering::SeqCst) as u64;
    if kind == 0 { return true; } // futex: the addressee is the application's business
    !still_waiting(a as *const u32, WVAL[t].load(Ordering::SeqCst), kind, WEXTRA[t].load(Ordering::SeqCst))
}

/// Is the task t ready to execute (without the side counter WSKIP): it is not sleeping on a
/// word, or it has already been woken/a signal/the deadline has expired.
pub fn runnable_peek(t: usize) -> bool {
    if WADDR[t].load(Ordering::SeqCst) == 0 || WOKEN[t].load(Ordering::SeqCst) != 0 { return true; }
    if crate::proc::sig_pending_t(t) { return true; }
    let d = WDEADLINE[t].load(Ordering::SeqCst);
    if d != 0 && crate::tsc::mono_ns() >= d { return true; }
    ready_now(t)
}

/// Is the waiter's condition fulfilled RIGHT NOW.
///
/// The scheduler must know that, and it is NOT insurance but part of the contract. A waiter that
/// has given up the processor (`WSLEEP=2`) is invisible to the scheduler, and only a foreign wakeup
/// can return it into the queue. But a wakeup is lost as a matter of course, and not
/// because of an error:
///
/// * A FUTEX_WAKE(1) on a word where TWO are waiting wakes one — and the addressee is chosen
///   not by the application but by the order of the task numbers in the registry. In an MPSC queue on the word
///   `seq` of one slot both the consumer (of the data) and the producer of the next round
///   (of the release) wait; "wake one" went to the producer, while the consumer
///   stayed asleep with an already fulfilled condition. On Linux the same does not happen
///   not because the choice is cleverer there, but because there a waiter whose word
///   has changed physically cannot lie in the queue: `FUTEX_WAIT` verifies the
///   value under the bucket's lock. Our waiter verifies it in ITS OWN loop — that
///   is, only when it gets the processor.
/// * A waker that read the state word before the parking one updated it
///   is entitled not to call at all (the fast path without a barrier is the whole point of the
///   protocol).
///
/// So "the condition is fulfilled" must make the task visible BY ITSELF.
/// The insurance "once every 64 skips" could not cure that: it works only while
/// the core ticks, and a core where the only neighbour sleeps in `kwait` does not tick
/// at all. Hence the old oddity of the gate tsync: it passed the more often the
/// LONGER the slice was — with a long slice one less often managed to get into the parking window.
///
/// The word is read only when the task's address space is active on this core right
/// now — otherwise foreign memory lies at that address. The criterion is ONE PROCESS
/// with a task that is on the core right now: a process's threads share the address space,
/// and since the current one is executing, its AS is loaded. Verifying against `CR3[t] != 0` was
/// not allowed: for the tasks living in the common (identity) space — and those are all the
/// self-test modes, including the tsync gate itself — the CR3 in the table is zero, and the check
/// silently switched itself off exactly where it was being checked.
fn ready_now(t: usize) -> bool {
    let a = WADDR[t].load(Ordering::SeqCst);
    if a == 0 { return false; }
    let me = crate::sched::current_id();
    if t != me && crate::sched::proc_of(t) != crate::sched::proc_of(me) { return false; }
    let cr3 = crate::sched::task_cr3(t);
    if cr3 != 0 && cr3 != crate::paging::current_cr3() & !0xFFF { return false; }
    let kind = WKIND[t].load(Ordering::SeqCst) as u64;
    !still_waiting(a as *const u32, WVAL[t].load(Ordering::SeqCst), kind, WEXTRA[t].load(Ordering::SeqCst))
}

/// For the scheduler: the task t is sleeping on a futex word and it may be left unscheduled.
/// The insurance against any missed wakeup: once every 64 skips the task does get
/// a quantum and rechecks the word by itself (a spurious wake is allowed by the futex ABI).
/// On which word the task is sleeping (0 — it is not sleeping). It is needed by `ps`/`top`: "the thread stands still" and
/// "the thread waits for this very address" are different diagnoses, while in the fact of "not on the processor"
/// they are indistinguishable.
pub fn waiting_on(t: usize) -> u64 {
    if t >= NTASKS { return 0; }
    if WOKEN[t].load(Ordering::Relaxed) != 0 { return 0; }
    WADDR[t].load(Ordering::Relaxed)
}

pub fn blocked_for_sched(t: usize) -> bool {
    if WADDR[t].load(Ordering::SeqCst) == 0 || WOKEN[t].load(Ordering::SeqCst) != 0 { return false; }
    if crate::proc::sig_pending_t(t) { crate::sched::note_ready(t); return false; }
    let d = WDEADLINE[t].load(Ordering::SeqCst);
    if d != 0 && crate::tsc::mono_ns() >= d { crate::sched::note_ready(t); return false; }
    if ready_now(t) { crate::sched::note_ready(t); return false; } // the condition is fulfilled — the task is ready (see `ready_now`)
    // The insurance launch once every 64 skips does NOT start the count: the task is not ready.
    if WSKIP[t].fetch_add(1, Ordering::SeqCst) % 64 == 63 { return false; }
    true
}

// The diagnostics: a ring of the latest "slow" waits (wait_on_ex > 20 us) —
// who, on what word, how many us. The printing: dump_slow() (the metrics at the exit).
const SLOWN: usize = 64;
static SLOW_ADDR: [AtomicU64; SLOWN] = [const { AtomicU64::new(0) }; SLOWN];
static SLOW_INFO: [AtomicU64; SLOWN] = [const { AtomicU64::new(0) }; SLOWN]; // t<<56 | the end(us)<<20 | the duration(us)
static SLOW_IDX: AtomicU64 = AtomicU64::new(0);
static SLOW_CNT: AtomicU64 = AtomicU64::new(0);
static SLOW_PRN: AtomicU64 = AtomicU64::new(0);
fn slow_note(t: usize, addr: u64, us: u64) {
    if us <= 20 { return; }
    SLOW_CNT.fetch_add(1, Ordering::Relaxed);
    let i = (SLOW_IDX.fetch_add(1, Ordering::Relaxed) % SLOWN as u64) as usize;
    let end_us = crate::tsc::mono_ns() / 1000;
    SLOW_ADDR[i].store(addr, Ordering::Relaxed);
    SLOW_INFO[i].store((t as u64) << 56 | (end_us & 0xf_ffff_ffff) << 20 | us.min((1 << 20) - 1), Ordering::Relaxed);
}
pub fn dump_slow() {
    let n = SLOW_CNT.load(Ordering::Relaxed);
    if n == 0 { return; }
    let head = SLOW_IDX.load(Ordering::Relaxed);
    crate::println!("[slow-wait] {} waits > 20 us in total; the tape of the last {} waits (t @addr end_us duration_us):", n, SLOWN);
    for k in 0..SLOWN as u64 {
        let i = ((head + k) % SLOWN as u64) as usize;
        let a = SLOW_ADDR[i].load(Ordering::Relaxed);
        if a == 0 { continue; }
        let v = SLOW_INFO[i].load(Ordering::Relaxed);
        crate::println!("[slow-wait] t{} @{:#x} {} {}", v >> 56, a, (v >> 20) & 0xf_ffff_ffff, v & ((1 << 20) - 1));
    }
}

/// The demotion of a BSP sleeper being preempted in kwait (WSLEEP==1): the state word -> 2
/// (the producer will see it after a fence and will send a wake syscall), and if the predicate is ALREADY
/// fulfilled (the producer managed to read state=1 before the demotion and did not send the syscall) —
/// WOKEN, so that the scheduler sees the task as runnable. It is called from rust_schedule
/// BEFORE the switch (the CR3 of the task's process is still active — the user words are readable).
pub fn demote_preempted_sleeper(t: usize) {
    // RETURN THE TIMER TO THE CORE. A wait by time on an AP borrows the LVT timer for a
    // TSC-deadline (the WAKE vector — only an EOI). If the waiter was preempted between
    // "borrowed" and "returned", the core stayed WITHOUT a periodic tick forever:
    // there is no rotation on it any more, a woken task does not get the processor, and
    // after that everything stops — from the outside it looked like a lost futex
    // wakeup (the threads stand in pthread_cond_wait, there is nobody to wake them). The LVT register
    // belongs TO THE CORE, not to the task, so it is removed by whoever takes the core.
    if WARMED[t].swap(false, Ordering::SeqCst) {
        ARMED_PREEMPT.fetch_add(1, Ordering::SeqCst);
        disarm_tsc_deadline_raw();
    }
    let prev = WSLEEP[t].swap(2, Ordering::SeqCst);
    if prev != 1 && prev != 3 { return; } // 1 = an mwait on the word, 3 = an mwait on the core's doorbell
    let sp = WSTATE[t].load(Ordering::SeqCst);
    // SAFETY: W2 + W3 — the state word lives in the PROCESS's memory (the sleeper passed it to
    // `upark`), and the demotion to 2 must be seen by the producer before we go on; the LVT
    // timer this task borrowed is returned just above.
    if sp != 0 { unsafe { core::ptr::write_volatile(sp as *mut u32, 2); } }
    fence(Ordering::SeqCst);
    let a = WADDR[t].load(Ordering::SeqCst);
    if a == 0 { return; }
    // SAFETY: W1 — the word waited on is re-read to decide whether the predicate is ALREADY satisfied:
    // if it is, the task is marked runnable, otherwise a wakeup that the producer decided to
    // skip would be lost.
    let cur = unsafe { core::ptr::read_volatile(a as *const u32) };
    let val = WVAL[t].load(Ordering::SeqCst);
    // kind1: the index has grown up; kind0/2: the word has changed / the door was rung
    let sat = crate::waitpred::satisfied(crate::waitpred::kind_of(WKIND[t].load(Ordering::SeqCst) as u64), cur, val);
    if sat { WOKEN[t].store(1, Ordering::SeqCst); }
}

/// The diagnostics (a strace tick): who sleeps on what word and what is in the word right now.
pub fn dump_waiters() {
    for t in 0..NTASKS {
        let a = WADDR[t].load(Ordering::SeqCst);
        if a == 0 { continue; }
        // SAFETY: W1 — a diagnostic read of the word a task sleeps on; a task with `WADDR == 0` is
        // skipped, so the address is one a sleeper validated.
        let cur = unsafe { core::ptr::read_volatile(a as *const u32) };
        crate::println!("[futex] t{} waits @{:#x} cur={:#x} val={:#x} kind={} wsleep={} woken={} dl={} cpu={}",
            t, a, cur, WVAL[t].load(Ordering::SeqCst), WKIND[t].load(Ordering::SeqCst),
            WSLEEP[t].load(Ordering::SeqCst), WOKEN[t].load(Ordering::SeqCst),
            WDEADLINE[t].load(Ordering::SeqCst), crate::sched::cpu_of(t));
    }
}

/// The FUTEX_WAIT semantics: sleep while *addr == val (until a wake/a signal/a deadline).
/// 0 = woken/the word has changed, -11 EAGAIN, -4 EINTR, -110 ETIMEDOUT.
pub fn wait_on(addr: *const u32, val: u32, deadline: Option<u64>) -> i64 {
    wait_on_ex(addr, val, deadline, 0, 0, 0)
}

/// The same, but with a COARSE deadline: "wake me roughly then, the tick's accuracy is enough".
/// For the polling syscalls (eventfd/timerfd/signalfd, waiting for a child) —
/// they need not accuracy but invisibility to the scheduler; see `coarse` in
/// `wait_on_ex`, the price of the question is there too.
pub fn wait_on_coarse(addr: *const u32, val: u32, deadline: u64) -> i64 {
    wait_on_ex(addr, val, Some(deadline), 8, 0, 0)
}

/// Whether to keep waiting. kind 0: while *addr == val (futex); kind 1: while
/// (i32)(*addr - val) < 0 — "until the index has grown up to target" (wrap-safe;
/// the upark hysteresis: the kernel re-arms the monitor itself, without a return into user).
/// kind 2 (a doorbell): addr is "the door", we sleep while the door == val AND the index has not grown up:
/// extra -> the user pair {idx_ptr: u64, target: u32} — the partner's progress does NOT wake
/// the monitor (the door is on a separate line, the partner rings ONCE at the threshold), so
/// the predicate by the index is rechecked by the kernel after a membarrier (an anti-deadlock).
#[inline]
fn still_waiting(addr: *const u32, val: u32, kind: u64, extra: u64) -> bool {
    // SAFETY: W1 — the word waited on, read volatile: the workload may write it at any moment, and
    // that is what we are waiting for.
    let cur = unsafe { core::ptr::read_volatile(addr) };
    let k = crate::waitpred::kind_of(kind);
    // the doorbell's pair {idx_ptr: u64, target: u32} lies in the user's memory: it is read
    // here, while the predicate itself (including the wrap-safe comparison of the index) is
    // `waitpred`, tested on the host
    let idx = if k == crate::waitpred::Kind::Door && extra != 0 {
        // SAFETY: W1 — the doorbell's pair {idx_ptr, target} lies in the process's memory; `uaccess`
        // validated the `upark` argument, and a zero pointer is handled below.
        let idx_ptr = unsafe { core::ptr::read_volatile(extra as *const u64) };
        // SAFETY: W1 — as above: the partner's index.
        let target = unsafe { core::ptr::read_volatile((extra + 8) as *const u32) };
        // SAFETY: W1 — as above.
        if idx_ptr == 0 { None } else { Some((unsafe { core::ptr::read_volatile(idx_ptr as *const u32) }, target)) }
    } else { None };
    crate::waitpred::still_waiting(k, cur, val, idx)
}
#[inline]
fn set_state(state: u64, v: u32) {
    // SAFETY: W1 — the state word of the wait (`upark`'s argument, validated); 0 means the caller
    // passed none.
    if state != 0 { unsafe { core::ptr::write_volatile(state as *mut u32, v); } }
}

/// The extended wait (PLAN-SYNC SPSC v2): a kind predicate + a state word for
/// the waker: 1 = we sleep with an mwait ON THE WORD (an ordinary write wakes — a wake syscall is
/// not needed), 2 = we are parked/in hlt (the waker needs a futex wake). Before state=2
/// an asymmetric membarrier is performed (an IPI ack: the draining of the other cores' store buffers)
/// and a recheck — the waker's fast path stays without a fence.
static HANG_PRN: AtomicU64 = AtomicU64::new(0);

/// With what the waits on a futex end. Counting this became necessary because "we have thirteen
/// times more futex syscalls than Linux on the same workload" is
/// an observation rather than a diagnosis: the same number of calls will be made by an application that
/// is told "wake up" in vain every time.
/// `talaria.futexhang=<seconds>` — print "IT HAS HUNG" for a waiter that has not
/// woken up for N seconds. 0 (the default) — stay silent: for a real workload a long
/// sleep on a futex is the norm rather than an accident.
pub static HANG_SECS: AtomicU64 = AtomicU64::new(0);
pub fn configure_hang_from_cmdline(cmdline: &str) {
    if let Some(v) = crate::cmdline::value(cmdline, "talaria.futexhang") {
        if let Ok(n) = v.parse::<u64>() {
            HANG_SECS.store(n, Ordering::Relaxed);
            if n > 0 { crate::println!("talaria: futex: print \"IT HAS HUNG\" after {} s of waiting", n); }
        }
    }
}

pub static W_CALLS: AtomicU64 = AtomicU64::new(0);      // the entries into the wait in total
pub static W_EAGAIN: AtomicU64 = AtomicU64::new(0);     // the predicate is already not fulfilled at the entry
pub static W_WOKEN: AtomicU64 = AtomicU64::new(0);      // woken explicitly (FUTEX_WAKE)
pub static W_PRED: AtomicU64 = AtomicU64::new(0);       // the word changed by itself
pub static W_TIMEOUT: AtomicU64 = AtomicU64::new(0);    // the deadline expired
pub static W_SIG: AtomicU64 = AtomicU64::new(0);        // a signal
pub static W_EXIT: AtomicU64 = AtomicU64::new(0);       // the exits from the loop in total
pub static W_DEAD: AtomicU64 = AtomicU64::new(0);       // the exit "the task is dead"
/// An exact deadline FURTHER than one tick period on a ticking core: we sleep under
/// the tick, we do not borrow the LVT (`tick_serves`). Next to it — how many times we did borrow it.
pub static W_DL_TICK: AtomicU64 = AtomicU64::new(0);
pub static W_DL_BORROW: AtomicU64 = AtomicU64::new(0);
pub static W_SLEPT: AtomicU64 = AtomicU64::new(0);      // we really fell asleep at least once
// BY WHAT THE WAKEUP WAS DELIVERED (wake_addr). One number "IPIs by a hypercall" per
// machine does not say WHICH path sends them, and there are five paths, and they are cured differently.
pub static WK_STORE: AtomicU64 = AtomicU64::new(0);     // a write into the word was enough (the sleeper is in an mwait on it)
pub static WK_SAME: AtomicU64 = AtomicU64::new(0);      // its own core: a hint to the scheduler
pub static WK_RESCHED_IDLE: AtomicU64 = AtomicU64::new(0); // an IPI reschedule: the core is busy with A SLEEPING task
pub static WK_RESCHED_BUSY: AtomicU64 = AtomicU64::new(0); // an IPI reschedule: the current one has worked out the threshold
pub static WK_RESCHED_LONG: AtomicU64 = AtomicU64::new(0); // an IPI reschedule: the core is on a long tick
pub static WK_KICK: AtomicU64 = AtomicU64::new(0);      // a kick of an AP (the word did not change / a yield parking)
pub static WK_BSP: AtomicU64 = AtomicU64::new(0);       // an IPI reschedule of the BSP from an AP
pub static WK_BELL: AtomicU64 = AtomicU64::new(0);      // by a write into the core's doorbell (the sleeper is on the doorbell / the pickup loop)
/// AN EXACT DEADLINE FURTHER THAN ONE TICK PERIOD IS SERVED BY THE TICK, NOT BY BORROWING THE LVT.
///
/// Session 32, the mongodb stand (smp=2): the thread t28 "conn20" became ready on core 0
/// and waited for the processor for 216 ms (in session 31 — 1167 ms, in M35 — 844). The ring of
/// core 0's decisions during that time is EMPTY: t5 (a thread without a name) took the core by
/// a rotation out of a futex and for 1.145 s there was neither a tick nor a yield, then within 21 us the rotation
/// t5->t6->t8->t9->t13->t15->t28 — six ready threads stood in the queue behind
/// a sleeping one. And that is exactly the borrowing: an exact timeout (`parkNanos`, a condvar with time)
/// on a core without other READY ones borrowed the LVT for a TSC-deadline for THE WHOLE term, and
/// there was no periodic tick on the core for a second. Everything that during that time made a
/// task ready NOT through a futex wake (a move by the balancer from a neighbouring
/// core — `ap_ring` rings only an AP, the expired coarse deadline of a parked
/// neighbour, a signal) waited for the sleeping one's deadline.
///
/// A timeout needs accuracy only on the last stretch: while there is more than a tick period
/// until the deadline, the tick wakes the mwait every period anyway, the loop's head
/// rechecks the deadline and `*_has_other_runnable` — as with a coarse wait.
/// The borrowing stays for the last period (the JVM's `parkNanos(1 ms)`: 40 us
/// of miss instead of 340) and for a core without a tick (run-to-completion).
#[inline]
fn tick_serves(d: u64) -> bool {
    let p = crate::smp::my_period_ns();
    if p == 0 { return false; }
    if d.saturating_sub(crate::tsc::mono_ns()) > p { W_DL_TICK.fetch_add(1, Ordering::Relaxed); true }
    else { W_DL_BORROW.fetch_add(1, Ordering::Relaxed); false }
}
pub fn wait_on_ex(addr: *const u32, val: u32, deadline: Option<u64>, kind: u64, state: u64, extra: u64) -> i64 {
    // kind bit2 (|4): "the waker itself puts a fence before reading the state word" —
    // a membarrier before state=2 is not needed (MPSC: the producer's mfence is ~20 cycles
    // against 7.5 us of an IPI ack for the parking one; frequent parkings on a crowded BSP
    // arranged a membarrier storm of IPIs over the mwaits of the neighbouring APs).
    let no_mb = kind & 4 != 0;
    // kind bit3 (|8) — A COARSE deadline: "wake me roughly then, the tick's
    // accuracy suits me". Telling it from a real timeout is obligatory,
    // because the price of accuracy on a secondary core is not cycles but THE TICK OF THAT CORE:
    // the LAPIC has no "pause and continue", and returning the timer starts the
    // slice's counter from zero. A core that sleeps by time more often than once per slice never
    // reaches the end of the slice — there is no rotation on it at all.
    // A workload has a handful of real timeouts per run, and they need accuracy
    // (the JVM's `parkNanos(1ms)`: p99 30-60 us with a TSC-deadline against 500-1000 with
    // the tick's accuracy). While the polling syscalls come here thousands of times, and for them it is
    // exactly the other way round — that is why they ask for a coarse one.
    let coarse = kind & 8 != 0;
    let kind = kind & 3;
    W_CALLS.fetch_add(1, Ordering::Relaxed);
    if !still_waiting(addr, val, kind, extra) { W_EAGAIN.fetch_add(1, Ordering::Relaxed); return -11; }
    let dbg_t0 = if crate::syscall::strace_on() { crate::tsc::rdtsc() } else { 0 };
    let t = crate::sched::current_id();
    let mut cpu;
    WOKEN[t].store(0, Ordering::SeqCst);
    // We extinguish the wakeup's timestamp AT THE ENTRY: otherwise a wakeup delivered to
    // a task that left the wait by a timeout would be counted for THE NEXT
    // wait and would give seconds out of nowhere in the histogram.
    WOKE_TSC[t].store(0, Ordering::Relaxed);
    WDEADLINE[t].store(deadline.unwrap_or(0), Ordering::SeqCst);
    WSKIP[t].store(0, Ordering::SeqCst);
    WVAL[t].store(val, Ordering::SeqCst);
    WKIND[t].store(kind as u8, Ordering::SeqCst);
    WEXTRA[t].store(extra, Ordering::SeqCst);
    WSTATE[t].store(state, Ordering::SeqCst);
    WSLEEP[t].store(0, Ordering::SeqCst);
    WADDR[t].store(addr as u64, Ordering::SeqCst);
    fence(Ordering::SeqCst);
    let threaded = crate::syscall::THREADED.load(Ordering::Relaxed);
    let mut r = 0i64;
    let mut ap_deadline_ok;
    let store_wakes = mode() >= MODE_MWAIT; // in hlt mode a store does not wake — state=2
    let mut parked_state = 0u32;
    // kind2 (a doorbell): the ring of the door comes only at the threshold — the index's progress does not
    // wake the monitor, so BEFORE the first sleep a membarrier + a recheck of the predicate is
    // obligatory (otherwise the partner's final index store might not be visible — a deadlock).
    let mut door_barrier_done = false;
    let mut slept = false;
    // PARKING IS A POINT OF NO RETURN, and that is why one must announce it BEFORE
    // the last check of the predicate, and make the check AFTER a full barrier.
    //
    // A task that has given up the processor through `yield_now` is INVISIBLE to the scheduler
    // (`blocked_for_sched` skips the waiters on a futex), and only a foreign wakeup — WOKEN —
    // can return it into the queue. So if the predicate became
    // fulfilled in the window between "I decided to park" and "I announced that I am asleep",
    // while the waker at that moment read the still-old state word, then it will NEVER
    // call, and the waiter will stay in the snapshot with the mark "THE PREDICATE IS
    // FULFILLED, woken=0" — exactly what the tsync gate flaked with for years.
    //
    // Both sides must do a store-fence-load (Dekker's StoreLoad): the
    // waker — "published the value; a barrier; read the state", the parking one —
    // "announced state=2; a barrier; reread the predicate". Then with any
    // interleaving at least one sees the other. For `kind|4` (the MPSC producer
    // fences by itself, the kernel membarrier is not needed) the second half was absent
    // entirely: state=2 was written and the task went into a yield without a single recheck.
    //
    // The price is one SeqCst store and one read of the word per parking, on a path
    // that ends with a context switch anyway.
    macro_rules! park_recheck {
        ($st:expr) => {{
            WSLEEP[t].store($st, Ordering::SeqCst);
            set_state(state, $st as u32);
            fence(Ordering::SeqCst);
            if !still_waiting(addr, val, kind, extra) { continue; }
        }};
    }
    // The diagnostics of A HUNG wait: a waiter that has not woken up for seconds is
    // almost always a lost wakeup, and one needs to learn about it FROM THE LOOP
    // (such a waiter never comes out, and the post-mortem printing below stays silent).
    //
    // IT IS OFF BY DEFAULT, and that is a fix rather than a weakening. The threshold of three
    // seconds was chosen for the synchronisation gates, where a waiter must wake up
    // immediately. For a real workload a thread sleeping on a futex for longer than three seconds
    // is the norm: ClickHouse's thread pool lives exactly like that. The kernel printed the word
    // "IT HAS HUNG" about it, interleaving the operator's output with a snapshot of all the tasks — that is, it
    // reported an accident where everything was healthy, and it got in the way of reading the console exactly
    // under load. A false alarm is worse than silence: people get used to it.
    // It is switched on by `talaria.futexhang=<seconds>`.
    let hang_secs = HANG_SECS.load(Ordering::Relaxed);
    let hang_t0 = crate::tsc::rdtsc();
    let hang_ticks = crate::tsc::calibrate_cached().saturating_mul(hang_secs.max(1));
    let mut hang_reported = hang_secs == 0;
    loop {
        // THE CORE IS NOT A CONSTANT OF THE WAIT. A parked (yield) task is carried by a thief
        // to another core, and the next turn of the loop goes already there. Everything that
        // is chosen by the core — the BSP/AP branch, the core's doorbell for the sleep, "are there
        // neighbours", the borrowing of the TSC-deadline — must read the core ANEW on every
        // turn. With a sleep on a word that cost only a wrong decision about parking
        // (the word is common, after all), with a sleep on the core's doorbell it cost a sleep on A FOREIGN doorbell: our own
        // core rings, while the monitor looks at the neighbouring one.
        cpu = crate::percpu::cpu_id();
        ap_deadline_ok = cpu == 0 || TSC_DL.load(Ordering::SeqCst);
        if !hang_reported && crate::tsc::rdtsc().wrapping_sub(hang_t0) > hang_ticks
            && HANG_PRN.fetch_add(1, Ordering::Relaxed) < 24 {
            hang_reported = true;
            // SAFETY: W1 — the predicate is re-read before every sleep: between the check and the
            // arming the workload may have changed the word.
            let cur = unsafe { core::ptr::read_volatile(addr) };
            let lvt = crate::smp::lapic_peek(0x320);
            if HANG_PRN.load(Ordering::Relaxed) == 1 { dump_tasks(); } // the first one to notice prints EVERYBODY
            crate::println!("[futex] IT HAS HUNG: t{} on cpu{} has been waiting for 3 s @{:#x} val={:#x} cur={:#x} woken={} kind={} sleep={} deadline={} \
                LVT={:#x} armed-preempt={}",
                t, cpu, addr as u64, val, cur, WOKEN[t].load(Ordering::SeqCst), kind,
                WSLEEP[t].load(Ordering::SeqCst), deadline.is_some(), lvt,
                ARMED_PREEMPT.load(Ordering::SeqCst));
        }
        if !still_waiting(addr, val, kind, extra) { W_PRED.fetch_add(1, Ordering::Relaxed); note_wake_latency(t); break; }
        if WOKEN[t].load(Ordering::SeqCst) != 0 { W_WOKEN.fetch_add(1, Ordering::Relaxed); note_wake_latency(t); break; }
        if crate::proc::sig_pending() { W_SIG.fetch_add(1, Ordering::Relaxed); r = -4; break; }
        if let Some(d) = deadline { if crate::tsc::mono_ns() >= d { W_TIMEOUT.fetch_add(1, Ordering::Relaxed); r = -110; break; } }
        if !crate::sched::task_alive(t) { W_DEAD.fetch_add(1, Ordering::Relaxed); break; } // an exit_all from another core
        if kind == 2 && !door_barrier_done { door_barrier_done = true; membarrier(); continue; }
        slept = true;
        // SAFETY: W1 — as above (kwait waits for a CHANGE of the value it saw).
        let cur = unsafe { core::ptr::read_volatile(addr) }; // kwait waits for A CHANGE of the current value
        // A membarrier before state=2 is needed ONLY by the state protocol (tsync: the waker
        // skips the wake syscall by looking at the state word — the StoreLoad is closed
        // by the barrier). An ordinary futex (state==0): the waker MUST do a FUTEX_WAKE,
        // and that one does a fence + a scan of the registry — the barrier here is superfluous (that was
        // 7.5 us of an IPI ack on EVERY futex WAIT with live runnables on the BSP:
        // a same-core RTT of 15.7 us, a musl condvar of 32-44 us).
        if cpu != 0 && crate::sched::cpu_has_other_runnable(cpu, t) {
            // A core OF THE COMMON POOL: there is somebody to run on it, and to sleep on in an mwait
            // holding the core means to eat somebody else's slice. We give up the processor exactly
            // in the same way as the control plane below does it. On A DEDICATED
            // core there are no neighbours by construction, and this branch is never taken —
            // run-to-completion stays untouched.
            if parked_state != 2 {
                parked_state = 2;
                if state != 0 { set_state(state, 2); if !no_mb { membarrier(); continue; } }
            }
            // Announce oneself parked and ONLY THEN recheck the predicate
            // (see the big analysis at `park_recheck`). Without that parking is a one-way
            // ticket: the scheduler does not see a parked one, while the waker has already
            // read the old state word and will not call.
            park_recheck!(2);
            crate::sched::yield_origin(crate::sched::YO_FUTEX);
            crate::sched::yield_now();
        } else if cpu != 0 {
            // An AP: without a periodic tick. The timeout is a TSC-deadline LAPIC (the WAKE vector);
            // without it we do not sleep with a deadline (pause polling, correct but hot).
            let st = if store_wakes { 1 } else { 2 };
            if st != parked_state { parked_state = st; set_state(state, st); if st == 2 && state != 0 && !no_mb { membarrier(); continue; } }
            // Announce oneself ANEW on every round and only THEN recheck the
            // predicate. Previously the announcement stood before the check only on the first
            // round, and after that the task went to sleep without announcing itself: the partner
            // published the value, looked at the word, saw a zero (or a stale
            // two after the demotion) and did not call. The window is a couple of instructions, but
            // preemption stretches it over a slice, and the shorter the slice, the more often
            // one gets into it: at 8 ms it was not caught, at 0.5 ms it stops at once.
            // SLEEP ON THE CORE'S DOORBELL RATHER THAN ON THE WORD if there are more tasks
            // on this core. One core, two threads of a pool: the first fell asleep on its own word in an
            // mwait, the second is parked (yield). The second one could be woken
            // only by an IPI over the first one (it holds the core in an mwait on A FOREIGN line), and
            // an IPI in a guest is 5-20 us per wakeup: in the actors' ring
            // 85 % of the wakeups went along that path, and a hop cost 9 us against Linux's
            // 5.6. A monitor on the core's doorbell (`sched::doorbell`) is woken by a write
            // from `wake_addr` for ANY task of the core — without an IPI. The price: one's own
            // wakeup comes not by a write into the word but by a FUTEX_WAKE syscall,
            // which with an ordinary futex (kind 0) is obligatory by the contract; with
            // the protocols kind 1/2 the waker skips the syscall — they need the word's
            // line. A core with one task sleeps on the word, as before.
            let (bell_p, bell_v) = { let b = crate::sched::doorbell(cpu); (b.as_ptr() as *const u32, b.load(Ordering::SeqCst)) };
            let on_bell = st == 1 && kind == 0 && crate::sched::cpu_has_other_task(cpu, t);
            let (wa, wv) = if on_bell { (bell_p, bell_v) } else { (addr, cur) };
            WSLEEP[t].store(if on_bell { 3 } else { 1 }, Ordering::SeqCst);
            set_state(state, st);
            fence(Ordering::SeqCst);
            if !still_waiting(addr, val, kind, extra) { continue; }
            if on_bell && crate::sched::cpu_has_other_runnable(cpu, t) { continue; } // a neighbour was woken — give up the core
            if let Some(d) = deadline {
                // A COARSE deadline on a TICKING core does not borrow the timer (see
                // `coarse` above). There is no need to wake separately either: the periodic tick
                // will wake it itself, the deadline is rechecked in the loop's head, and from
                // the moment it expires the task stops being
                // `blocked_for_sched` — the scheduler will pick it up itself.
                // SAFETY: W2 + W3 — the row is this task's; the sleep happens on the core the monitor
                // was armed on.
                if crate::smp::timer_running() && (coarse || tick_serves(d)) { unsafe { kwait_once_wchk(wa, wv, &WOKEN[t]) } }
                // A dedicated core (run-to-completion): there is no tick by construction,
                // and without a TSC-deadline a "missed" wakeup would cost an eternity.
                // SAFETY: W3 — a borrowed TSC-deadline timer for a bounded sleep on an AP without a
                // periodic tick.
                else if ap_deadline_ok { arm_tsc_deadline(d); unsafe { kwait_once_wchk(wa, wv, &WOKEN[t]) } disarm_tsc_deadline(); }
                // SAFETY: W4 — the fallback when neither the tick nor the LVT can be used: one
                // instruction of an open window.
                else { unsafe { cpu::irq_enable_pause_disable(); } }
            // SAFETY: W2 + W3 — as above.
            } else { unsafe { kwait_once_wchk(wa, wv, &WOKEN[t]) } }
        } else if threaded {
            // If there is another runnable on the BSP — give up the CPU immediately (do not sleep on
            // until the tick holding the core in an mwait); we will come back by a wake (a HINT) or by the backstop.
            if crate::sched::bsp_has_other_runnable(t) {
                if parked_state != 2 {
                    parked_state = 2;
                    if state != 0 { set_state(state, 2); if !no_mb { membarrier(); continue; } } // recheck after the barrier
                }
                park_recheck!(2);
                crate::sched::yield_origin(crate::sched::YO_FUTEX);
                crate::sched::yield_now();
            } else {
                let st = if store_wakes { 1 } else { 2 };
                if st != parked_state { parked_state = st; if st == 2 && state != 0 && !no_mb { set_state(state, st); membarrier(); continue; } }
                // See the AP branch: announce oneself anew, then recheck the predicate.
                // See the AP branch: on a BSP with neighbours — sleep on the core's doorbell.
                let (bell_p, bell_v) = { let b = crate::sched::doorbell(0); (b.as_ptr() as *const u32, b.load(Ordering::SeqCst)) };
                let on_bell = st == 1 && kind == 0 && crate::sched::cpu_has_other_task(0, t);
                let (wa, wv) = if on_bell { (bell_p, bell_v) } else { (addr, cur) };
                WSLEEP[t].store(if on_bell { 3 } else { 1 }, Ordering::SeqCst);
                set_state(state, st); // refresh after the demotion too (it writes a 2 into the user word)
                fence(Ordering::SeqCst);
                if !still_waiting(addr, val, kind, extra) { continue; }
                if on_bell && crate::sched::bsp_has_other_runnable(t) { continue; }
                match deadline {
                    // AN EXACT TIMEOUT ON THE BSP IS ALSO BY A TSC-DEADLINE, as on an AP.
                    // Previously the BSP relied on its periodic tick, and the timeout's
                    // accuracy equalled the slice: the JVM's `parkNanos(1 ms)`
                    // overshot by 340 us instead of 40. From the outside that looked
                    // random — the numbers are bimodal — while it depended exactly on
                    // which core the parking thread ended up on.
                    // Borrowing the LVT here is safe for the same reasons as
                    // on an AP: the branch is chosen only when there are no other ready tasks on the
                    // BSP, and `lapic_timer_restore` returns THE REMAINDER of the slice to
                    // the timer.
                    Some(d) if !coarse && !tick_serves(d) => {
                        arm_tsc_deadline(d);
                        // SAFETY: W2 + W3 — the doorbell of the core: a sleeper is woken by a write
                        // into it.
                        unsafe { kwait_once_wchk(wa, wv, &WOKEN[t]) }
                        disarm_tsc_deadline();
                    }
                    // SAFETY: W2 + W3 — everybody else is asleep, so we sleep on the word itself.
                    _ => unsafe { kwait_once_wchk(wa, wv, &WOKEN[t]) }, // everybody is asleep — we sleep on the word
                }
            }
        } else {
            // SAFETY: W4 — interrupts are opened on the way out of the loop: the caller's path expects
            // them enabled.
            unsafe { cpu::irq_enable(); }
            core::hint::spin_loop(); // the old behaviour of the single-task run() mode
        }
    }
    W_EXIT.fetch_add(1, Ordering::Relaxed);
    if slept { W_SLEPT.fetch_add(1, Ordering::Relaxed); }
    set_state(state, 0);
    WSLEEP[t].store(0, Ordering::SeqCst);
    WADDR[t].store(0, Ordering::SeqCst);
    WEXTRA[t].store(0, Ordering::SeqCst);
    WDEADLINE[t].store(0, Ordering::SeqCst);
    if dbg_t0 != 0 && deadline.is_none() {
        let us = (crate::tsc::rdtsc() - dbg_t0) / (crate::tsc::calibrate_cached() / 1_000_000);
        if us > 500 && SLOW_PRN.fetch_add(1, Ordering::Relaxed) < 16 {
            // SAFETY: W1 — the last re-read of the word before the return: the caller must not be told
            // 'a timeout' when the predicate is already satisfied.
            let cur = unsafe { core::ptr::read_volatile(addr) };
            crate::println!("[slow-wait] t{} @{:#x} {} us: val={:#x} cur={:#x} woken={} kind={} r={}",
                t, addr as u64, us, val, cur, WOKEN[t].load(Ordering::SeqCst), kind, r);
        }
        slow_note(t, addr as u64, us);
    }
    r
}

/// A snapshot of ALL the tasks at the moment of a suspicion of a hang: who is on which core, who
/// waits for what and whether its predicate is already fulfilled. It is needed because "everybody hangs" and
/// "everybody hangs except one that does not get the processor" are different illnesses, while
/// by the individual wait lines they cannot be told apart.
pub fn dump_tasks() {
    let n = crate::sched::task_count();
    let online = crate::percpu::ONLINE.load(Ordering::SeqCst);
    for c in 0..crate::percpu::MAXCPU {
        if online & (1 << c) == 0 { continue; }
        let t = crate::percpu::cur_task_of(c);
        if t == u64::MAX { crate::println!("[futex] core {}: IS IDLE (the pickup loop)", c); }
        else { crate::println!("[futex] core {}: executes t{} (the hint {})", c, t, crate::sched::hint_of(c)); }
    }
    {   // Whom we rang while nobody was at home: to be verified against the waiters' addresses below. A coincidence
        // means that the wakeup came earlier than the waiter registered itself in the registry.
        let now = crate::tsc::rdtsc();
        let hz = crate::tsc::calibrate_cached().max(1);
        let mut line = false;
        for i in 0..NOBODY_N {
            let a = NOBODY_ADDR[i].load(Ordering::Relaxed);
            if a == 0 { continue; }
            if !line { crate::println!("[futex] WAKEUPS INTO THE VOID ({} in total), the latest:", NOBODY_TOTAL.load(Ordering::Relaxed)); line = true; }
            crate::println!("[futex]   @{:#x} {} ms ago", a, now.wrapping_sub(NOBODY_TSC[i].load(Ordering::Relaxed)) / (hz / 1000));
        }
    }
    crate::println!("[futex] A SNAPSHOT OF THE TASKS ({} of them; cpu/alive/waits-address/val/cur/woken/sleep/deadline):", n);
    for t in 0..n {
        let a = WADDR[t].load(Ordering::SeqCst);
        let alive = crate::sched::task_alive(t);
        if a == 0 {
            let ls = crate::syscall::LAST_SYS[t].load(Ordering::Relaxed);
            crate::println!("[futex]   t{} cpu{} {} NOT in a wait (rip={:#x}, the last syscall #{} arg0={:#x})",
                t, crate::sched::cpu_of(t), if alive { "alive" } else { "dead" }, crate::sched::saved_rip(t),
                if ls == u64::MAX { 9999 } else { ls >> 48 }, crate::syscall::LAST_ARG0[t].load(Ordering::Relaxed));
            continue;
        }
        // SAFETY: W1 — a diagnostic read of the word a task sleeps on (see `dump_waiters`).
        let cur = unsafe { core::ptr::read_volatile(a as *const u32) };
        let val = WVAL[t].load(Ordering::SeqCst);
        let (taken, pend) = crate::sched::sched_flags(t);
        crate::println!("[futex]   t{} cpu{} {} @{:#x} val={:#x} cur={:#x} kind={} rn={} blk={} woken={} sleep={} dl={} taken={} pend={} {}",
            t, crate::sched::cpu_of(t), if alive { "alive" } else { "dead" }, a, val, cur,
            // Calling `blocked_for_sched` here is NOT ALLOWED: it spins the skip counter
            // WSKIP, while a snapshot must only look. We print the pure answer.
            WKIND[t].load(Ordering::SeqCst), ready_now(t) as u8, !runnable_peek(t) as u8,
            WOKEN[t].load(Ordering::SeqCst), WSLEEP[t].load(Ordering::SeqCst),
            WDEADLINE[t].load(Ordering::SeqCst) != 0, taken as u8, pend as u8,
            // The verdict is by the protocol's REAL predicate rather than by "the word
            // has changed": with kind 1 (the index has grown up) an equality means FULFILLED, and
            // the line "cur == val" read as "it waits lawfully", hiding exactly the
            // case for whose sake the snapshot is printed.
            if !still_waiting(a as *const u32, val, WKIND[t].load(Ordering::SeqCst) as u64, WEXTRA[t].load(Ordering::SeqCst))
                { "<-- THE PREDICATE IS FULFILLED (the wakeup was lost)" } else { "" });
    }
}

/// An asymmetric membarrier (PLAN-SYNC §7): an IPI to all the other online cores, a
/// wait for the ack (the iret on the target serialises — its store buffer is drained). After
/// that a recheck of the word closes the StoreLoad race "the waker's store / the reading of the
/// state" WITHOUT a fence on the waker's fast path. Only the sleeper's slow path.
pub fn membarrier() {
    let me = crate::percpu::cpu_id();
    let online = crate::percpu::ONLINE.load(Ordering::SeqCst);
    let mut expect = 0u64;
    let t0 = WAKE_ACKS.load(Ordering::SeqCst);
    for c in 0..crate::percpu::MAXCPU {
        if c == me || online & (1 << c) == 0 { continue; }
        kick(c as u32);
        expect += 1;
    }
    if expect == 0 { fence(Ordering::SeqCst); return; }
    let start = crate::tsc::rdtsc();
    let hz = crate::tsc::calibrate_cached();
    while WAKE_ACKS.load(Ordering::SeqCst).wrapping_sub(t0) < expect {
        crate::paging::tlb_service(); // a neighbour in munmap waits for our acknowledgement (see ext2::fs_lock)
        core::hint::spin_loop();
        if crate::tsc::rdtsc().wrapping_sub(start) > hz / 1000 { break; } // a 1 ms insurance
    }
    fence(Ordering::SeqCst);
}
/// The counter of acks from the WAKE vector (the increment is in wake_stub, a lock inc).
#[unsafe(no_mangle)]
pub static WAKE_ACKS: AtomicU64 = AtomicU64::new(0);

/// Wake up to maxn tasks of the process leader that sleep on addr. It returns the number.
/// allow_switch: a BSP waker gives the remainder of its slice to the first same-core waiter
/// (a directed switch — a futex wake "into the hands", past the scheduler's rotation).
/// A ring of the latest wakeups that had NOBODY to wake (the address + a TSC mark).
///
/// A hang of the kind "everybody sleeps" differs from "everybody sleeps, but somebody was forgotten"
/// in one thing only: did a wake come to this address BEFORE the waiter registered itself in the
/// registry. The check of the predicate at the entry into the wait and the registration in the registry are not one
/// action (with Linux they are under the bucket's lock), and that is the only known window
/// through which a wakeup can go into the void. By itself it is harmless:
/// the wait loop rereads the word already AFTER the registration. But if a remainder
/// ever turns out to be here, the proof must lie ready.
const NOBODY_N: usize = 16;
static NOBODY_ADDR: [AtomicU64; NOBODY_N] = [const { AtomicU64::new(0) }; NOBODY_N];
static NOBODY_TSC: [AtomicU64; NOBODY_N] = [const { AtomicU64::new(0) }; NOBODY_N];
static NOBODY_I: AtomicU64 = AtomicU64::new(0);
/// The wakeups into the void in total (normally there are MANY of them: having nobody to wake is an ordinary thing).
pub static NOBODY_TOTAL: AtomicU64 = AtomicU64::new(0);

/// How much the current task must already have worked for a woken one to preempt it.
/// Not a share of the slice but a number: see the analysis in `wake_addr`.
pub static WAKE_PREEMPT_NS: AtomicU64 = AtomicU64::new(150_000);

/// `talaria.wakepreempt=<ns>` — after how long a woken thread is entitled to preempt
/// a computing one. A knob rather than a constant: this is the DIRECT price of the wakeup's tail, and
/// different workloads pay it differently. A woken one waits exactly up to this
/// threshold, so it is visible in the percentiles as a plateau (with mongod the stage
/// "the socket->the reader" has p50 = 1.4 us with p99 = 182 us — the median is fine, the tail
/// runs into waiting for the processor rather than into the network).
pub fn wakepreempt_from_cmdline(cmdline: &str) {
    if let Some(ns) = crate::cmdline::num(cmdline, "talaria.wakepreempt") {
        WAKE_PREEMPT_NS.store(ns, Ordering::SeqCst);
        crate::println!("sync: a woken one preempts a computing one after {} ns on the processor", ns);
    }
}

/// WHEN THE TASK WAS WOKEN (in cycles) — in order to measure how much it waited for
/// THE PROCESSOR after that.
///
/// Why a separate measurement. "The waiter waited 5 ms" says nothing: the wait could have
/// been lawful (the predicate is not fulfilled). The question is always a different one — how much time passed
/// between "it became ready" and "it ran". That is exactly what separates our latency from
/// somebody else's logic, and exactly what was missing in order to judge the scheduler's changes:
/// three of them were made and reverted blindly.
static WOKE_TSC: [core::sync::atomic::AtomicU64; NTASKS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; NTASKS];
/// The histogram of the latency "woken -> ran": <5, <20, <50, <100, <200,
/// <500, <1000 us and everything longer.
pub static WAKE_LAT: [core::sync::atomic::AtomicU64; 8] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 8];
pub static WAKE_LAT_SUM: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static WAKE_LAT_MAX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
fn note_wake_latency(t: usize) {
    let t0 = WOKE_TSC[t].swap(0, Ordering::Relaxed);
    if t0 == 0 { return; }
    let hz = crate::tsc::calibrate_cached().max(1);
    let us = crate::tsc::rdtsc().wrapping_sub(t0).saturating_mul(1_000_000) / hz;
    WAKE_LAT_SUM.fetch_add(us, Ordering::Relaxed);
    if us > WAKE_LAT_MAX.load(Ordering::Relaxed) { WAKE_LAT_MAX.store(us, Ordering::Relaxed); }
    let b = match us { 0..=4 => 0, 5..=19 => 1, 20..=49 => 2, 50..=99 => 3,
                       100..=199 => 4, 200..=499 => 5, 500..=999 => 6, _ => 7 };
    WAKE_LAT[b].fetch_add(1, Ordering::Relaxed);
}

/// Wake the waiters on A KERNEL word — without a selection by process.
///
/// The selection by leader is obligatory with an ordinary futex: the same VIRTUAL address in
/// different address spaces means different objects. For a word living in the
/// kernel (a pipe, a queue) it is the other way round: the address is unique per machine, while the waiters may
/// be from DIFFERENT processes (the classic `sh: a | b`), and a selection by leader
/// would leave them asleep.
pub fn wake_addr_kernel(addr: u64, maxn: usize) -> usize { wake_impl(addr, maxn, usize::MAX, false) }

pub fn wake_addr(addr: u64, maxn: usize, leader: usize, allow_switch: bool) -> usize {
    wake_impl(addr, maxn, leader, allow_switch)
}

fn wake_impl(addr: u64, maxn: usize, leader: usize, allow_switch: bool) -> usize {
    if maxn == 0 { return 0; }
    fence(Ordering::SeqCst); // the waker's store(word) is drained before the reading of the registry
    let my_cpu = crate::percpu::cpu_id();
    let mut n = 0usize;
    let mut hint: Option<usize> = None;
    for t in 0..NTASKS {
        if n >= maxn { break; }
        if WADDR[t].load(Ordering::SeqCst) != addr { continue; }
        if leader != usize::MAX && crate::sched::proc_of(t) != leader { continue; }
        // We skip AN ALREADY WOKEN one. It is still listed in the registry (WADDR is removed
        // only when the task really gets the processor and leaves the wait), and
        // without this check EVERY next FUTEX_WAKE(1) gave the wakeup to
        // one and the same — the first by number — waiter, while the real addressee did not
        // wake up. ClickHouse's thread pool grew to the task limit on this and fell by
        // an abort, while the JVM benchmark stalled: ONE must be woken, but a new one.
        if WOKEN[t].load(Ordering::SeqCst) != 0 { continue; }
        // With the protocols with a threshold (kind 1/2) we wake only the one whose threshold has already been
        // passed: otherwise the wakeup goes to a neighbour on the word for whom it is still
        // too early, while the ready one stays asleep (see `waiter_ready`).
        if !waiter_ready(t) { continue; }
        WOKEN[t].store(1, Ordering::SeqCst);
        WOKE_TSC[t].store(crate::tsc::rdtsc(), Ordering::Relaxed);
        crate::sched::note_ready(t);                 // "ready and not running" — from this cycle on (`sched::READY_TSC`)
        // Where to seat it is decided HERE: a pool wakes threads rather than creating them, and
        // the layout at the creation could not know about that (`affinity::wake_target`).
        if let Some(dst) = crate::affinity::wake_target(t, crate::sched::cpu_of(t)) {
            crate::sched::migrate_idle_task(t, dst);
        }
        // The limit `maxn` is about an ordinary futex, where "wake one" is decided by the
        // application. With our protocols (kind 1/2) several may wait on one word,
        // each with its own threshold, and a wakeup that went to the wrong one
        // IS LOST: the woken one sees that it is still too early and lies back down, while the
        // ready one keeps sleeping. So we wake ALL the ready ones — that cannot be
        // superfluous: every one of them must leave the wait anyway.
        if WKIND[t].load(Ordering::SeqCst) == 0 { n += 1; }
        let c = crate::sched::cpu_of(t);
        // The mwait mode + a sleeper in kwait ON THE WORD (WSLEEP==1) + kind0 + the word has already
        // changed: the waker's store woke the monitor itself (or the recheck before the mwait
        // will see it) — an IPI is not needed. An IPI to a vCPU in a native mwait (cpu-pm=on) is
        // a chain of vmexits of ~5-10 us on both ends (it strangled the musl condvar).
        // WSLEEP==2 (a yield parking of the BSP) or the word did not change (a wake without a write,
        // allowed by the futex ABI) — an IPI is obligatory. The race 1->2: a waker with SeqCst sees
        // a 2 -> it sends an IPI; it saw a stale 1 -> the waiter has already passed the WOKEN check, its
        // yield will return it by a rotation (WOKEN removes blocked_for_sched) within <= 1 round.
        let store_woke = mode() >= MODE_MWAIT
            && WSLEEP[t].load(Ordering::SeqCst) == 1
            && WKIND[t].load(Ordering::SeqCst) == 0
            // SAFETY: W1 — the word of the task being woken is read to decide whether the store has
            // already woken it (then the IPI is skipped).
            && unsafe { core::ptr::read_volatile(addr as *const u32) } != WVAL[t].load(Ordering::SeqCst);
        if c == my_cpu {
            WK_SAME.fetch_add(1, Ordering::Relaxed);
            if hint.is_none() { hint = Some(t); }
            // On an AP (and on the BSP when a directed switch is not allowed) we do not switch
            // ourselves — we leave a hint to the scheduler of OUR OWN core,
            // otherwise the woken task waits for its turn in the common rotation.
            if my_cpu != 0 || !allow_switch { crate::sched::hint_set_on(my_cpu, t); }
        }
        else if c != 0 {
            // The core is busy with ANOTHER task: one "wake up" is not enough — it will return into
            // the same task, while the woken one will wait for its turn until the tick
            // (500 us). We ask that core's scheduler to switch to it at once:
            // that is exactly a resched IPI. ClickHouse's pipeline (the pool's threads wait for tasks
            // from each other) paid three times as much for such a latency.
            let on_cpu = crate::percpu::cur_task_of(c);
            // We poke that core's scheduler ONLY if it is busy right now with a
            // task that is itself asleep (that is, the core idles in vain).
            // If real work is going on there — we do not interfere: preempting a
            // computing thread for the sake of a woken one means exchanging one latency for
            // another (on a ping-pong that is visible at once).
            let idle_ish = on_cpu != u64::MAX && (on_cpu as usize) < NTASKS
                && blocked_for_sched(on_cpu as usize);
            // PREEMPTION AT A WAKEUP. Previously a core busy with real work
            // was not touched at all — and the woken thread waited for A TICK, that is, up to a whole
            // slice. For a pipeline where the threads wake each other several times
            // per request that is the whole gap: a `LIKE` filter over a table took
            // 9.5 ms against Linux's 7, and with a slice of 150 us — 8, that is, the time
            // ran exactly into the quantum's length rather than into the computation.
            //
            // But preempting every computing thread for the sake of a woken one is not allowed either: that
            // exchanges one latency for another, and on a ping-pong it is visible at once. Hence
            // the threshold: we preempt only if the current one has ALREADY worked `WAKE_PREEMPT_NS`.
            //
            // The threshold is set BY A NUMBER rather than by a share of the slice, and that is a matter of principle. A share of the
            // slice means "the wakeup's latency is the worse the longer the quantum is"
            // — that is, exactly the connection we want to break: the slice
            // is chosen by throughput, while how long a woken thread has to wait
            // is a question of latency. And the measurement gave two different answers: the slice
            // is more profitable to keep at 500 us, while the `LIKE` filter over a table comes out
            // at Linux's level (7-8 ms against 9.5-10) only when one has to wait
            // no longer than ~150 us; half a slice (250 us) was already not enough.
            let thr = WAKE_PREEMPT_NS.load(Ordering::Relaxed).min(crate::smp::effective_slice_ns() / 2);
            let busy_long = !idle_ish && crate::sched::ran_ns_on(c) >= thr;
            // A CORE ON A LONG TICK CANNOT BE WAITED FOR. It thinned its tick exactly
            // because there was nobody to preempt there — while we are right now
            // making a task ready. Without a call it would wait until the next
            // long tick (milliseconds), that is, the thinning of the tick would be paid for
            // by the wakeup's latency. The call at the same time reconfigures the period:
            // an entry into the scheduler by this vector is an ordinary hardware tick.
            let long = crate::smp::long_tick(c);
            // THE CORE SLEEPS ON ITS OWN DOORBELL (the current task is in an mwait on it, WSLEEP=3,
            // or this is the pickup loop itself): we wake it BY A WRITE, without an IPI. The woken one —
            // the current one or its parked neighbour — is taken from the hint.
            let bell_sleeper = mode() >= MODE_MWAIT && (on_cpu == u64::MAX
                || ((on_cpu as usize) < NTASKS && WSLEEP[on_cpu as usize].load(Ordering::SeqCst) == 3));
            if bell_sleeper {
                WK_BELL.fetch_add(1, Ordering::Relaxed);
                if on_cpu != t as u64 { crate::sched::hint_set_on(c, t); }
                crate::sched::ap_ring(c);
            } else if on_cpu != t as u64 && (idle_ish || busy_long || long) {
                (if idle_ish { &WK_RESCHED_IDLE } else if busy_long { &WK_RESCHED_BUSY } else { &WK_RESCHED_LONG }).fetch_add(1, Ordering::Relaxed);
                crate::sched::hint_set_on(c, t);
                crate::smp::send_ipi(crate::smp::apic_of(c), crate::idt::TIMER_VECTOR as u8);
            } else if !store_woke { WK_KICK.fetch_add(1, Ordering::Relaxed); kick(c as u32); } // it sleeps on the word itself — a write/a call is enough
            else { WK_STORE.fetch_add(1, Ordering::Relaxed); }
        }
        else if !store_woke { // a BSP sleeper, we wake it from an AP
            let on_cpu = crate::percpu::cur_task_of(0);
            let bell_sleeper = mode() >= MODE_MWAIT && (on_cpu as usize) < NTASKS
                && WSLEEP[on_cpu as usize].load(Ordering::SeqCst) == 3;
            crate::sched::hint_set_on(0, t);
            if bell_sleeper { // the BSP sleeps on its own doorbell — a write instead of an IPI (see the AP branch)
                WK_BELL.fetch_add(1, Ordering::Relaxed);
                crate::sched::ap_ring(0);
            } else { // an IPI reschedule with a hint (an immediate switch)
                WK_BSP.fetch_add(1, Ordering::Relaxed);
                crate::smp::send_ipi(crate::smp::apic_of(0), crate::idt::TIMER_VECTOR as u8);
            }
        }
        else { WK_STORE.fetch_add(1, Ordering::Relaxed); }
    }
    if allow_switch && my_cpu == 0 {
        if let Some(h) = hint { crate::sched::yield_origin(crate::sched::YO_WAKE); crate::sched::yield_to(h); }
    }
    if n == 0 {
        NOBODY_TOTAL.fetch_add(1, Ordering::Relaxed);
        let i = (NOBODY_I.fetch_add(1, Ordering::Relaxed) as usize) % NOBODY_N;
        NOBODY_ADDR[i].store(addr, Ordering::Relaxed);
        NOBODY_TSC[i].store(crate::tsc::rdtsc(), Ordering::Relaxed);
    }
    n
}

// The TSC-deadline LAPIC timer (LVT timer mode 10b, the WAKE vector — only an EOI).
const IA32_TSC_DEADLINE: u32 = 0x6E0;
fn arm_tsc_deadline(deadline_mono_ns: u64) {
    WARMED[crate::sched::current_id()].store(true, Ordering::SeqCst);
    // Remember THE REMAINDER of the slice before we reprogram the LVT: returning
    // the full slice to the timer means cancelling the preemption on this core (see
    // `smp::timer_borrow`).
    crate::smp::timer_borrow();
    {   static ONCE: AtomicBool = AtomicBool::new(false);
        if !ONCE.swap(true, Ordering::Relaxed) {
            crate::println!("sync: the wait tier has taken the LVT timer for a TSC-deadline (the first time)");
        }
    }
    let now = crate::tsc::mono_ns();
    let hz = crate::tsc::calibrate_cached();
    let dt_tsc = deadline_mono_ns.saturating_sub(now).saturating_mul(hz) / 1_000_000_000;
    let target = crate::tsc::rdtsc().saturating_add(dt_tsc.max(1));
    // SAFETY: W3 — the LAPIC and IA32_TSC_DEADLINE of THIS core: the timer is borrowed for the
    // duration of one bounded sleep.
    unsafe {
        crate::smp::lapic_poke(0x320, crate::idt::WAKE_VECTOR as u32 | 0x4_0000); // LVT: TSC-deadline
        cpu::mfence(); // SDM: order the LVT before the wrmsr
        cpu::wrmsr(IA32_TSC_DEADLINE, target);
    }
}
fn disarm_tsc_deadline() {
    WARMED[crate::sched::current_id()].store(false, Ordering::SeqCst);
    disarm_tsc_deadline_raw();
}
/// Remove the TSC-deadline and return the LVT to the scheduler. Separately from `disarm_tsc_deadline`,
/// because it is called ON BEHALF of a task too — from the scheduler, when it is being preempted.
fn disarm_tsc_deadline_raw() {
    // SAFETY: W3 — the borrowed timer is returned to the CORE; it is called both by the sleeper and by
    // whoever takes the core from a preempted one.
    unsafe {
        cpu::wrmsr(IA32_TSC_DEADLINE, 0);
    }
    // A12: the LVT timer is a SHARED register, and we borrowed it from the scheduler. Previously
    // there was a masking here, that is, the scheduler's timer died forever after the very first
    // wait by time (on metal that cost the production mode:
    // there is no preemption -> a thread waits for its partners, whom nobody gives a core).
    crate::smp::lapic_timer_restore();
}

// ── A ring0 self-check of a cross-core wakeup (talaria.test=sync) ────────────
// The BSP writes into a word, an AP sleeps on it (kwait_once); a measurement of the min/p50/p99 of the cycle
// from the write to the wakeup. The protocol goes through cache-line-aligned statics.

#[repr(align(64))]
struct Line(AtomicU32);
#[repr(align(64))]
struct Line64(AtomicU64);
static WORD: Line = Line(AtomicU32::new(0));
static ACK: Line64 = Line64(AtomicU64::new(0));

/// The AP's loop in self-test mode (from ap_entry with AP_MODE=1): sleep on WORD,
/// on the wakeup mark an rdtsc in ACK.
pub fn ap_selftest_loop() -> ! {
    loop {
        let cur = WORD.0.load(Ordering::SeqCst);
        // SAFETY: W4 — the self-check of the AP path: without MONITOR the core waits in `hlt` for an
        // interrupt.
        if cur == u32::MAX { loop { unsafe { cpu::irq_enable_halt(); } } }
        while WORD.0.load(Ordering::SeqCst) == cur {
            // SAFETY: W1 + W3 — the self-check sleeps on a kernel word of its own.
            unsafe { kwait_once(WORD.0.as_ptr() as *const u32, cur); }
        }
        ACK.0.store(crate::tsc::rdtsc(), Ordering::SeqCst);
    }
}

/// The BSP's side of the self-test: N rounds of a write + a wait for the ACK. It prints the statistics.
pub fn selftest_bsp(n: usize) {
    let hz = crate::tsc::calibrate_cached();
    let to_ns = |c: u64| c.saturating_mul(1_000_000_000) / hz;
    let mut lat = [0u64; 2000];
    let n = n.min(lat.len());
    let m = mode();
    // the warm-up: let the AP reach the sleep
    for _ in 0..200_000 { core::hint::spin_loop(); }
    for i in 0..n {
        ACK.0.store(0, Ordering::SeqCst);
        for _ in 0..50_000 { core::hint::spin_loop(); } // the AP manages to fall asleep (mwait/hlt)
        let t0 = crate::tsc::rdtsc();
        WORD.0.fetch_add(1, Ordering::SeqCst); // a write = a wakeup (mwait)
        if m < MODE_MWAIT { kick(1); }         // the hlt mode: an IPI is needed
        let mut spins = 0u64;
        while ACK.0.load(Ordering::SeqCst) == 0 {
            core::hint::spin_loop();
            spins += 1;
            if spins > 2_000_000_000 { crate::println!("SYNC: FAIL — the AP did not wake up (round {})", i); return; }
        }
        lat[i] = ACK.0.load(Ordering::SeqCst).saturating_sub(t0);
    }
    // the measurement of the asymmetric membarrier (an IPI ack; the AP is still alive in the self-test loop)
    let mut mb = [0u64; 200];
    for v in mb.iter_mut() {
        let t0 = crate::tsc::rdtsc();
        membarrier();
        *v = crate::tsc::rdtsc() - t0;
    }
    mb.sort_unstable();
    WORD.0.store(u32::MAX, Ordering::SeqCst); // dismiss the AP
    if mode() < MODE_MWAIT { kick(1); }
    lat[..n].sort_unstable();
    crate::println!("SYNC: the mode {} a cross-core wakeup: min={} ns p50={} ns p99={} ns (N={}) -> OK",
        mode_name(m), to_ns(lat[0]), to_ns(lat[n / 2]), to_ns(lat[n * 99 / 100]), n);
    crate::println!("SYNC-MB: an asymmetric membarrier (an IPI ack, 1 AP): p50={} ns p99={} ns (N=200) -> OK",
        to_ns(mb[100]), to_ns(mb[198]));
}
