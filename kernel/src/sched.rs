//! The preemptive task scheduler. A core (CPU) is in one of two states,
//! and the difference between them is the whole point of this OS:
//!
//!   * **dedicated** — there is exactly one task on the core, the LAPIC timer is NOT armed,
//!     run-to-completion: the thread owns the core until it leaves, the waits happen in the kernel
//!     (mwait). Zero preemptions, zero jitter from us. That is how the cores live that were
//!     handed out by `pin=...:N` / `pin=...:dedicated` (see `affinity.rs`), and that is how —
//!     for the sake of backward compatibility — EVERY AP lives until a layout is set;
//!   * **of the common pool** — a round robin by the LAPIC timer's tick between all the tasks
//!     of that core. The BSP (logical core 0) is always in the pool: it is the control
//!     plane. The other cores get into the pool when the manifest has allowed the unpinned
//!     threads to spread out (`spread=on`).
//!
//! The scheduler is executed on EVERY core of the pool (`rust_schedule` looks at
//! `percpu::cpu_id()`), every core has its own queue: a task belongs to the core
//! `CPU_OF[t]`, and foreign ones do not choose it. The balancing is in `affinity.rs`: it
//! moves the `CPU_OF` of a task that is NOT executing right now, so a
//! migration costs one write and a ring of the target core's doorbell.
//! # Safety
//!
//! This module switches CONTEXTS: it writes the stack pointer a core will return
//! on, the CR3 an address will mean something under, and the register frame a
//! thread resumes from. A mistake here does not corrupt an object — it resumes the
//! wrong thread on the wrong stack, and the dump shows a call chain that never
//! happened. The `unsafe` rests on four invariants, named C1..C4 at the sites.
//!
//! - **C1. A `static mut` here is INDEXED BY TASK, and a row is touched by the core
//!   that owns the task.** `RSP`/`KSTACK`/`FSBASE`/`GONE`/... are arrays of
//!   `NTASKS`; a task belongs to the core `CPU_OF[t]`, and a foreign core does not
//!   choose it. What crosses cores is atomic (the occupancy marks `AP_TAKEN`,
//!   `CURRENT`, the doorbells) — that is the whole protocol by which a steal and a
//!   migration are safe, and the long comments at those places record what a plain
//!   store cost.
//! - **C2. A context is entered ONLY when the task is claimed.** Before a switch
//!   the core marks the task as its own (a CAS on `AP_TAKEN`, and for the BSP the
//!   `CURRENT` field); `enter_ctx`/`ctx_enter` then abandon this stack for good.
//!   Entering an unclaimed context means two cores running one thread.
//! - **C3. The stack and the frame are the task's own.** A kernel stack is
//!   allocated once per slot (`alloc_kstack`) and freed only after the task's
//!   sisters have stopped (`reap_dead_kstacks`, `drain_proc`); a user frame written
//!   by a tick belongs to the thread that was interrupted. `-> !` functions do not
//!   return: the stack under them ceases to exist.
//! - **C4. `asm!` and the MSRs are THIS core's machine state** (the FPU area, the
//!   FS base, the LAPIC). The FPU area is per task and 64-byte aligned, as
//!   `fxsave`/`xsave` require.
//! The crate switches `unsafe_op_in_unsafe_fn` off (`main.rs`) and modules switch it back
//! on as they are documented. It stays OFF for this one on purpose: the twenty `unsafe fn`
//! here are long (`spawn_*`, `start`, `migrate_current_to`), their bodies are dangerous from
//! the first line to the last, and wrapping each of them in one `unsafe {}` would indent
//! hundreds of lines to say what the signature already says. The invariants above and the
//! line at every site are what a reviewer needs; `sync.rs` — where the operations are few
//! and their ORDER is the contract — has the lint on.
use core::sync::atomic::{AtomicUsize, AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

pub const NTASKS: usize = 192; // it was 64: ClickHouse's thread pool ran into the limit and the process fell by an abort
pub const MAXCPU: usize = crate::percpu::MAXCPU;
static mut RSP: [u64; NTASKS] = [0; NTASKS];
static mut KSTACK: [u64; NTASKS] = [0; NTASKS];   // rsp0 per thread (0 = a ring0 task)
/// The task has left FOR GOOD (`exit_current_code`): it is not scheduled any more under
/// any conditions. It differs from `DEAD`: a dead thread preempted IN THE MIDDLE
/// OF THE KERNEL is run to the boundary with user space (see `dying`).
static mut GONE: [bool; NTASKS] = [false; NTASKS];
/// A thread's user registers at the moment when it stopped on the way out
/// of a syscall while dead (rip, rsp, rbp, rbx, r12, r13, r14, r15, rflags, rax).
/// They are needed by a core dump: such a thread's saved context is the exit path in the kernel.
static mut EXITREG: [[u64; 10]; NTASKS] = [[0; 10]; NTASKS];
static mut EXITREG_OK: [bool; NTASKS] = [false; NTASKS];
static mut FSBASE: [u64; NTASKS] = [0; NTASKS];   // FS_BASE (TLS) per thread
static mut DEAD: [bool; NTASKS] = [false; NTASKS]; // the thread has finished
static mut CTID: [u64; NTASKS] = [0; NTASKS];      // the address to clear at the exit (join)
static mut CR3: [u64; NTASKS] = [0; NTASKS];       // the task's address space (0 = the common one, threads)
static mut HEAP_B: [u64; NTASKS] = [0; NTASKS];    // the process's private heap: the base (0 = the kernel's common heap)
static mut HEAP_BR: [u64; NTASKS] = [0; NTASKS];   // the current brk
static mut HEAP_TP: [u64; NTASKS] = [0; NTASKS];   // the top of the heap
static mut EXITCODE: [i32; NTASKS] = [0; NTASKS];  // the task's exit code (for wait4)
static mut REAPED: [bool; NTASKS] = [false; NTASKS]; // the task has already been returned in wait4
static mut PROC: [u8; NTASKS] = [0; NTASKS];       // the leader of the task's process (the threads share the leader)
/// A deferred substitution of the user rsp at the return from a syscall (the delivery of a signal /
/// rt_sigreturn). It lives IN THE TASK rather than in a per-CPU area: previously the value
/// lay only in gs:16, and if the task was preempted between "it put it there" and "the stub
/// read it", it was taken by THE NEXT task of that core — it returned into ring 3
/// with A FOREIGN stack pointer, while the original one returned with a pointer to its own signal frame.
/// Under SMP and with frequent signals (the JVM catches a SIGSEGV at every safepoint poll)
/// that looked like random memory corruption: crashes of the garbage collector during the walk over
/// the roots and a "stack overflow" of a thread creeping down by 1.9 KB per signal.
static mut SIGRSP: [u64; NTASKS] = [0; NTASKS];
/// # Safety
/// C1: both indices are task slots; the row saved is `cur`'s and the one restored is
/// `next`'s, so a wrong index gives a thread somebody else's signal stack — which is
/// what the comment above records happening.
#[inline]
unsafe fn sigrsp_switch(cur: usize, next: usize) {
    SIGRSP[cur] = crate::percpu::sig_newrsp();
    crate::percpu::set_sig_newrsp(SIGRSP[next]);
}
#[repr(C, align(64))]
struct FpuArea([u8; FPU_AREA]);
/// The area of a task's FPU state. THE SIZE AND THE ALIGNMENT are for XSAVE: 512 bytes
/// of the legacy area + 64 bytes of the header + 256 bytes of YMM_Hi128 = 832; we take 1024 with
/// some headroom, an alignment of 64 is obligatory (an XSAVE/XRSTOR requirement).
///
/// WHY NOT fxsave. The kernel enables AVX for the user (XCR0=0x7), while `fxsave`
/// saves only the low 128 bits of the XMM registers. The upper halves of YMM were NOT saved at a
/// context switch: a task preempted in the middle of
/// AVX code got A NEIGHBOUR's upper halves back. For the JVM's JIT code (it
/// uses AVX in array copying and string operations) that is a silent
/// memory corruption — and it was caught as random crashes of the garbage collector.
const FPU_AREA: usize = 1024;
static mut FPU: [FpuArea; NTASKS] = [const { FpuArea([0; FPU_AREA]) }; NTASKS];
/// The XCR0 mask for XSAVE/XRSTOR (0 — XSAVE is unavailable, we work with fxsave).
pub static XSAVE_MASK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Save the current core's FPU state into the task's area.
///
/// # Safety
/// C3 + C4: `area` must be the CURRENT task's FPU area — at least the size the active
/// `XSAVE_MASK` requires and 64-byte aligned, as `fxsave`/`xsave` demand. A wrong area
/// silently mixes two threads' floating point.
#[inline]
pub unsafe fn fpu_save(area: *mut u8) {
    let m = XSAVE_MASK.load(Ordering::Relaxed);
    if m != 0 {
        core::arch::asm!("xsave [{a}]", a = in(reg) area, in("eax") m as u32, in("edx") (m >> 32) as u32, options(nostack));
    } else {
        core::arch::asm!("fxsave [{}]", in(reg) area, options(nostack));
    }
}
/// Restore the FPU state from the task's area.
///
/// # Safety
/// C3 + C4, as `fpu_save`, and the area must hold a state SAVED by it (a zeroed area
/// restores a legal but foreign state).
#[inline]
pub unsafe fn fpu_restore(area: *const u8) {
    let m = XSAVE_MASK.load(Ordering::Relaxed);
    if m != 0 {
        core::arch::asm!("xrstor [{a}]", a = in(reg) area, in("eax") m as u32, in("edx") (m >> 32) as u32, options(nostack));
    } else {
        core::arch::asm!("fxrstor [{}]", in(reg) area, options(nostack));
    }
}
static mut FPU_VALID: [bool; NTASKS] = [false; NTASKS];

// The address spaces of finished processes: they are freed by the scheduler when there is
// another task on the CPU (the tables must not be torn down under the active CR3 / on the dying one's kernel stack).
static mut DEAD_AS: [u64; NTASKS] = [0; NTASKS];
/// talaria.stop=SECS: at that moment (mono ns) send a SIGTERM to the leader process (pid 1) — a clean stop of the workload.
pub static STOP_AT_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static STOP_SENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Put a process's AS into the queue for freeing (all its tasks are already DEAD).
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn queue_dead_space(cr3: u64) { if cr3 == 0 { return; } unsafe { for i in 0..NTASKS { if DEAD_AS[i] == cr3 { return; } } for i in 0..NTASKS { if DEAD_AS[i] == 0 { DEAD_AS[i] = cr3; return; } } } }
/// The CR3 loaded on an AP — as the core itself publishes it. The second value is
/// the former CR3 while the switch has not certainly finished (the stub changes CR3
/// AFTER the scheduler has chosen the target): it is removed at the next entry into
/// that core's scheduler. They are needed by `reap_dead_spaces`: an idling AP holds
/// the CR3 of its last task, and the tables of a dead process freed from under
/// it would keep being used by the AP (the same class as the "KVM internal error"
/// at a restart of a cell — there it is caught by `aps_to_kernel_cr3`).
static AP_CR3: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
pub static AS_AP_DEFER: AtomicU64 = AtomicU64::new(0);
pub static AS_AP_LEFT: AtomicU64 = AtomicU64::new(0);
static AP_CR3_PREV: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
#[inline]
fn note_cr3(cpu: usize, new: u64) {
    if cpu == 0 || cpu >= MAXCPU || new == 0 { return; }
    let new = new & !0xFFF;
    let old = AP_CR3[cpu].load(Ordering::Relaxed);
    if old == new { return; }
    AP_CR3_PREV[cpu].store(old, Ordering::SeqCst);
    AP_CR3[cpu].store(new, Ordering::SeqCst);
}
/// Is this CR3 in the queue for freeing.
fn is_dead_as(cr3: u64) -> bool {
    if cr3 == 0 { return false; }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe { for i in 0..NTASKS { let d = core::ptr::read_volatile(core::ptr::addr_of!(DEAD_AS[i])); if d != 0 && d & !0xFFF == cr3 { return true; } } }
    false
}
/// # Safety
/// C1 + C3: the queue of address spaces to free is a task-indexed table; a space is
/// freed only when NO core runs on it — that is what `active_cr3` is compared against,
/// and the APs have been moved to the kernel's CR3 by the caller.
unsafe fn reap_dead_spaces(active_cr3: u64) {
    for i in 0..NTASKS {
        let cr3 = DEAD_AS[i];
        if cr3 == 0 || cr3 == active_cr3 { continue; }
        // Not a single AP must be executing on these tables — even while idling.
        let mut on_ap = false;
        for c in 1..MAXCPU {
            let a = AP_CR3[c].load(Ordering::SeqCst);
            let pv = AP_CR3_PREV[c].load(Ordering::SeqCst);
            if a == cr3 & !0xFFF || pv == cr3 & !0xFFF {
                on_ap = true;
                // An idling one — wake it: the pickup loop will leave the dead AS by itself.
                if crate::percpu::cur_task_of(c) == u64::MAX { ap_ring(c); crate::sync::kick(c as u32); }
            }
        }
        if on_ap {
            // The first cases go out loud: otherwise by the log one cannot tell "the protection is not needed"
            // from "the protection did not work".
            let k = AS_AP_DEFER.fetch_add(1, Ordering::Relaxed);
            if k < 3 { crate::println!("sched: the freeing of the AS {:#x} is deferred — its tables are held by an AP (the idling one has been taken onto the kernel's CR3)", cr3); }
            continue;
        }
        // not a single live task must use this CR3
        let n = NREADY.load(Ordering::Relaxed); let mut busy = false;
        // An address space is held by A FINISHED task too, while it is physically
        // on a core: the interrupts work under its CR3 until the first switch.
        for t in 0..n { if CR3[t] == cr3 && (!DEAD[t] || task_on_cpu(t) || dying(t)) { busy = true; break; } }
        if busy { continue; }
        DEAD_AS[i] = 0;
        for t in 0..n { if CR3[t] == cr3 { CR3[t] = 0; } } // the dead tasks no longer point at the torn-down tables
        crate::paging::free_address_space(cr3);
    }
}
/// The diagnostics: is the frame phys mapped privately in the AS of any live task (except `except`).
#[allow(dead_code)]
pub fn frame_mapped_elsewhere(phys: u64, except: u64) -> Option<(u64, u64, u64)> {
    let n = NREADY.load(Ordering::Relaxed);
    let mut seen = [0u64; NTASKS]; let mut ns = 0;
    for t in 0..n {
        // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
        // touches, and what crosses cores is atomic (see the module's note).
        let cr3 = unsafe { CR3[t] };
        // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
        // touches, and what crosses cores is atomic (see the module's note).
        if cr3 == 0 || cr3 == except || unsafe { DEAD[t] } { continue; }
        if seen[..ns].contains(&cr3) { continue; } seen[ns] = cr3; ns += 1;
        if let Some((va, e)) = crate::paging::find_phys_in(cr3, phys) { return Some((cr3, va, e)); }
    }
    None
}
/// Give a task an FPU/SSE state from an fxsave area (a fork: a copy of the parent's).
pub fn set_fpu(tid: usize, area: u64) {
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe {
        let dst = core::ptr::addr_of_mut!(FPU[tid]) as *mut u8;
        core::ptr::write_bytes(dst, 0, FPU_AREA);                       // the XSAVE header must be clean
        core::ptr::copy_nonoverlapping(area as *const u8, dst, 512);    // the legacy area from the syscall's frame
        // XSTATE_BV = x87|SSE: XRSTOR will restore what was copied, while the AVX part
        // will be brought to the initial (zero) state — and that is what a new thread needs.
        if XSAVE_MASK.load(Ordering::Relaxed) != 0 { core::ptr::write_unaligned(dst.add(512) as *mut u64, 0x3); }
        FPU_VALID[tid] = true;
    }
}
static mut PPID: [i32; NTASKS] = [-1; NTASKS];     // the parent process's leader (-1 = there is none)

// ── PLAN-SYNC: the affinity of the tasks by core (a pinned thread on an AP, tier 2) ──
/// The task's core: 0 = the BSP, N>0 = the AP N. On a dedicated core the task is alone and lives
/// run-to-completion; on a core of the common pool there are several of them and they rotate by the tick.
static CPU_OF: [AtomicU8; NTASKS] = [const { AtomicU8::new(0) }; NTASKS];
/// The task's address space (0 — it has none of its own: a thread or a kernel thread).
/// It is needed by the TLB flush: a core executing A FOREIGN space physically has
/// nowhere to get our entries from — we do not use PCID, and a change of CR3 extinguishes the TLB
/// entirely.
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn cr3_of(t: usize) -> u64 { unsafe { CR3[t.min(NTASKS - 1)] } }

pub fn cpu_of(t: usize) -> usize { CPU_OF[t.min(NTASKS - 1)].load(Ordering::SeqCst) as usize }
pub fn set_cpu_of(t: usize, cpu: usize) { CPU_OF[t].store(cpu as u8, Ordering::SeqCst); }

/// The slot IS OCCUPIED, but the task has not been assembled entirely yet: the frame on its kernel stack, the CR3,
/// the TLS, the layout's core are set already AFTER the slot is allocated. The scheduler
/// must not take such a task.
///
/// Why this is a separate flag rather than "DEAD until it is ready". `reuse_slot` removes
/// DEAD at the moment the slot is handed out — while the previous tenant's kernel stack by that moment
/// has already been returned to the allocator, and RSP[i] points into foreign memory. A timer tick on
/// a neighbouring core that got into that window honestly switched to the "ready" task
/// and drove off by a garbage frame: a #GP on the iretq in the scheduler's stub. The same with
/// a new slot: NREADY grows earlier than anything is put into RSP[i].
/// The window is microscopic — that is why a large multi-threaded workload (the JVM creates
/// threads in batches) caught it once in several runs, while a small test never did.
static PENDING: [core::sync::atomic::AtomicBool; NTASKS] =
    [const { core::sync::atomic::AtomicBool::new(false) }; NTASKS];

/// How many times the timer's tick caught a task on the processor — a crude profile "who
/// eats the core". One increment is put per tick; the answer to the question "is the workload computing
/// or is somebody spinning in vain" cannot be got any other way: from the outside both
/// look like a busy processor.
pub static TICKS_ON: [core::sync::atomic::AtomicU64; NTASKS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; NTASKS];

/// The profile by THE ADDRESS at which the tick caught the workload (`talaria.prof=1`).
///
/// "Who ate the core" is answered by `TICKS_ON`, but there is nothing to answer the question "what exactly
/// the thread was doing": it may make no syscalls at all and spin in user space.
/// The timer's tick knows the return address of the interrupted code — that is enough to
/// collect a crude histogram by the pages of the code and hand it outside: the offset in
/// the binary is resolved on the host (`addr2line`), the kernel does not need the symbols.
pub static PROF_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
const PROF_N: usize = 64;
static PROF_PAGE: [core::sync::atomic::AtomicU64; PROF_N] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PROF_N];
static PROF_HITS: [core::sync::atomic::AtomicU64; PROF_N] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PROF_N];
static PROFK_ADDR: [core::sync::atomic::AtomicU64; PROF_N] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PROF_N];
static PROFK_HITS: [core::sync::atomic::AtomicU64; PROF_N] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PROF_N];

/// The watchdog "the workload has stalled": how many ticks in a row the core executes ONE and the same
/// task. A waiter in an mwait is "executing" too, so this counter alone means nothing
/// — it means something together with the address: if a task sits at one and the
/// same place for seconds, that is either an eternal loop or a lost wakeup, and in both
/// cases one needs the address rather than the fact.
static STUCK_TASK: [core::sync::atomic::AtomicU64; MAXCPU] =
    [const { core::sync::atomic::AtomicU64::new(u64::MAX) }; MAXCPU];
static STUCK_RIP: [core::sync::atomic::AtomicU64; MAXCPU] =
    [const { core::sync::atomic::AtomicU64::new(0) }; MAXCPU];
static STUCK_N: [core::sync::atomic::AtomicU64; MAXCPU] =
    [const { core::sync::atomic::AtomicU64::new(0) }; MAXCPU];
static STUCK_PRN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// `talaria.stuck=N` — print if a task does not leave the core for N ticks in a row.
pub static STUCK_TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn stuck_note(cpu: usize, task: usize, frame: u64) {
    use core::sync::atomic::Ordering::Relaxed;
    let limit = STUCK_TICKS.load(Relaxed);
    if limit == 0 || cpu >= MAXCPU || frame == 0 { return; }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    let rip = unsafe { core::ptr::read_volatile((frame + 15 * 8) as *const u64) };
    let same = STUCK_TASK[cpu].load(Relaxed) == task as u64;
    STUCK_TASK[cpu].store(task as u64, Relaxed);
    let n = if same { STUCK_N[cpu].fetch_add(1, Relaxed) + 1 } else { STUCK_N[cpu].store(1, Relaxed); 1 };
    let prev = STUCK_RIP[cpu].swap(rip, Relaxed);
    if n >= limit && STUCK_PRN.fetch_add(1, Relaxed) < 8 {
        // SAFETY: C3 — the interrupt/syscall FRAME of the thread this core interrupted: it lies
        // on that thread's kernel stack, which is alive for as long as the thread is, and the
        // offsets are the ones the stub pushed.
        let cs = unsafe { core::ptr::read_volatile((frame + 16 * 8) as *const u64) };
        crate::println!("[stuck] core {}: t{} has been holding it for {} ticks, rip={:#x} (the previous one {:#x}) ring{} — {}",
            cpu, task, n, rip, prev, if cs & 3 == 3 { 3 } else { 0 },
            if rip == prev { "IT STANDS STILL" } else { "it spins" });
        crate::sync::dump_tasks();
        STUCK_N[cpu].store(0, Relaxed);
    }
}

/// Mark the address of the interrupted code. `frame` is the saved switch frame
/// (15 registers, then the hardware iret frame: rip, cs, rflags, rsp, ss).
// -- THE RAW RING OF PROFILE SAMPLES -----------------------------------------
//
// The histogram by pages (`PROF_PAGE`) answers "in which piece of code the time is", and
// for a quick glance that is enough. But a profile that can be fed to
// external tools (`addr2line`, a flamegraph) must be A LIST OF SAMPLES:
// a STACK of addresses, the thread, the core, the protection ring. Folding a list into a histogram on
// the host is possible, unfolding a histogram back is not.
//
// A STACK RATHER THAN ONE ADDRESS. "The time is in memcpy" is not an answer: memcpy is called from everywhere.
// The answer is who called it, and it lies in the chain of frames (see `obs::unwind`).
// The depth is fixed: eight addresses per sample, 2048 samples — 128 KB of .bss.
// A variable length would save memory at the price of walking the ring from the end at every
// read, and memory is cheaper here.
//
// THE THINNING. The slice is 0.5 ms by default, that is, the tick comes 2000 times a
// second: a ring of 2048 samples would fill in a second and the profile would cover
// one instant. So a sample is taken at every `PROF_DIV`-th tick, and
// the divisor is computed from THE DESIRED FREQUENCY (`perf on [hz]`) rather than set by a number:
// the operator thinks about "two hundred samples a second" rather than about the slice's length.
pub const PROFR_N: usize = 2048;
pub const PROFR_D: usize = crate::obs::unwind::MAXDEPTH;
static PROFR_STK: [[AtomicU64; PROFR_D]; PROFR_N] =
    [const { [const { AtomicU64::new(0) }; PROFR_D] }; PROFR_N];
/// tid | cpu<<8 | ring<<16 | the depth<<24.
static PROFR_META: [AtomicU64; PROFR_N] = [const { AtomicU64::new(0) }; PROFR_N];
static PROFR_W: AtomicU64 = AtomicU64::new(0);
/// A sample is taken at every N-th tick (1 — at every one).
pub static PROF_DIV: AtomicU64 = AtomicU64::new(1);
static PROF_SKIP: AtomicU64 = AtomicU64::new(0);

/// Set the samples' frequency in hertz: the divisor is computed from the current slice.
/// 0 — at every tick. It returns the resulting frequency.
pub fn prof_set_hz(hz: u64) -> u64 {
    let slice = crate::smp::effective_slice_ns().max(1);
    let ticks = (1_000_000_000 / slice).max(1);
    let div = if hz == 0 || ticks <= hz { 1 } else { ticks / hz };
    PROF_DIV.store(div, Ordering::SeqCst);
    ticks / div
}
/// The effective frequency of the samples (Hz).
pub fn prof_hz() -> u64 {
    let slice = crate::smp::effective_slice_ns().max(1);
    (1_000_000_000 / slice).max(1) / PROF_DIV.load(Ordering::Relaxed).max(1)
}

/// How many samples are available and how many there were in total (the second is greater than the first if
/// the ring has turned over: the operator must see that a part of the profile has been lost).
pub fn prof_ring_stats() -> (usize, u64) {
    let w = PROFR_W.load(Ordering::Relaxed);
    ((w as usize).min(PROFR_N), w)
}
/// The sample `i` (0 — the oldest of the available ones): the stack goes into `stk`, it returns
/// (the depth, the tid, the cpu, the ring).
pub fn prof_ring_get(i: usize, stk: &mut [u64; PROFR_D]) -> (usize, usize, usize, u8) {
    let (have, w) = prof_ring_stats();
    for k in 0..PROFR_D { stk[k] = 0; }
    if i >= have { return (0, 0, 0, 0); }
    let idx = ((w - have as u64 + i as u64) % PROFR_N as u64) as usize;
    let m = PROFR_META[idx].load(Ordering::Relaxed);
    let d = (((m >> 24) & 0xFF) as usize).min(PROFR_D);
    for k in 0..d { stk[k] = PROFR_STK[idx][k].load(Ordering::Relaxed); }
    (d, (m & 0xFF) as usize, ((m >> 8) & 0xFF) as usize, ((m >> 16) & 0xFF) as u8)
}
/// Forget what has been accumulated (before a new measurement).
pub fn prof_ring_reset() {
    PROFR_W.store(0, Ordering::SeqCst);
    PROF_SKIP.store(0, Ordering::SeqCst);
    for i in 0..PROF_N { PROF_PAGE[i].store(0, Ordering::Relaxed); PROF_HITS[i].store(0, Ordering::Relaxed);
                         PROFK_ADDR[i].store(0, Ordering::Relaxed); PROFK_HITS[i].store(0, Ordering::Relaxed); }
}

fn prof_note(frame: u64) {
    use core::sync::atomic::Ordering::Relaxed;
    if !PROF_ON.load(Relaxed) || frame == 0 { return; }
    // SAFETY: C3 — the interrupt/syscall FRAME of the thread this core interrupted: it lies
    // on that thread's kernel stack, which is alive for as long as the thread is, and the
    // offsets are the ones the stub pushed.
    let (rip, cs) = unsafe {
        (core::ptr::read_volatile((frame + 15 * 8) as *const u64),
         core::ptr::read_volatile((frame + 16 * 8) as *const u64))
    };
    {   // The raw ring: A STACK rather than one address, and not at every tick (see PROF_DIV).
        // The unwinding goes BEFORE any processing: the chain of frames is alive exactly
        // now, while the interrupted context has not been touched yet.
        let div = PROF_DIV.load(Relaxed);
        let take = div <= 1 || PROF_SKIP.fetch_add(1, Relaxed).is_multiple_of(div);
        if take {
            // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row
            // it touches, and what crosses cores is atomic (see the module's note).
            let rbp = unsafe { core::ptr::read_volatile((frame + 8 * 8) as *const u64) };
            let user = cs & 3 == 3;
            let mut stk = [0u64; PROFR_D];
            let d = crate::obs::unwind::walk(rip, rbp, user, &mut stk);
            let w = PROFR_W.fetch_add(1, Relaxed);
            let i = (w % PROFR_N as u64) as usize;
            for k in 0..d { PROFR_STK[i][k].store(stk[k], Relaxed); }
            PROFR_META[i].store(crate::percpu::cur_task() & 0xFF
                | ((crate::percpu::cpu_id() as u64 & 0xFF) << 8)
                | ((cs & 3) << 16)
                | ((d as u64 & 0xFF) << 24), Relaxed);
        }
    }
    // Ring 0 is written SEPARATELY and with an accuracy of 64 bytes: the kernel has symbols, and
    // "the thread sleeps in the kernel" differs from "the thread spins in the kernel" exactly by the address.
    if cs & 3 != 3 {
        let slot = rip & !0x3F;
        for i in 0..PROF_N {
            let p = PROFK_ADDR[i].load(Relaxed);
            if p == slot { PROFK_HITS[i].fetch_add(1, Relaxed); return; }
            if p == 0 && PROFK_ADDR[i].compare_exchange(0, slot, Relaxed, Relaxed).is_ok() {
                PROFK_HITS[i].fetch_add(1, Relaxed);
                return;
            }
        }
        return;
    }
    let page = rip & !0xFFF;
    for i in 0..PROF_N {
        let p = PROF_PAGE[i].load(Relaxed);
        if p == page { PROF_HITS[i].fetch_add(1, Relaxed); return; }
        if p == 0 && PROF_PAGE[i].compare_exchange(0, page, Relaxed, Relaxed).is_ok() {
            PROF_HITS[i].fetch_add(1, Relaxed);
            return;
        }
    }
}

/// The profile's histogram outside: (the code page's address, the hits).
pub fn prof_slot(i: usize) -> (u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    if i >= PROF_N { return (0, 0); }
    (PROF_PAGE[i].load(Relaxed), PROF_HITS[i].load(Relaxed))
}
pub fn prof_len() -> usize { PROF_N }
/// The same for ring 0 (the address with an accuracy of 64 bytes, the hits).
pub fn profk_slot(i: usize) -> (u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    if i >= PROF_N { return (0, 0); }
    (PROFK_ADDR[i].load(Relaxed), PROFK_HITS[i].load(Relaxed))
}

/// May the task t not be scheduled (it has left or is still being assembled).
///
/// A DEAD TASK PREEMPTED IN THE MIDDLE OF THE KERNEL IS SCHEDULED. Previously `DEAD` meant
/// "never launch it again", and a thread killed (`exit_group`, a sister's crash,
/// a restart by the supervisor) in the middle of an `fsync` stayed the holder of
/// ext2's journal transaction: a new instance after the restart stood on
/// "the journal is busy with a foreign transaction" forever, while a core dump waited for the same
/// journal, while the sisters of the crashed thread kept working. Now such a thread
/// finishes the syscall (the interruptible waits see the death as a signal — see
/// `proc::sig_pending`) and stops at the way out of it (`stop_if_dead`).
/// A dead thread preempted in ring 3 is still not launched.
#[inline]
fn not_ready(t: usize) -> bool {
    if PENDING[t].load(Ordering::SeqCst) { return true; }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    let dead = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(DEAD[t])) };
    if !dead { return false; }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    let gone = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(GONE[t])) };
    gone || !ctx_in_kernel(t)
}
/// The task's saved context is in the kernel (it was preempted in the middle of a syscall or
/// of a handler). The context is checked for belonging to its kernel stack:
/// for a task that has never been taken off a core anything may lie there.
fn ctx_in_kernel(t: usize) -> bool {
    if t >= NTASKS { return false; }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe {
        let sp = core::ptr::read_volatile(core::ptr::addr_of!(RSP[t]));
        let top = core::ptr::read_volatile(core::ptr::addr_of!(KSTACK[t]));
        let pages = { let k = core::ptr::read_volatile(core::ptr::addr_of!(KSZ[t])); if k != 0 { k } else { KSTACK_PAGES } };
        if sp == 0 || top == 0 || sp < top - pages * 4096 || sp + 20 * 8 > top { return false; }
        core::ptr::read_volatile((sp + 16 * 8) as *const u64) & 3 == 0
    }
}
/// It is dead but has not stopped yet: it stands on a core or was preempted in the middle of the kernel and
/// will be run to the end. While such a task exists, its address space, its stack
/// and what it holds (the FS locks, a journal transaction) must not be freed.
pub fn dying(t: usize) -> bool {
    if t >= NTASKS { return false; }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe {
        if !core::ptr::read_volatile(core::ptr::addr_of!(DEAD[t])) { return false; }
        // It has left for good — it has nothing to do, even if the core still lists it as
        // the current one (the BSP spins its `hlt` until the next tick). The freeing of its
        // stack and AS separately waits for it to leave the core (`task_on_cpu`).
        if core::ptr::read_volatile(core::ptr::addr_of!(GONE[t])) { return false; }
    }
    task_on_cpu(t) || ctx_in_kernel(t)
}
/// A DEAD THREAD CAUGHT BY A TICK IN RING 3 LEAVES THROUGH THE KERNEL. A thread
/// spinning in its own code without syscalls will never reach `stop_if_dead`,
/// while the scheduler, having nobody else to take, stayed on it: after
/// a restart by the watchdog such a thread occupied its core at 92 % forever, and
/// the supervisor waited for it just as forever. Here its saved frame is rewritten:
/// the next entry into it is an entry into the kernel, into `dead_park`, that is, an ordinary
/// exit of a thread (`exit_current_code`). The user registers before the rewriting
/// are remembered — a core dump will show where it stood in its own code.
fn park_dead_in_user(t: usize, sp: u64) {
    if t >= NTASKS || task_alive(t) || sp == 0 { return; }
    // SAFETY: C3 — the interrupt/syscall FRAME of the thread this core interrupted: it lies
    // on that thread's kernel stack, which is alive for as long as the thread is, and the
    // offsets are the ones the stub pushed.
    unsafe {
        // The frame's layout is `timer_stub_sched`: 0..14 GP (r15..rax), 15 rip,
        // 16 cs, 17 rflags, 18 rsp, 19 ss.
        let g = |k: u64| core::ptr::read_volatile((sp + k * 8) as *const u64);
        if g(16) & 3 != 3 { return; }
        set_exit_uregs(t, [g(15), g(18), g(8), g(13), g(3), g(2), g(1), g(0), g(17), u64::MAX]);
        let w = |k: u64, v: u64| core::ptr::write_volatile((sp + k * 8) as *mut u64, v);
        w(15, dead_park as *const () as u64);
        w(16, 0x08);                                  // kernel CS
        w(17, 0x2);                                   // IF=0: the exit will decide itself when to open them
        w(18, ((sp - 512) & !0xF) - 8);               // its own kernel stack, as after a call
        w(19, 0x10);                                  // kernel SS
    }
}
extern "C" fn dead_park() -> ! {
    let t = current_id();
    exit_current_code(exit_code_of(t))
}
/// Remember the user registers of a thread stopped at the way out of
/// a syscall (for a core dump), and take them back.
pub fn set_exit_uregs(t: usize, r: [u64; 10]) {
    if t >= NTASKS { return; }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe { EXITREG[t] = r; core::ptr::write_volatile(core::ptr::addr_of_mut!(EXITREG_OK[t]), true); }
}
pub fn exit_uregs(t: usize) -> Option<[u64; 10]> {
    if t >= NTASKS { return None; }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe { if core::ptr::read_volatile(core::ptr::addr_of!(EXITREG_OK[t])) { Some(EXITREG[t]) } else { None } }
}

/// The task is assembled — hand it to the scheduler (and ring its core's doorbell).
pub fn publish(t: usize) {
    if t >= NTASKS { return; }
    // The mark "it left the core" is started HERE rather than at the first preemption.
    // Otherwise a just-created thread that has never run looks infinitely starving to a
    // thief and moves off the core where the layout put it — that is,
    // stealing cancels the layout exactly at the moment when the layout is right. Counting from the birth
    // gives the thread a slice to start.
    note_left_cpu(t);
    acct_born(t);                                 // the slot is occupied by a new task: the top/ps counters start from zero
    note_ready(t);                                // from this moment it waits for the processor (see `READY_TSC`)
    PENDING[t].store(false, Ordering::SeqCst);
    let cpu = cpu_of(t);
    if cpu != 0 && cpu != crate::percpu::cpu_id() { ap_ring(cpu); }
}

/// The thread's name (`prctl(PR_SET_NAME)` / `pthread_setname_np`) — the manifest lays
/// the threads out over the cores by it, and `ps`/`cores` read it too. 16 bytes, as
/// `comm` in Linux (15 significant + \0).
static mut COMM: [[u8; 16]; NTASKS] = [[0; 16]; NTASKS];
pub fn set_comm(t: usize, name: &[u8]) {
    if t >= NTASKS { return; }
    let n = name.len().min(15);
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe {
        let d = core::ptr::addr_of_mut!(COMM[t]) as *mut u8;
        for i in 0..n { core::ptr::write_volatile(d.add(i), name[i]); }
        for i in n..16 { core::ptr::write_volatile(d.add(i), 0); }
    }
}
/// The thread's name as a copy (in no_std handing a slice out of a `static mut` outside is a source of UB).
pub fn comm(t: usize) -> [u8; 16] {
    if t >= NTASKS { return [0; 16]; }
    let mut out = [0u8; 16];
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe { let s = core::ptr::addr_of!(COMM[t]) as *const u8; for i in 0..16 { out[i] = core::ptr::read_volatile(s.add(i)); } }
    out
}
/// The length of the thread's name (0 — the thread has not named itself).
pub fn comm_len(t: usize) -> usize { let c = comm(t); let mut n = 0; while n < 16 && c[n] != 0 { n += 1; } n }

/// The cores on which THE SCHEDULER SPINS (the BSP + the APs that reached `ap_loop`).
/// A mask separate from `percpu::ONLINE` is not pedantry: ONLINE also includes a core
/// brought up in self-test mode (`AP_MODE=1`), which does not take tasks
/// at all. To put a thread on such a core is to lose it silently, and that is exactly that
/// class of error which session 10 was paid for.
///
/// This is NOT a "pool of cores" in the manifest's sense (`pool=`): those are named groups
/// inside this set, and they live in `affinity.rs`. Here it is simply "the cores where
/// the scheduler works".
pub static SCHED_CPUS: AtomicU64 = AtomicU64::new(1);
pub fn sched_cpus() -> u64 { SCHED_CPUS.load(Ordering::SeqCst) }
pub fn sched_cpu(cpu: usize) -> bool { cpu < MAXCPU && SCHED_CPUS.load(Ordering::SeqCst) & (1 << cpu) != 0 }

/// How many LIVE tasks belong to the core cpu (the core's load for the balancer).
pub fn tasks_on(cpu: usize) -> usize {
    let n = NREADY.load(Ordering::SeqCst);
    let mut c = 0;
    for t in 0..n {
        // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
        // touches, and what crosses cores is atomic (see the module's note).
        if unsafe { core::ptr::read_volatile(core::ptr::addr_of!(DEAD[t])) } { continue; }
        if CPU_OF[t].load(Ordering::SeqCst) as usize == cpu { c += 1; }
    }
    c
}

/// Is the task executing RIGHT NOW on its core. The balancer may
/// move only those that are NOT executing: their context lies entirely in
/// `RSP[t]`/`CR3[t]`/`FSBASE[t]`/`FPU[t]`, and a move costs one write into
/// `CPU_OF` — neither new stacks nor surgery over the return frame.
/// For a snapshot of the waiters (`sync::dump_tasks`): the flags by which the scheduler decides
/// whether it may enter a task — without them the snapshot "ready but not running" is mute.
pub fn sched_flags(t: usize) -> (bool, bool) {
    if t >= NTASKS { return (false, false); }
    (AP_TAKEN[t].load(Ordering::SeqCst), PENDING[t].load(Ordering::SeqCst))
}
pub fn hint_of(cpu: usize) -> usize { HINT[cpu.min(MAXCPU - 1)].0.load(Ordering::SeqCst) }
/// A RELEASED TASK'S STACK IS STILL OCCUPIED. The scheduler releases the current task
/// (`AP_TAKEN=false`, on the BSP — `CURRENT=next`) WHILE STANDING ON ITS KERNEL STACK: the remainder of
/// `ap_schedule`/`bsp_schedule`, `rust_schedule` and the stub up to `mov %rax,%rsp`
/// are executed there. A foreign core that entered the task in that window (a hint,
/// a pull, a steal, a pickup) executes its code ON THE SAME STACK — two cores write
/// one memory. Session 32: with the pull for the sake of the balance (`talaria.wakepull=1
/// wakeidle=1`) MariaDB stalled within a minute — a thread in the "yield" phase held the core
/// with IF=0 for milliseconds, then forever; an NMI snapshot showed the core in a foreign frame.
/// The flag is raised BEFORE the release and is extinguished by the stub AFTER the stack is changed
/// (`percpu::off_stack`); the taking core waits for it in `stack_wait`.
/// THE BSP IN LIMBO: `migrate_current_to` has built a fresh context of the task for
/// another core, while the BSP stayed on its old stack waiting for a tick — saving that
/// context over the fresh one is NOT ALLOWED. Previously limbo was recognised by `CPU_OF[cur]!=0`,
/// and that was BROADER than necessary: a move (`migrate_idle_task`) has a window
/// "CPU_OF is already foreign, CURRENT has not been reread yet, a rollback is ahead" — a BSP tick in that
/// window THREW AWAY the live context of an executing task, the rollback returned it to the BSP, and
/// the next entry raised the PREVIOUS saved frame: the thread executed the old
/// syscall a second time with foreign registers (session 32: a #PF in smoltcp with self=0,
/// an int3 in a shim, a thread with IF=0 in the "yield" phase; once in 1-2 min under a pull).
pub static BSP_LIMBO: AtomicBool = AtomicBool::new(false);
/// The instrument: the BSP's ticks that caught `CPU_OF[cur]!=0` for a live task (the move's window) —
/// the former code would lose the context here.
pub static BSP_SAVE_FOREIGN: AtomicU64 = AtomicU64::new(0);
#[unsafe(no_mangle)]
pub static STACK_BUSY: [AtomicBool; NTASKS] = [const { AtomicBool::new(false) }; NTASKS];
/// The instrument: how many times a taking core found the stack occupied, the worst wait and
/// how many times we waited longer than 10 ms (the releasing core died on a foreign stack —
/// we enter all the same, otherwise the task would be lost).
pub static STACK_WAITS: AtomicU64 = AtomicU64::new(0);
pub static STACK_WAIT_MAX_NS: AtomicU64 = AtomicU64::new(0);
pub static STACK_WAIT_LONG: AtomicU64 = AtomicU64::new(0);
/// Wait until the core that released the task `t` leaves its kernel stack.
/// It is called AFTER a successful marking (`AP_TAKEN.swap(true)` / `CURRENT=next`).
#[inline]
pub fn stack_wait(t: usize) {
    if t >= NTASKS || !STACK_BUSY[t].load(Ordering::SeqCst) { return; }
    let t0 = crate::tsc::rdtsc();
    STACK_WAITS.fetch_add(1, Ordering::Relaxed);
    let mut n = 0u32;
    while STACK_BUSY[t].load(Ordering::SeqCst) {
        core::hint::spin_loop();
        n = n.wrapping_add(1);
        if n & 0x3FF == 0 {
            let us = crate::tsc::rdtsc().wrapping_sub(t0).saturating_mul(1_000_000) / crate::tsc::calibrate_cached().max(1);
            if us > 10_000 { STACK_WAIT_LONG.fetch_add(1, Ordering::Relaxed); break; }
        }
    }
    let ns = crate::tsc::cycles_to_ns(crate::tsc::rdtsc().wrapping_sub(t0), crate::tsc::calibrate_cached());
    let mut m = STACK_WAIT_MAX_NS.load(Ordering::Relaxed);
    while ns > m { match STACK_WAIT_MAX_NS.compare_exchange_weak(m, ns, Ordering::Relaxed, Ordering::Relaxed) { Ok(_) => break, Err(v) => m = v } }
}
/// THE INSTRUMENT OF THE FRAME'S FRESHNESS. Every write of `RSP[t]` (a context save, a fresh
/// frame at a creation/a move) sets a mark; every entry into the task removes it.
/// An entry by an already removed mark is an entry into A USED frame: somebody raised
/// the task's previous context (session 32: the third path of context corruption was looked for
/// by exactly this — neither `STACK_BUSY` nor `BSP_LIMBO` caught it). The frame itself is
/// checked right here: cs must be 0x8/0x2B, rflags has bit 1 set and
/// the upper 42 bits empty — otherwise what lies at the address `RSP[t]` is not a frame.
pub static CTX_FRESH: [AtomicBool; NTASKS] = [const { AtomicBool::new(false) }; NTASKS];
pub static CTX_LAST_CPU: [AtomicU8; NTASKS] = [const { AtomicU8::new(0xFF) }; NTASKS];
pub static CTX_STALE: AtomicU64 = AtomicU64::new(0);
/// The ticks that caught a core on the pickup loop's stack with a named task (see `ap_schedule`).
pub static LOOP_TICKS: AtomicU64 = AtomicU64::new(0);
pub static CTX_BAD: AtomicU64 = AtomicU64::new(0);
#[inline]
pub fn ctx_saved(t: usize) { if t < NTASKS { CTX_FRESH[t].store(true, Ordering::SeqCst); } }
#[inline]
pub fn ctx_enter(t: usize, cpu: usize, sp: u64) {
    if t >= NTASKS { return; }
    let prev = CTX_LAST_CPU[t].swap(cpu as u8, Ordering::SeqCst);
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    let (rip, cs, rfl) = unsafe { (
        core::ptr::read_volatile((sp + 15 * 8) as *const u64),
        core::ptr::read_volatile((sp + 16 * 8) as *const u64),
        core::ptr::read_volatile((sp + 17 * 8) as *const u64)) };
    if !CTX_FRESH[t].swap(false, Ordering::SeqCst) {
        let n = CTX_STALE.fetch_add(1, Ordering::Relaxed);
        if n < 4 {
            crate::println!("sched: AN ENTRY INTO A USED FRAME t{} by core {} (the previous entry was by core {}): rsp={:#x} rip={:#x} cs={:#x} rflags={:#x} CPU_OF={} AP_TAKEN={} CURRENT={} syscall #{}",
                t, cpu, prev, sp, rip, cs, rfl, CPU_OF[t].load(Ordering::Relaxed), AP_TAKEN[t].load(Ordering::Relaxed),
                CURRENT.load(Ordering::Relaxed), crate::syscall::LAST_SYS[t].load(Ordering::Relaxed) >> 48);
        }
    }
    if (cs != 0x8 && cs != 0x2B) || rfl & 2 == 0 || rfl >> 22 != 0 {
        let n = CTX_BAD.fetch_add(1, Ordering::Relaxed);
        if n < 4 {
            crate::println!("sched: AT THE ADDRESS RSP[t{}] THERE IS NO FRAME (the entry is by core {}, the previous entry was by core {}): rsp={:#x} rip={:#x} cs={:#x} rflags={:#x} kstack={:#x} syscall #{}",
                // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose
                // row it touches, and what crosses cores is atomic (see the module's note).
                t, cpu, prev, sp, rip, cs, rfl, unsafe { KSTACK[t] }, crate::syscall::LAST_SYS[t].load(Ordering::Relaxed) >> 48);
        }
    }
}
/// OCCUPY A TASK BY AN AP CORE BY THE RIGHT OF OWNERSHIP (`CPU_OF[t] == cpu`).
///
/// The mark `AP_TAKEN` is set by a swap, but the check "the task is mine" before it costs
/// nothing: between the check and the marking the task managed to be MOVED to the BSP by a
/// network waiter (`migrate_idle_task`: CPU_OF=0, a barrier, a read of AP_TAKEN — still
/// false), while the BSP managed to ENTER it (CURRENT=t, a barrier, a read of AP_TAKEN — still
/// false). Our swap went through after that, and two cores raised one frame
/// (session 32: "AN ENTRY INTO A USED FRAME t13 by core 1 (the previous entry was by core 0)
/// … CPU_OF=0 AP_TAKEN=true CURRENT=13", followed by a #UD/#BP/#PF in ring 0 with foreign
/// registers; it reproduced with a string of short mysql connections at the ceiling within
/// 3-30 probes). The protocol is the same as with the BSP and the move: the marking -> a barrier ->
/// a recheck of the ownership. We saw a foreign core — we remove the mark (it is ours: the swap
/// returned false) and, if the new owner is another AP, we ring it: it could have
/// skipped the task after finding our mark.
#[inline]
fn ap_claim(t: usize, cpu: usize) -> bool {
    if AP_TAKEN[t].swap(true, Ordering::SeqCst) { return false; }
    core::sync::atomic::fence(Ordering::SeqCst);
    let owner = CPU_OF[t].load(Ordering::SeqCst) as usize;
    if owner != cpu {
        AP_TAKEN[t].store(false, Ordering::SeqCst);
        CLAIM_MOVED.fetch_add(1, Ordering::Relaxed);
        if owner != 0 { ap_ring(owner); }
        return false;
    }
    true
}
/// How many times the mark was removed because the task moved away between the check and the swap.
pub static CLAIM_MOVED: AtomicU64 = AtomicU64::new(0);
/// A steal with a check of the donor BSP (see `ap_schedule`): the BSP declares its occupancy
/// through CURRENT rather than through AP_TAKEN — we verify AFTER our own marking.
#[inline]
fn ap_claim_steal(t: usize, cpu: usize) -> bool {
    if AP_TAKEN[t].swap(true, Ordering::SeqCst) { return false; }
    core::sync::atomic::fence(Ordering::SeqCst);
    if CPU_OF[t].load(Ordering::SeqCst) == 0 && !BSP_IDLE.load(Ordering::SeqCst) && CURRENT.load(Ordering::SeqCst) == t {
        AP_TAKEN[t].store(false, Ordering::SeqCst); // the BSP managed to enter it
        STEAL_BSP_BUSY.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    CPU_OF[t].store(cpu as u8, Ordering::SeqCst);
    STOLEN.fetch_add(1, Ordering::Relaxed);
    true
}
pub static STEAL_BSP_BUSY: AtomicU64 = AtomicU64::new(0);
pub fn is_running(t: usize) -> bool {
    let cpu = cpu_of(t);
    if cpu == 0 { return !BSP_IDLE.load(Ordering::SeqCst) && CURRENT.load(Ordering::SeqCst) == t; }
    AP_TAKEN[t].load(Ordering::SeqCst)
}

/// Move a NOT executing task to another core of the pool. It returns false
/// if the task is on a core right now (its context is in the registers — it must not be touched).
pub fn migrate_idle_task(t: usize, cpu: usize) -> bool {
    if t >= NTASKS || !sched_cpu(cpu) { return false; }
    if is_running(t) || !task_alive(t) { return false; }
    let old = cpu_of(t);
    if old == cpu { return true; }
    // ANNOUNCE THE MOVE, THEN RECHECK THE OCCUPANCY — the same protocol as with the
    // BSP and the thieves (see `bsp_schedule`). The former version did it the other way round and worse:
    // it checked "it is not executing", and then UNCONDITIONALLY cleared AP_TAKEN. Between
    // the check and the clearing the old core's scheduler managed to ENTER the task
    // (`AP_TAKEN.swap(true)`), the clearing erased its mark, and the new core, having received
    // a call, took the same task a second time: two cores executed ONE context
    // ("core 1: executes t8 / core 2: executes t8" in the IT HAS HUNG dump of the actors
    // `ring tsync none`). The window is narrow, but this path goes from EVERY wakeup
    // (`wake_target`) rather than once every 32 ms from the balancer.
    //
    // The order is now: CPU_OF -> a barrier -> a read of the old owner's mark; in the
    // scheduler it is the mark -> a barrier -> a read of CPU_OF (an AP rings the
    // owner back at the exit, the BSP backs off). With any interleaving at least one
    // side sees the other. There is no clearing of AP_TAKEN at all: for a non-executing task
    // it is false anyway, while for an executing one it must not be touched.
    CPU_OF[t].store(cpu as u8, Ordering::SeqCst);
    core::sync::atomic::fence(Ordering::SeqCst);
    let entered = AP_TAKEN[t].load(Ordering::SeqCst)
        || (old == 0 && !BSP_IDLE.load(Ordering::SeqCst) && CURRENT.load(Ordering::SeqCst) == t);
    if entered {
        // A rollback happens only if CPU_OF is still ours: a thief could have already rewritten it to itself.
        let _ = CPU_OF[t].compare_exchange(cpu as u8, old as u8, Ordering::SeqCst, Ordering::SeqCst);
        MIGRATE_LOST.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    if cpu != 0 { ap_ring(cpu); }
    true
}
/// How many times a move backed off after finding the scheduler already inside the task (the window above).
pub static MIGRATE_LOST: AtomicUsize = AtomicUsize::new(0);
/// A CALL TO AN IDLING NEIGHBOUR: MADE, MEASURED AND REMOVED (session 25).
///
/// The reasoning was this: an AP's pickup loop makes exactly one attempt to steal and
/// falls asleep on its own doorbell, which means it will not see a queue that formed LATER —
/// one must ring it from a loaded core's tick. The mechanism was written, a knob for the
/// counter-measurement was introduced — and NO CONTRIBUTION WAS FOUND: the gate `talaria.test=stealbal`
/// passes with it and without it identically (5-7 steals in both cases), with ClickHouse
/// there are 10-41 calls per run and there is no difference in the times.
///
/// The reason why the premise is wrong is worth writing down:
/// AN UNDERLOADED CORE IS ALMOST NEVER IN THE PICKUP LOOP. It sits inside
/// ITS OWN task, which sleeps on a futex — and a tick comes there every slice, and
/// `ap_schedule` calls `steal_candidate` itself from it. An empty pickup loop happens
/// only on a core to which not a single task is assigned at all; with any
/// multi-threaded workload there is no such core.
///
/// HOW MANY TASKS ARE READY TO RUN AT ALL — a histogram by ticks (`talaria.runhist=1`).
///
/// The question that cannot be settled otherwise: "a thread is ready, but there is nowhere to put it"
/// or "the threads are simply NOT READY". Both look the same — the cores are underloaded —
/// while they are cured by the opposite: the first by the layout, the second by the wakeup path.
/// It is counted at the tick of ONE core (the zeroth), that is, one walk over the task
/// table per slice, and only when the knob is on.
pub static RUNHIST_ON: AtomicBool = AtomicBool::new(false);
pub static RUNHIST: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];
/// Next to the number of the READY ones — the number of the cores OCCUPIED by them. And that is the answer: there are four
/// ready ones, while there are three cores under them — which means it is about the layout rather than about the wakeup.
pub static OCCHIST: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];
/// A BAD LAYOUT: the ticks at which some core has two or more ready ones,
/// while another core of the scheduler is EMPTY. `MISPLACED[0]` — how many such ticks there were in total,
/// `[1]` — how many "extra" ready ones there were at them (the sum of cnt-1 over the overloaded
/// cores), `[2]` — how many of the extra ones are pinned (their move is forbidden by the rule).
pub static MISPLACED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// Why an empty neighbour would not steal the extra ready one: it is pinned / it is executing /
/// it sleeps by `blocked_for_sched` / the mask / a refusal by the balance of the assigned ones / it is hot / it would steal it.
pub static MIS_WHY: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
pub fn runhist_note() {
    if !RUNHIST_ON.load(Ordering::Relaxed) { return; }
    let mut cnt = [0u8; MAXCPU];
    runnable_counts(&mut cnt);
    let (mut tot, mut occ) = (0usize, 0usize);
    let online = sched_cpus();
    let mut empty = false;
    for c in 0..MAXCPU {
        tot += cnt[c] as usize;
        if cnt[c] != 0 { occ += 1; } else if online & (1u64 << c) != 0 { empty = true; }
    }
    RUNHIST[tot.min(8)].fetch_add(1, Ordering::Relaxed);
    OCCHIST[occ.min(8)].fetch_add(1, Ordering::Relaxed);
    let excess: usize = cnt.iter().map(|&k| (k as usize).saturating_sub(1)).sum();
    if empty && excess != 0 {
        MISPLACED[0].fetch_add(1, Ordering::Relaxed);
        MISPLACED[1].fetch_add(excess as u64, Ordering::Relaxed);
        // The pinned ones among the ready ones on the overloaded cores — and WHY THE EXTRA ONE
        // WAS NOT STOLEN by an empty neighbour: the same filters as in `steal_candidate`, in
        // order, for every extra ready one and the first empty core.
        let n = NREADY.load(Ordering::Relaxed).min(NTASKS);
        let mut pinned = 0u64;
        let thief = (0..MAXCPU).find(|&c| cnt[c] == 0 && online & (1u64 << c) != 0).unwrap_or(0);
        for t in 0..n {
            let c = CPU_OF[t].load(Ordering::Relaxed) as usize;
            if !(c < MAXCPU && cnt[c] >= 2 && task_alive(t) && !not_ready(t) && crate::sync::runnable_peek(t)) { continue; }
            if crate::affinity::is_pinned(t) { pinned += 1; }
            let why = if crate::affinity::is_pinned(t) { 0 }
                else if is_running(t) { 1 }
                else if crate::sync::blocked_for_sched(t) { 2 }
                else if crate::affinity::allowed_mask(t) & (1u64 << thief) == 0 { 3 }
                else if tasks_on(thief) + 2 > tasks_on(c) { 4 }
                else if ns_since_off(t) < crate::smp::effective_slice_ns() { 5 }
                else { 6 };
            MIS_WHY[why].fetch_add(1, Ordering::Relaxed);
        }
        MISPLACED[2].fetch_add(pinned, Ordering::Relaxed);
    }
}
/// THE PUSH: an overloaded core at its own tick HANDS an extra ready task over to
/// an empty neighbour ITSELF.
///
/// Stealing is arranged as "the thief comes itself": a core without ready ones of its own enters
/// `steal_candidate` from the pickup loop and from the scheduler. But on a core with ONE
/// task the scheduler almost never runs: the task sleeps on the doorbell or on
/// a word in an mwait, while such a core's tick is stretched to `IDLESLICE` (2 ms). The measurement on
/// ClickHouse (a `LIKE` over a table in memory, `max_threads=4`, `talaria.runhist=1`):
/// the layout "two ready ones on a core with an empty neighbour" held for 1005 ticks out of
/// ~2000, while the empty neighbour meanwhile TRIED to steal 96 times, and in 91 of them
/// the balance rule by the ASSIGNED tasks refused it (the pool's sleeping threads
/// counted as load). The layout at every wakeup (`wake_target`) is
/// correct, but the executors fall asleep and wake up hundreds of times per query and
/// slide into 2+1+1+0.
///
/// The first variant — to wake the neighbour so that it comes to steal — was checked and
/// removed: the neighbour came (1250 calls, 1250 yields), but it found the donor
/// EXACTLY at the moment of the tasks' change at a tick: the one that had waited a quantum had just got
/// the core, while the one that left waits zero — "hot" 1140 times out of 1245. So the move
/// is made by the donor itself, which has the whole picture before its eyes: at a tick, having seen
/// a second ready one of its own and an empty shared core of the scheduler, it hands over the one
/// that has waited longer than `STEAL_WAIT_NS` (that is, has stood ready for almost a quantum — a
/// "producer-consumer" pair that changes over in microseconds does not wait that long), while
/// it itself continues with the current one — that one is cache-hot. The balance by the READY ones: the donor has
/// 2+ -> 1+, the neighbour has 0 -> 1. It is switched on by `talaria.stealpush=1`; by default it is
/// off until the contribution is measured on ClickHouse AND on the actors' matrix.
pub static STEAL_PUSH: AtomicBool = AtomicBool::new(false);
/// The push's counters: [0] the moves, [1] the neighbour was woken by an IPI (it slept on a word),
/// [2] the move backed off (the scheduler managed to enter), [3] no candidate was found.
pub static PUSH_STAT: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// The debug log of the push's latest moves: (tsc<<24 | t<<16 | from<<8 | to).
static PUSH_LOG: [AtomicU64; 32] = [const { AtomicU64::new(0) }; 32];
static PUSH_LOG_N: AtomicUsize = AtomicUsize::new(0);
fn t_pack(t: usize, from: usize, to: usize) -> u64 {
    (crate::tsc::rdtsc() << 24) | ((t as u64 & 0xFF) << 16) | ((from as u64 & 0xFF) << 8) | (to as u64 & 0xFF)
}
fn push_log(v: u64) { let i = PUSH_LOG_N.fetch_add(1, Ordering::Relaxed) % 32; PUSH_LOG[i].store(v, Ordering::Relaxed); }
/// Printing the push's log (at a fatal signal of the workload under `talaria.stealpush`).
pub fn dump_push_log() {
    if !STEAL_PUSH.load(Ordering::Relaxed) { return; }
    let n = PUSH_LOG_N.load(Ordering::Relaxed);
    let now = crate::tsc::rdtsc();
    crate::println!("[push] {} moves in total, the latest ones (the thread from a core -> to a core, us ago):", n);
    let start = n.saturating_sub(32);
    for i in start..n {
        let v = PUSH_LOG[i % 32].load(Ordering::Relaxed);
        let ago = crate::tsc::cycles_to_ns((now << 24).wrapping_sub(v & !0xFF_FFFF) >> 24, crate::tsc::calibrate_cached()) / 1000;
        crate::println!("[push]   t{} c{} -> c{}, {} us ago", (v >> 16) & 0xFF, (v >> 8) & 0xFF, v & 0xFF, ago);
    }
}
/// The donor `cpu` (at a tick, with a second ready task of its own) hands it over to an empty neighbour.
pub fn push_steal(cpu: usize) {
    if !STEAL_PUSH.load(Ordering::Relaxed) { return; }
    let mut cnt = [0u8; MAXCPU];
    runnable_counts(&mut cnt);
    if cnt[cpu.min(MAXCPU - 1)] < 2 { return; }
    let online = sched_cpus();
    // We skip a BSP addressee: its scheduler is woken not by a call but by a tick.
    let Some(c) = (1..MAXCPU).find(|&c| c != cpu && online & (1u64 << c) != 0 && cnt[c] == 0
        && crate::affinity::shared_core(c) && !crate::affinity::is_dedicated(c)) else { return; };
    let n = NREADY.load(Ordering::Relaxed).min(NTASKS);
    let wait = crate::affinity::STEAL_WAIT_NS.load(Ordering::Relaxed);
    let mut best = usize::MAX; let mut best_w = 0u64;
    for t in 0..n {
        if CPU_OF[t].load(Ordering::Relaxed) as usize != cpu || !task_alive(t) || task_pending(t) { continue; }
        if not_ready(t) || is_running(t) || !crate::sync::runnable_peek(t) { continue; }
        if crate::affinity::is_pinned(t) || crate::affinity::allowed_mask(t) & (1u64 << c) == 0 { continue; }
        let w = rq_open_ns(t);
        if w >= wait && w > best_w { best = t; best_w = w; }
    }
    if best == usize::MAX { PUSH_STAT[3].fetch_add(1, Ordering::Relaxed); return; }
    if !migrate_idle_task(best, c) { PUSH_STAT[2].fetch_add(1, Ordering::Relaxed); return; }
    PUSH_STAT[0].fetch_add(1, Ordering::Relaxed);
    push_log(t_pack(best, cpu, c));
    hint_set_on(c, best);
    // `migrate_idle_task` rang the doorbell; a core whose only task
    // sleeps on A WORD does not hear the call — it is woken by an IPI.
    let on = crate::percpu::cur_task_of(c);
    if on != u64::MAX && !crate::sync::sleeps_on_bell(on as usize) {
        PUSH_STAT[1].fetch_add(1, Ordering::Relaxed);
        crate::sync::kick(c as u32);
    }
}
/// An AP core's doorbell: a write wakes its pickup loop (a kwait_once on the line).
#[repr(align(64))]
struct Bell(AtomicU32);
static DOORBELL: [Bell; MAXCPU] = [const { Bell(AtomicU32::new(0)) }; MAXCPU];
/// Wake an AP core's pickup loop (a new task has migrated onto it).
pub fn ap_ring(cpu: usize) {
    DOORBELL[cpu].0.fetch_add(1, Ordering::SeqCst);
    if crate::sync::mode() < crate::sync::MODE_MWAIT { crate::sync::kick(cpu as u32); }
}
/// A core's doorbell as a word to wait on: both the pickup loop sleeps on it and —
/// on A SHARED core — a task waiting on a futex (`sync::wait_on_ex`, WSLEEP=3),
/// so that the wakeup of ANY task of that core is delivered by a write rather than by an IPI.
pub fn doorbell(cpu: usize) -> &'static AtomicU32 { &DOORBELL[cpu.min(MAXCPU - 1)].0 }
/// Is there on the core `cpu` at least one more live task besides `me` (a sleeping one
/// counts too): it is exactly its wakeup that will require reaching that core.
pub fn cpu_has_other_task(cpu: usize, me: usize) -> bool {
    let n = NREADY.load(Ordering::Relaxed);
    for t in 0..n {
        if t == me { continue; }
        // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
        // touches, and what crosses cores is atomic (see the module's note).
        if unsafe { core::ptr::read_volatile(core::ptr::addr_of!(DEAD[t])) } && !dying(t) { continue; }
        if CPU_OF[t].load(Ordering::Relaxed) as usize == cpu { return true; }
    }
    false
}
/// The old kernel stacks of migrated tasks: they may be freed only when the BSP
/// has left them (a limbo context lived on the old stack until the first tick).
static DEAD_KSTK: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS]; // base | pages<<48 | (tid+1)<<56
pub fn queue_dead_kstack(tid: usize, base: u64, pages: u64) {
    for s in DEAD_KSTK.iter() {
        if s.compare_exchange(0, base | pages << 48 | ((tid as u64 + 1) << 56), Ordering::SeqCst, Ordering::SeqCst).is_ok() { return; }
    }
}
/// # Safety
/// C1 + C3: a kernel stack is returned only after its task has stopped for good and is
/// not the current one — a stack freed under a live thread is the hardest crash to read.
unsafe fn reap_dead_kstacks() {
    let cur = CURRENT.load(Ordering::Relaxed);
    for s in DEAD_KSTK.iter() {
        let v = s.load(Ordering::SeqCst);
        if v == 0 { continue; }
        let tid = (v >> 56) as usize - 1;
        if tid == cur { continue; } // the BSP may still be on this stack (limbo)
        if s.compare_exchange(v, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            crate::mem::free_contig(v & 0xFFFF_FFFF_FFFF, (v >> 48) & 0xFF);
        }
    }
}

/// An AP core's pickup loop (PLAN-SYNC): wait for a task with CPU_OF==cpu, enter it.
/// On A DEDICATED core the task owns it until an exit (the timer is not armed; the blockings are
/// in the kernel, futex/mwait). On a core OF THE COMMON POOL a LAPIC timer is armed before the entry,
/// and after that the tasks of that core rotate in a circle in `ap_schedule`.
/// After an exit the task returns control to the loop through ap_reenter (a fresh stack).
pub fn ap_loop(cpu: usize) -> ! {
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe { AP_LOOP_RSP[cpu] = ap_stack_top(cpu); }
    SCHED_CPUS.fetch_or(1 << cpu, Ordering::SeqCst); // the core takes tasks — one may put them here
    loop {
        // The core is idling: a timer tick caught here must not take
        // the previous (already handed over) task for the current one.
        crate::percpu::set_cur_task(u64::MAX);
        // A request to leave the workload's address space (see `aps_to_kernel_cr3`).
        let g = KCR3_REQ.load(Ordering::SeqCst);
        let dead_here = is_dead_as(crate::paging::current_cr3() & !0xFFF);
        if KCR3_ACK[cpu].load(Ordering::SeqCst) != g || dead_here {
            let k = crate::paging::kernel_cr3();
            // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row
            // it touches, and what crosses cores is atomic (see the module's note).
            if k != 0 && crate::paging::current_cr3() != k { unsafe { crate::paging::switch(k); } note_cr3(cpu, k); }
            KCR3_ACK[cpu].store(g, Ordering::SeqCst);
            if dead_here && AS_AP_LEFT.fetch_add(1, Ordering::Relaxed) < 3 {
                crate::println!("sched: core {} is idling — it has left the tables of the finished process onto the kernel's CR3", cpu);
            }
        }
        AP_CR3_PREV[cpu].store(0, Ordering::SeqCst); // here the switch has certainly finished
        crate::gdb::apply_if_changed(cpu);
        // This core's share of L3. Here rather than only at the bring-up: the layout comes
        // from the manifest LATER than the cores are brought up, while a dedicated core has no tick
        // armed at all — there is no other place to reach it.
        crate::cat::apply_local_if_changed(cpu);
        let n = NREADY.load(Ordering::SeqCst);
        // AN EMPTY CORE MUST LOOK AT THE HINT TOO.
        //
        // The mechanism "give the processor to the woken one" ended exactly here.
        // The waker sees a core without a task (`cur_task_of(c) == u64::MAX`), puts
        // a hint and rings the doorbell (`bell_sleeper` in `sync::wake_impl`,
        // `push_steal` — the same way); the pickup loop wakes up and takes THE FIRST BY
        // NUMBER task of its core — without looking either at the hint or at whether the task
        // is ready at all. A woken socket owner with a greater number waited
        // until the core entered the sleeping neighbour, that one rechecked its word, announced itself
        // parked again and yielded: two extra context switches
        // with a reload of CR3 — the very single microseconds in which
        // 89 % of the wakeups stand ("ready->cpu" 5-20 us).
        //
        // The accounting is fixed here too: the slot is taken by a `swap`, so an unsuitable
        // hint does not lie in it forever (see `HINT_STALE`).
        let mut found = None;
        let h = HINT[cpu].0.swap(usize::MAX, Ordering::SeqCst);
        if h < n && CPU_OF[h].load(Ordering::SeqCst) as usize == cpu && !not_ready(h)
            && crate::sync::runnable_peek(h) && ap_claim(h, cpu) {
            HINT_PICKUP.fetch_add(1, Ordering::Relaxed);
            found = Some(h);
        }
        // THE READY ONES GO IN THE FIRST PASS. The scan took a task without asking whether it sleeps
        // on a word: the core entered a sleeping one only for it to yield at once.
        // The second pass is left as it was — a sleeping one is taken by it too, and that
        // is not over-insurance: a wakeup is lost as a matter of course (see `blocked_for_sched`),
        // and entering such a task is the only way to let it recheck the
        // predicate. Only THE ORDER has changed rather than the set of candidates, so
        // the sleep condition below (`pending`) stays the former one.
        if found.is_none() {
            for pass in 0..2 {
                for t in 0..n {
                    if CPU_OF[t].load(Ordering::SeqCst) as usize != cpu || not_ready(t) { continue; }
                    if pass == 0 && !crate::sync::runnable_peek(t) { continue; }
                    if ap_claim(t, cpu) { found = Some(t); break; }
                }
                if found.is_some() { break; }
            }
        }
        // There is no work of its own — take it from an overloaded neighbour (work stealing).
        // Without this the pool's threads stand in a queue on "their own" cores while the neighbouring ones
        // idle: the periodic balancing moves one task once every
        // 32 ms and does not catch up with a burst of work.
        let found = match found {
            Some(t) => Some(t),
            None => match crate::affinity::steal_candidate(cpu) {
                Some(t) if ap_claim_steal(t, cpu) => Some(t),
                _ => None,
            },
        };
        if let Some(t) = found {
            // THE ENTRY INTO A TASK GOES WITH THE INTERRUPTS CLOSED, STARTING WITH `set_cur_task`.
            // `enter_ctx` does a cli itself, but before it the loop lived with IF=1 having already named
            // the task current: a timer tick in that window (several us per a slice of
            // 100-150 us, that is, ~1-3 % of the entries) took THE LOOP's frame for the task's
            // context, put it into `RSP[t]` and switched on. The task's real
            // context was lost; the next entry into it raised the loop's frame,
            // the loop ran on along its own stack over that same frame and a second
            // time entered garbage already — a #BP/#PF in ring 0 with foreign registers,
            // the kernel went silent (session 32: mysql at the ceiling with a string of short
            // connections — every new thread enters through this loop). The iretq
            // will restore the task's IF; there is nothing to wait for here longer than `stack_wait`.
            // SAFETY: C4 — this core's machine state (the registers/MSRs the switch is made of).
            unsafe { core::arch::asm!("cli", options(nomem, nostack)); }
            // The preemption on this core is switched on exactly when the core is given
            // to the common pool. A dedicated core NEVER has a timer armed: a tick in
            // a latency-critical thread is the very thing for whose absence all this was done.
            if crate::affinity::shared_core(cpu) { crate::smp::lapic_timer_init(crate::smp::SLICE_NS); }
            else { crate::smp::lapic_timer_stop(); }
            // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row
            // it touches, and what crosses cores is atomic (see the module's note).
            unsafe {
                crate::percpu::set_cur_task(t as u64);
                note_on_cpu(cpu);
                note_run_start(t);
                crate::percpu::set_sig_newrsp(SIGRSP[t]); // the deferred rsp substitution has arrived with the task
                crate::user::set_rsp0(KSTACK[t]);
                wrmsr(IA32_FS_BASE, FSBASE[t]);
                let cr3 = CR3[t];
                if cr3 != 0 && cr3 != crate::paging::current_cr3() { note_cr3(cpu, cr3); crate::paging::switch(cr3); }
                if FPU_VALID[t] {
                    let area = core::ptr::addr_of!(FPU[t]) as *const u8;
                    let mxcsr = core::ptr::read_volatile(area.add(24) as *const u32);
                    if mxcsr & 0xFFFF_0000 != 0 || area as usize & 0xF != 0 {
                        crate::println!("[ap] t{}: a broken fxsave image mxcsr={:#x} area={:p} — I skip the fxrstor", t, mxcsr, area);
                    } else {
                        fpu_restore(area);
                    }
                }
                stack_wait(t);     // the core that released it may still be standing on its stack
                ctx_enter(t, cpu, RSP[t]);
                enter_ctx(RSP[t]); // pop 15 GP + iretq — it does not return
            }
        }
        let d = DOORBELL[cpu].0.load(Ordering::SeqCst);
        // a recheck after reading the doorbell: a migration could have happened between the scan and the sleep.
        // The condition must COINCIDE with the scan above, including the DEAD check: a task
        // killed from another core (`exit_all`) at the moment when it was preempted
        // and therefore not marked as taken would otherwise look like an eternal waiter — the scan
        // skips it, while the recheck counts it as work, and the core spins in vain.
        let mut pending = false;
        for t in 0..NREADY.load(Ordering::SeqCst) {
            if CPU_OF[t].load(Ordering::SeqCst) as usize == cpu
                && !not_ready(t)
                && !AP_TAKEN[t].load(Ordering::SeqCst) { pending = true; break; }
        }
        if pending { continue; }
        // The core has nothing to execute — this is the very place to pay the debt of PRINTING (the snapshot
        // `talaria.stat=`, 25 KB over the polled UART). From a tick that went with
        // IF=0 and cost the workload up to 400-500 ms — the instrument spoiled what
        // it measured (see `obs::stat_tick`). Here there is no task on the core, IF=1.
        crate::obs::stat_drain();
        // THE STACK IS DRIVEN BY THE ONE THAT HAS NOTHING TO EXECUTE (see `netsys::idle_poll`).
        // Before that an empty core went into a `hlt` until a call, while for an arrived frame
        // there is nobody to call: there are no interrupts from the card in this build. The frame lay in
        // the ring until the tick of the core where its waiter sits.
        if crate::netsys::idle_poll(|| DOORBELL[cpu].0.load(Ordering::Relaxed) != d) { continue; }
        // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
        // touches, and what crosses cores is atomic (see the module's note).
        unsafe { crate::sync::kwait_once(DOORBELL[cpu].0.as_ptr() as *const u32, d); }
    }
}
static AP_TAKEN: [AtomicBool; NTASKS] = [const { AtomicBool::new(false) }; NTASKS];
/// The TSC mark of when the task LAST left a core. It is needed by exactly one
/// decision — whether it may be stolen (`affinity::steal_candidate`): for a task that has just
/// left, the core's cache is still its own, and a move zeroes it twice — for the task itself and
/// for the one it comes to.
static LAST_OFF: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS];
/// When the current task started executing on the core (the TSC). It is needed by exactly one
/// decision — whether it may be preempted for the sake of a just-woken one (`sync::wake_addr`).
static ON_SINCE: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
#[inline]
pub fn note_on_cpu(cpu: usize) { if cpu < MAXCPU { ON_SINCE[cpu].store(crate::tsc::rdtsc(), Ordering::Relaxed); } }
/// How many nanoseconds the current task has already occupied the core `cpu`.
pub fn ran_ns_on(cpu: usize) -> u64 {
    if cpu >= MAXCPU { return 0; }
    let t0 = ON_SINCE[cpu].load(Ordering::Relaxed);
    if t0 == 0 { return u64::MAX; }
    // Through `cycles_to_ns` and NOT by a division by `hz/1e9`: this result is compared
    // with the preemption threshold at a wakeup (150 us), and rounding the frequency to
    // whole gigahertz would shift the threshold by twenty percent.
    crate::tsc::cycles_to_ns(crate::tsc::rdtsc().wrapping_sub(t0), crate::tsc::calibrate_cached())
}

/// The ticks BY CORE: how many times a tick caught the core at a task and how many — idling.
static TICKS_BUSY: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
static TICKS_IDLE: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
/// Is the scheduler's periodic timer armed on the core `c` (by our own
/// table of the initial counters). A core without it does not preempt at all, and telling
/// that from "there is simply no work there" by the number of ticks is impossible.
pub fn cpu_timer_armed(c: usize) -> bool { crate::smp::timer_armed_on(c) }
pub fn cpu_ticks(c: usize) -> (u64, u64) {
    if c >= MAXCPU { return (0, 0); }
    (TICKS_BUSY[c].load(Ordering::Relaxed), TICKS_IDLE[c].load(Ordering::Relaxed))
}
// ── THE TICK'S GAP ─────────────────────────────────────────────────────────
//
// Session 32 (mongodb/insert r10000, the kernel bc32552): the worst "ready -> cpu" was
// 18 ms, in the ring of core 1's decisions there is EMPTINESS for 18 ms, then "a tick t5->t11 @r3".
// The LVT was not occupied (4 borrowings over the whole run), the host took nothing from the core
// (core 1's steal did not move). Which means the tick did not come because the core
// sat with IF=0 for 18 ms: all the syscalls go under FMASK=0x200, and the interrupts
// are returned only by explicit wait points (mwait/hlt/yield). A tick deferred
// for the whole time of such a syscall fires at the very first ring 3 instruction after
// the sysret — exactly "@r3". This instrument catches such a gap AFTER THE FACT: at every
// real tick the distance from the previous one on the same core is measured, the worst one is
// remembered together with who was on the core, with which ring and which syscall
// the task called last (and whether it covered the gap entirely — by the stamps of the entry and
// the exit `syscall::LAST_SYS_T0/T1`). The cost: one rdtsc per tick.
static TICK_LAST: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
static GAP_WORST: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU]; // ns
static GAP_END_MS: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
static GAP_TASK: [AtomicU64; MAXCPU] = [const { AtomicU64::new(u64::MAX) }; MAXCPU];
static GAP_RIP: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];   // the tick's rip, bit 63 = ring 3
static GAP_SYS: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];   // the task's LAST_SYS (nr<<48|rip)
static GAP_ARG0: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
static GAP_SYS_IN: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];  // the syscall's entry: ns before the end of the gap
static GAP_SYS_OUT: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU]; // the syscall's exit: ns before the end of the gap (u64::MAX — it has not left yet)
/// The gaps longer than 1 / 4 / 16 ms on a busy core (idling does not count: there the tick
/// is lengthened on purpose, see `IDLESLICE_NS`).
pub static GAP_N: [[AtomicU64; 3]; MAXCPU] = [const { [const { AtomicU64::new(0) }; 3] }; MAXCPU];
/// HOW MUCH OF THE GAP THE HOST TOOK (session 34).
///
/// The tick's gap says nothing about the only difference that decides here: the core did not
/// tick because WE sat in a long section with IF=0, or because the vCPU
/// was not on the processor at all. It looks the same ("the syscall covered
/// the gap entirely" is written in both cases: the syscall was going on all right), while it is cured by
/// the opposite — in the first case by a yield window in our code, in the second by
/// nothing, it is somebody else's machine. Guessing here is not allowed: scoreboard 33 counted exactly
/// one event of ~8 ms per run with mysql, postgresql and mongodb (smp=2) and NOT
/// A SINGLE one with redis (smp=1), and both hypotheses explain that equally well.
///
/// So at every tick this core's steal time is taken (KVM writes it into
/// our own page, see `smp::steal_ns`), and its increment OVER THE GAP lies next
/// to it. The price is one read of a foreign cache line per tick, and only when
/// there is a steal time at all.
static STEAL_LAST: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
static GAP_STEAL: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];  // the host's ns in the worst gap
/// The gaps >=1 ms where the host took more than half, and how much it took in the gaps in total.
pub static GAP_HOST_N: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
pub static GAP_HOST_NS: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
fn tick_gap_note(c: usize, cur: u64, frame: u64) {
    if c >= MAXCPU { return; }
    let now = crate::tsc::rdtsc();
    let prev = TICK_LAST[c].swap(now, Ordering::Relaxed);
    // The host's mark is taken BEFORE any return: otherwise the increment would be counted
    // from an arbitrarily old mark and would ascribe foreign pauses to the gap.
    let st = crate::smp::steal_ns(c);
    let st_prev = STEAL_LAST[c].swap(st, Ordering::Relaxed);
    let steal = if st == 0 || st_prev == 0 { 0 } else { st.saturating_sub(st_prev) };
    if prev == 0 || cur == u64::MAX || (cur as usize) >= NTASKS { return; }
    let hz = crate::tsc::calibrate_cached();
    if hz == 0 { return; }
    let gap = crate::tsc::cycles_to_ns(now.wrapping_sub(prev), hz);
    // A tick stretched on purpose (a loner, `IDLESLICE_NS`) is not a gap: we measure
    // against THE CURRENT period of this core.
    if gap < 4 * crate::smp::my_period_ns() { return; }
    let steal = steal.min(gap);   // the increment per tick, the gap is a part of it
    if gap >= 1_000_000 {
        GAP_N[c][0].fetch_add(1, Ordering::Relaxed);
        GAP_HOST_NS[c].fetch_add(steal, Ordering::Relaxed);
        if steal * 2 >= gap { GAP_HOST_N[c].fetch_add(1, Ordering::Relaxed); }
    }
    if gap >= 4_000_000 { GAP_N[c][1].fetch_add(1, Ordering::Relaxed); }
    if gap >= 16_000_000 { GAP_N[c][2].fetch_add(1, Ordering::Relaxed); }
    if gap <= GAP_WORST[c].load(Ordering::Relaxed) { return; }
    GAP_WORST[c].store(gap, Ordering::Relaxed);
    GAP_STEAL[c].store(steal, Ordering::Relaxed);
    GAP_END_MS[c].store(crate::tsc::mono_ns() / 1_000_000, Ordering::Relaxed);
    let t = cur as usize;
    GAP_TASK[c].store(cur, Ordering::Relaxed);
    if frame != 0 {
        // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
        // touches, and what crosses cores is atomic (see the module's note).
        let rip = unsafe { core::ptr::read_volatile((frame + 15 * 8) as *const u64) };
        // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
        // touches, and what crosses cores is atomic (see the module's note).
        let cs = unsafe { core::ptr::read_volatile((frame + 16 * 8) as *const u64) };
        GAP_RIP[c].store((rip & 0x7FFF_FFFF_FFFF_FFFF) | if cs & 3 == 3 { 1 << 63 } else { 0 }, Ordering::Relaxed);
    }
    GAP_SYS[c].store(crate::syscall::LAST_SYS[t].load(Ordering::Relaxed), Ordering::Relaxed);
    GAP_ARG0[c].store(crate::syscall::LAST_ARG0[t].load(Ordering::Relaxed), Ordering::Relaxed);
    let t0 = crate::syscall::LAST_SYS_T0[t].load(Ordering::Relaxed);
    let t1 = crate::syscall::LAST_SYS_T1[t].load(Ordering::Relaxed);
    let ago = |x: u64| if x == 0 { u64::MAX } else { crate::tsc::cycles_to_ns(now.wrapping_sub(x), hz) };
    GAP_SYS_IN[c].store(ago(t0), Ordering::Relaxed);
    GAP_SYS_OUT[c].store(if t1.wrapping_sub(t0) as i64 >= 0 { ago(t1) } else { u64::MAX }, Ordering::Relaxed);
}
/// The worst tick gap of the core `c`: (ns, the end in ms from the boot, the task, the tick's rip with bit 63 = ring 3,
/// the last syscall nr<<48|rip, arg0, the syscall's entry N ns before the end, the exit N ns before the end).
/// Reset the core's "worst gap" (after the stat block is printed).
pub fn tick_gap_reset(c: usize) { if c < MAXCPU { GAP_WORST[c].store(0, Ordering::Relaxed); } }
pub fn tick_gap(c: usize) -> (u64, u64, u64, u64, u64, u64, u64, u64) {
    if c >= MAXCPU { return (0, 0, u64::MAX, 0, 0, 0, 0, 0); }
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    (l(&GAP_WORST[c]), l(&GAP_END_MS[c]), l(&GAP_TASK[c]), l(&GAP_RIP[c]), l(&GAP_SYS[c]), l(&GAP_ARG0[c]), l(&GAP_SYS_IN[c]), l(&GAP_SYS_OUT[c]))
}
/// How much of the WORST gap of the core `c` the host took (ns), how many gaps >=1 ms were
/// mostly its own and how much it took in the gaps in total (ns).
pub fn tick_gap_host(c: usize) -> (u64, u64, u64) {
    if c >= MAXCPU { return (0, 0, 0); }
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    (l(&GAP_STEAL[c]), l(&GAP_HOST_N[c]), l(&GAP_HOST_NS[c]))
}
#[inline]
fn note_cpu_tick(c: usize, busy: bool) {
    if c >= MAXCPU { return; }
    crate::pmu::note_tick(); // this core's hardware counters — seven rdpmc once per slice
    if busy { TICKS_BUSY[c].fetch_add(1, Ordering::Relaxed); } else { TICKS_IDLE[c].fetch_add(1, Ordering::Relaxed); }
}
/// Mark that the task left the core (it is called from both schedulers).
#[inline]
pub fn note_left_cpu(t: usize) { if t < NTASKS { LAST_OFF[t].store(crate::tsc::rdtsc(), Ordering::Relaxed); } }
/// How many nanoseconds ago the task left the core (u64::MAX — it never left).
pub fn ns_since_off(t: usize) -> u64 {
    let v = LAST_OFF[t].load(Ordering::Relaxed);
    if v == 0 { return u64::MAX; }
    crate::tsc::cycles_to_ns(crate::tsc::rdtsc().wrapping_sub(v), crate::tsc::calibrate_cached())
}
// ── THE ACCOUNTING OF THE TIME ON THE PROCESSOR (top/ps: "how much CPU a thread eats") ─────
//
// WHY NOT THE TICKS. `TICKS_ON` counts how many times a tick caught a task on a core, and
// on a core OF THE COMMON POOL that is enough. But Talaria's main thread usually lives on a
// DEDICATED core, where the timer is not armed at all (that is the point of a dedicated
// core): there are zero ticks there, and `top` by the ticks would show 0% exactly for the thread for whose
// sake everything is done. So the time is measured in TSC cycles: the accumulated one is in
// `RUN_TSC`, while the current interval is added up AT THE READING (`run_ns`), so that a core without
// a single tick is accounted for just as precisely as a core with a tick.
//
// THE PRICE. One TSC read and one Relaxed add per context switch — that is,
// where an FPU save and a CR3 reload already happen. On the datapath
// (the thread owns the core, there are no switches) the price is EXACTLY ZERO.
static RUN_TSC: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS];
/// How many times the task was taken off a core (the context switches "from it").
static NSWITCH: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS];
/// The context switches on a core — the sum over all its tasks plus the departures into idling.
static CPU_SWITCH: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
/// The moment from which the slot is occupied by the current task (the TSC): "the thread's uptime".
static BORN: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS];

/// THE CORE's busy time: the same thing but from the other side. A sum over the tasks does not
/// do — a task migrates, and its time would move away with it, while the question
/// "did core 3 work" is asked exactly about the core.
static CPU_RUN: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];

/// Accumulate the time spent by the task `t` on the core `cpu` and start a new count.
/// It is called from both schedulers BEFORE the next task is chosen.
#[inline]
pub fn acct_run(cpu: usize, t: usize) {
    if cpu >= MAXCPU || t >= NTASKS { return; }
    let t0 = ON_SINCE[cpu].load(Ordering::Relaxed);
    if t0 == 0 { return; }
    let now = crate::tsc::rdtsc();
    let d = now.wrapping_sub(t0);
    RUN_TSC[t].fetch_add(d, Ordering::Relaxed);
    CPU_RUN[cpu].fetch_add(d, Ordering::Relaxed);
    ON_SINCE[cpu].store(now, Ordering::Relaxed);
}
/// The core goes into a HALT/MWAIT: the interval is closed, and the sleep's time goes to
/// NOBODY — neither to the task nor to the core.
///
/// Without that `top`/`ps` lied exactly where people look at them: a futex sleeps in the kernel
/// with an mwait WITHOUT leaving the core (see `sync::kwait_once`), so a sleeping thread
/// stayed the current task of its core, and the whole sleep was written down to it as
/// "the processor". The writer thread of `examples/mdpipe`, which makes five writes a
/// second, showed 96 % CPU — and the occupancy of its core was the same.
pub fn halt_acct_enter(cpu: usize) {
    if cpu >= MAXCPU { return; }
    let t = crate::percpu::cur_task_of(cpu);
    if t != u64::MAX && (t as usize) < NTASKS { acct_run(cpu, t as usize); }
    ON_SINCE[cpu].store(0, Ordering::Relaxed);
}
/// The core has woken up: open a new interval (the time belongs to the task again).
pub fn halt_acct_exit(cpu: usize) {
    if cpu >= MAXCPU { return; }
    ON_SINCE[cpu].store(crate::tsc::rdtsc(), Ordering::Relaxed);
}

/// The nanoseconds worked by the core `c` under tasks (with the current interval).
pub fn cpu_busy_ns(c: usize) -> u64 {
    if c >= MAXCPU { return 0; }
    let mut v = CPU_RUN[c].load(Ordering::Relaxed);
    let busy = crate::percpu::cur_task_of(c) != u64::MAX && !(c == 0 && BSP_IDLE.load(Ordering::Relaxed));
    if busy {
        let t0 = ON_SINCE[c].load(Ordering::Relaxed);
        if t0 != 0 { v = v.wrapping_add(crate::tsc::rdtsc().wrapping_sub(t0)); }
    }
    crate::tsc::cycles_to_ns(v, crate::tsc::calibrate_cached())
}
/// Mark the context switch `from` -> `to` on the core `cpu`.
#[inline]
pub fn acct_switch(cpu: usize, from: usize) {
    if from < NTASKS { NSWITCH[from].fetch_add(1, Ordering::Relaxed); }
    if cpu < MAXCPU { CPU_SWITCH[cpu].fetch_add(1, Ordering::Relaxed); }
}
/// Mark the birth of a task in a slot (the slots are reused — the mark is updated).
#[inline]
pub fn acct_born(t: usize) {
    if t >= NTASKS { return; }
    BORN[t].store(crate::tsc::rdtsc(), Ordering::Relaxed);
    RUN_TSC[t].store(0, Ordering::Relaxed);
    NSWITCH[t].store(0, Ordering::Relaxed);
    TICKS_ON[t].store(0, Ordering::Relaxed);
    READY_TSC[t].store(0, Ordering::Relaxed); RQ_TSC[t].store(0, Ordering::Relaxed);
    IO_YIELD[t].store(false, Ordering::Relaxed); IOY_OFF[t].store(0, Ordering::Relaxed); IOY_TSC[t].store(0, Ordering::Relaxed);
    crate::metrics::forget_task(t);
}

// ── READY BUT NOT RUNNING (the queue to the processor) ───────────────────────
//
// WHAT WAS MISSING. The histogram "woken -> ran" (`sync::WAKE_LAT`) sees
// only AN EXPLICIT wakeup and only up to the first exit from a wait. A task
// that was PREEMPTED by the quantum and not returned to a core is not listed in it at all, while
// that is exactly ClickHouse's question (session 25): "there are four ready threads, three cores
// under them" — how long did each of them STAND ready? `ns_since_off` does not answer it
// either: it is about how long ago the task left a core rather than about whether it was
// ready all that time.
//
// THE COUNT. `READY_TSC[t]` is the moment from which the task is ready and NOT on a core; it is set
// (a) by an explicit wakeup (`sync::wake_impl`), (b) by the publication of a new task,
// (c) by a departure from a core if the task is not sleeping meanwhile (a preemption, a `sched_yield`),
// (d) by the scheduler when it sees a waiter with a fulfilled condition / an expired
// term / a signal (`sync::blocked_for_sched`). Going to sleep ZEROES the mark.
// At the landing on a core the difference goes into `RQ_TSC[t]` and into the common histogram.
// The price is one TSC read per switch, in the same place where `RUN_TSC` is already computed;
// on the datapath without switches it is zero.
static READY_TSC: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS];
static RQ_TSC: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS];
/// The histogram over the machine, us: <5, <20, <50, <100, <200, <500, <1000, 1000+.
pub static RQ_HIST: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
pub static RQ_SUM_US: AtomicU64 = AtomicU64::new(0);
pub static RQ_MAX_US: AtomicU64 = AtomicU64::new(0);
/// Who waited the longest (the task's number) and on which core it stood: "the worst 525 ms"
/// without a name does not say whether a network waiter starved by design or a workload's thread did.
pub static RQ_MAX_TASK: AtomicU64 = AtomicU64::new(u64::MAX);
/// THE CIRCUMSTANCES of the worst interval — without them the number cannot be judged. "conn17 waited
/// 1167 ms on core 0" with a max service latency of 22 ms over the same run
/// (session 31) means either "it was not on a request" (a closing of a connection) or
/// "the instrument lies", and there is nothing to tell one from the other. What is written: the moment when
/// the wait ENDED (ms from the boot — to be verified against the run's window), who ran on
/// the waiter's core at the moment of the readiness (a spinner that did not yield the core — or
/// nobody: the core stood in a hlt, the wakeup was lost) and how many switches
/// that core made during the wait (zero — the core did not switch at all).
pub static RQ_MAX_AT_MS: AtomicU64 = AtomicU64::new(0);
pub static RQ_MAX_CTX: AtomicU64 = AtomicU64::new(0);
/// THE LATEST DECISIONS OF THE WAITER'S CORE before the worst interval ended
/// (session 32, goal 2). The line `at the moment of readiness the core was executing t10, 4 switches`
/// over 844 ms says that the tick decided "we stay" a thousand and a half times in a row — but not
/// WHY (a lock / nobody / it ran little) and WHERE the occupant was (ring 3 or an address
/// in the kernel). The ring `STRACE` knows that; we take its tail at a new worst one
/// longer than `RQ_MAX_EV_US`. The low 8 bits of `RQ_MAX_EV_N_GOT` are how many records there are,
/// the higher ones are the cycle of the taking (the negative offsets are printed from it).
pub const RQ_MAX_EV_US: u64 = 10_000;
pub const RQ_MAX_EV_N: usize = 8;
pub static mut RQ_MAX_EV: [SchedEv; RQ_MAX_EV_N] = [SEV0; RQ_MAX_EV_N];
pub static RQ_MAX_EV_N_GOT: AtomicU64 = AtomicU64::new(0);      // the low 16 bits — the task on the core at the moment of readiness (0xFFFF — idle), the higher ones — the core's switches during the wait
/// A snapshot at the moment of readiness: the task on the waiter's core | (the core's switch counter << 16).
static READY_CTX: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS];
#[inline]
fn ready_ctx_now(t: usize) -> u64 { core_ctx_of(t) }
/// A snapshot of the core of the task `t` right now: who is on it (0xFFFF — idle) |
/// its switch counter << 16. The delivery's instrument (`netsys::dl_class`) takes
/// it at the moment of the delivery and by it counts the switches until the pickup.
#[inline]
pub fn core_ctx_of(t: usize) -> u64 {
    let c = cpu_of(t);
    let cur = crate::percpu::cur_task_of(c);
    let cur = if cur == u64::MAX || cur >= NTASKS as u64 { 0xFFFF } else { cur & 0xFFFF };
    cur | (CPU_SWITCH[c.min(MAXCPU - 1)].load(Ordering::Relaxed) << 16)
}
// A YIELD TO THE DEVICE IS NOT A WAIT FOR THE PROCESSOR. The one waiting for a disk barrier gives the core
// to a neighbour (`fs::io_wait_step`) and from the scheduler's point of view is ready: `waiting_on`
// is empty. But what it waits for is not the processor but THE DISK, and the metric "ready->cpu"
// ascribed the whole barrier to it: with WTJourn.Flusher under a gp3 throttle "it waited
// 10.7 s" out of a 24 s run, the worst interval was 561 ms — exactly the barrier's length, and
// real starvation (one that is not seated on a core although it is ready) drowned in that noise.
// While the flag is raised, a departure from a core opens an interval in `IOY_TSC` rather than in `RQ_TSC`.
static IO_YIELD: [AtomicBool; NTASKS] = [const { AtomicBool::new(false) }; NTASKS];
static IOY_OFF: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS];
static IOY_TSC: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS];
/// The current task yields the core while waiting for the device: do not count it as ready.
pub fn io_yield_begin() { let t = current_id(); if t < NTASKS { IO_YIELD[t].store(true, Ordering::Relaxed); } }
pub fn io_yield_end() { let t = current_id(); if t < NTASKS { IO_YIELD[t].store(false, Ordering::Relaxed); } }
/// The nanoseconds the task spent off the core, having yielded it while waiting for the device.
pub fn ioy_ns(t: usize) -> u64 {
    if t >= NTASKS { return 0; }
    crate::tsc::cycles_to_ns(IOY_TSC[t].load(Ordering::Relaxed), crate::tsc::calibrate_cached())
}
/// The task has become ready (a wakeup / a publication / the condition is fulfilled): start
/// the count if it is not going yet.
#[inline]
pub fn note_ready(t: usize) {
    // The read comes first: it is called from the scheduler's walk over the candidates on every core,
    // and there the count is almost always going already — an RMW on a shared line is not needed.
    if t < NTASKS && READY_TSC[t].load(Ordering::Relaxed) == 0 {
        if READY_TSC[t].compare_exchange(0, crate::tsc::rdtsc(), Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            READY_CTX[t].store(ready_ctx_now(t), Ordering::Relaxed);
        }
    }
}
/// The task has left a core: if it is not sleeping, from this moment it waits for the processor,
/// if it is sleeping, there is no count (a wakeup will start it).
#[inline]
fn note_ready_off(t: usize) {
    if t >= NTASKS { return; }
    // A dead task waits for nothing: otherwise the last departure from a core would open
    // an interval that nobody would close, and `top` would show WAITED% for a corpse.
    if IO_YIELD[t].load(Ordering::Relaxed) {
        READY_TSC[t].store(0, Ordering::Relaxed);
        IOY_OFF[t].store(crate::tsc::rdtsc(), Ordering::Relaxed);
        return;
    }
    let v = if task_alive(t) && crate::sync::waiting_on(t) == 0 { crate::tsc::rdtsc() } else { 0 };
    READY_TSC[t].store(v, Ordering::Relaxed);
    if v != 0 { READY_CTX[t].store(ready_ctx_now(t), Ordering::Relaxed); }
}
/// The task has sat down on a core: close the wait's interval.
#[inline]
fn note_run_start(t: usize) {
    if t >= NTASKS { return; }
    let o = IOY_OFF[t].swap(0, Ordering::Relaxed);
    if o != 0 { IOY_TSC[t].fetch_add(crate::tsc::rdtsc().wrapping_sub(o), Ordering::Relaxed); }
    let r = READY_TSC[t].swap(0, Ordering::Relaxed);
    if r == 0 { return; }
    let d = crate::tsc::rdtsc().wrapping_sub(r);
    RQ_TSC[t].fetch_add(d, Ordering::Relaxed);
    let us = d.saturating_mul(1_000_000) / crate::tsc::calibrate_cached().max(1);
    RQ_SUM_US.fetch_add(us, Ordering::Relaxed);
    if us > RQ_MAX_US.load(Ordering::Relaxed) {
        RQ_MAX_US.store(us, Ordering::Relaxed);
        RQ_MAX_TASK.store(t as u64 | (CPU_OF[t].load(Ordering::Relaxed) as u64) << 32, Ordering::Relaxed);
        RQ_MAX_AT_MS.store(crate::tsc::mono_ns() / 1_000_000, Ordering::Relaxed);
        let ctx = READY_CTX[t].load(Ordering::Relaxed);
        let c = cpu_of(t);
        let sw = CPU_SWITCH[c.min(MAXCPU - 1)].load(Ordering::Relaxed).wrapping_sub(ctx >> 16);
        RQ_MAX_CTX.store((ctx & 0xFFFF) | (sw << 16), Ordering::Relaxed);
        // The tail of the waiter's core's decisions — how it was ending this interval (see `RQ_MAX_EV`).
        if us >= RQ_MAX_EV_US && c < MAXCPU {
            let end = STRACE_POS[c].load(Ordering::Relaxed);
            let from = end.saturating_sub(RQ_MAX_EV_N).max(end.saturating_sub(STRACE_N));
            let mut n = 0;
            // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row
            // it touches, and what crosses cores is atomic (see the module's note).
            for k in from..end { unsafe { RQ_MAX_EV[n] = STRACE[c][k % STRACE_N]; } n += 1; }
            RQ_MAX_EV_N_GOT.store(n as u64 | (crate::tsc::rdtsc() << 8), Ordering::Relaxed);
        }
    }
    let b = match us { 0..=4 => 0, 5..=19 => 1, 20..=49 => 2, 50..=99 => 3,
                       100..=199 => 4, 200..=499 => 5, 500..=999 => 6, _ => 7 };
    RQ_HIST[b].fetch_add(1, Ordering::Relaxed);
}
/// THE CORES RIGHT NOW AND THE STARVING ONES — into the `stat:` block (see metrics.rs).
///
/// It was introduced for session 32's hang (the kernel 277c736 on the mysql stand): the thread t11
/// "ran" on core 1 for a hundred and twenty seconds in a row, t14 on the same core "waited"
/// just as long, the TLB flushes to the neighbour stopped being acknowledged, while in the stat block
/// there was NOTHING that would say WHERE core 1 was: in ring 3, in a network waiter's spin,
/// in a lock or inside the scheduler. It prints for every core of the scheduler
/// the current task, its switch counter, the age and the essence of the scheduler's last
/// decision, the last phase of that task's network wait (see
/// `netsys::NW_PH`); and for every task that has been standing ready longer than `STUCK_MS` —
/// everything by which the scheduler decides whether to take it or not.
/// A SNAPSHOT OF A CORE BY AN NMI (see `dump_cores`): the NMI handler only writes here,
/// the printing is done by the core that asked. `seq` grows at every snapshot.
pub struct NmiSnap { pub seq: AtomicU64, pub rip: AtomicU64, pub cs: AtomicU64, pub rflags: AtomicU64, pub rsp: AtomicU64, pub task: AtomicU64, pub stack: [AtomicU64; 12] }
pub static NMI_SNAP: [NmiSnap; MAXCPU] = [const { NmiSnap { seq: AtomicU64::new(0), rip: AtomicU64::new(0), cs: AtomicU64::new(0), rflags: AtomicU64::new(0), rsp: AtomicU64::new(0), task: AtomicU64::new(0), stack: [const { AtomicU64::new(0) }; 12] } }; MAXCPU];
/// From the NMI handler: remember where the core is (without printing and without locks).
pub fn nmi_note(rip: u64, cs: u64, rflags: u64, rsp: u64) {
    let cpu = crate::percpu::cpu_id();
    if cpu >= MAXCPU { return; }
    let s = &NMI_SNAP[cpu];
    s.rip.store(rip, Ordering::Relaxed); s.cs.store(cs, Ordering::Relaxed);
    s.rflags.store(rflags, Ordering::Relaxed); s.rsp.store(rsp, Ordering::Relaxed);
    s.task.store(crate::percpu::cur_task(), Ordering::Relaxed);
    let kstack = cs & 3 == 0 && (0x1000..0x4000_0000_0000).contains(&rsp);
    for (i, w) in s.stack.iter().enumerate() {
        // SAFETY: C3 — the interrupt/syscall FRAME of the thread this core interrupted: it lies
        // on that thread's kernel stack, which is alive for as long as the thread is, and the
        // offsets are the ones the stub pushed.
        w.store(if kstack { unsafe { core::ptr::read_volatile((rsp + i as u64 * 8) as *const u64) } } else { 0 }, Ordering::Relaxed);
    }
    s.seq.fetch_add(1, Ordering::SeqCst);
}
pub fn dump_cores() {
    const STUCK_MS: u64 = 200;
    let hz = crate::tsc::calibrate_cached().max(1);
    crate::println!("stat: a foreign stack at the entry into a task: we waited {} times, the worst {} ns, longer than 10 ms {}; the BSP's ticks in the move's window (a foreign CPU_OF with a live task) {}",
        STACK_WAITS.load(Ordering::Relaxed), STACK_WAIT_MAX_NS.load(Ordering::Relaxed), STACK_WAIT_LONG.load(Ordering::Relaxed),
        BSP_SAVE_FOREIGN.load(Ordering::Relaxed));
    crate::println!("stat: the tasks' frames: entries into a used frame {}, non-frames at RSP[t] {}, ticks on the pickup loop's stack with a task {}; the mark was removed (the task moved away between the check and the swap) {}, a steal backed off before the BSP {}",
        CTX_STALE.load(Ordering::Relaxed), CTX_BAD.load(Ordering::Relaxed), LOOP_TICKS.load(Ordering::Relaxed),
        CLAIM_MOVED.load(Ordering::Relaxed), STEAL_BSP_BUSY.load(Ordering::Relaxed));
    let now = crate::tsc::rdtsc();
    let us = |c: u64| c.saturating_mul(1_000_000) / hz;
    let mask = SCHED_CPUS.load(Ordering::Relaxed);
    for c in 0..MAXCPU {
        if mask & (1u64 << c) == 0 { continue; }
        let cur = crate::percpu::cur_task_of(c);
        crate::print!("stat: core {} right now:", c);
        if cur == u64::MAX || cur >= NTASKS as u64 { crate::print!(" empty"); }
        else {
            let t = cur as usize;
            crate::print!(" t{} «{}»{}", t, crate::obs::comm(t).as_str(),
                if crate::netsys::is_net_waiter(t) { " (a network waiter)" } else { "" });
            let mut ph = [crate::netsys::PHE0; 1];
            let pos = crate::netsys::NW_PH_POS[t].load(Ordering::Relaxed) as usize;
            if pos != 0 {
                // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose
                // row it touches, and what crosses cores is atomic (see the module's note).
                ph[0] = unsafe { crate::netsys::NW_PH[t][(pos - 1) % crate::netsys::PH_N] };
                crate::print!(", the phase {}x{} {} us ago",
                    crate::netsys::PH_NAMES[(ph[0].ph as usize).min(crate::netsys::PH_NAMES.len() - 1)], ph[0].n, us(now.wrapping_sub(ph[0].tsc)));
            }
        }
        crate::print!(", switches {}, the hint {}, the tick left it (tickmin) {}, left it (reqfirst) {}", CPU_SWITCH[c].load(Ordering::Relaxed),
            match HINT[c].0.load(Ordering::Relaxed) { usize::MAX => 255, h => h }, TICKMIN_STAY[c].load(Ordering::Relaxed),
            REQFIRST_STAY[c].load(Ordering::Relaxed));
        let mut ev = [SEV0; 1];
        let n = strace_take(c, now, 1, &mut ev);
        if n != 0 {
            let e = ev[0];
            let why = match e.why & 0x7f { 1 => "hint", 2 => "a foreign hint->rotation", 3 => "rotation", 4 => "stay:lock", 5 => "stay:nobody", 6 => "stay:interception", 7 => "steal", 8 => "stay:it ran little", 9 => "stay:a request in progress", _ => "?" };
            crate::print!(", the decision {} us ago: {} t{}->t{} {}", us(now.wrapping_sub(e.tsc)),
                if e.why & SW_TICK != 0 { "tick" } else { "yield" }, e.from, e.to, why);
            if e.why & SW_TICK != 0 { if e.rip == 3 { crate::print!(" @r3"); } else if e.rip != 0 { crate::print!(" @{:#x}", e.rip); } }
            else if e.rip != 0 { crate::print!(" from:{}", YO_NAMES[(e.rip as usize).min(YO_NAMES.len() - 1)]); }
        }
        crate::println!();
        // THE CORE MAKES NO DECISIONS FOR LONGER THAN STUCK_MS WITH A LIVE TASK — which means it does not
        // take interrupts either (a tick would have come). The only way to ask it
        // where it is is an NMI (the IF mask is no law for it). The handler prints nothing —
        // it puts a snapshot into NMI_SNAP, while we print it once we have waited for the number.
        if cur != u64::MAX && n != 0 && us(now.wrapping_sub(ev[0].tsc)) >= STUCK_MS * 1000 && c != crate::percpu::cpu_id() {
            let snap = &NMI_SNAP[c];
            let seq0 = snap.seq.load(Ordering::SeqCst);
            crate::smp::send_nmi(c);
            let t0 = crate::tsc::rdtsc();
            while snap.seq.load(Ordering::SeqCst) == seq0 && crate::tsc::rdtsc().wrapping_sub(t0) < hz / 500 { core::hint::spin_loop(); }
            if snap.seq.load(Ordering::SeqCst) == seq0 {
                crate::println!("stat:   an NMI to core {}: there is no answer within 2 ms", c);
            } else {
                let rfl = snap.rflags.load(Ordering::Relaxed);
                crate::print!("stat:   an NMI to core {}: rip={:#x} cs={:#x} rflags={:#x} (IF={}) rsp={:#x} the task t{} the stack:",
                    c, snap.rip.load(Ordering::Relaxed), snap.cs.load(Ordering::Relaxed), rfl, (rfl >> 9) & 1,
                    snap.rsp.load(Ordering::Relaxed), snap.task.load(Ordering::Relaxed) as i64);
                for w in snap.stack.iter() { crate::print!(" {:#x}", w.load(Ordering::Relaxed)); }
                crate::println!();
            }
        }
    }
    let n = NREADY.load(Ordering::Relaxed);
    for t in 0..n {
        let r = READY_TSC[t].load(Ordering::Relaxed);
        if r == 0 || !task_alive(t) || running_on(t).is_some() { continue; }
        let ms = us(now.wrapping_sub(r)) / 1000;
        if ms < STUCK_MS { continue; }
        crate::println!("stat: t{} \"{}\" is starving: it has been ready for {} ms, the core {}, AP_TAKEN={}, not_ready={}, runnable={}, the word={:#x}, IO_YIELD={}, PENDING={}",
            t, crate::obs::comm(t).as_str(), ms, cpu_of(t), AP_TAKEN[t].load(Ordering::Relaxed), not_ready(t),
            crate::sync::runnable_peek(t), crate::sync::waiting_on(t), IO_YIELD[t].load(Ordering::Relaxed), PENDING[t].load(Ordering::Relaxed));
    }
}
/// The nanoseconds the task spent READY and not on a core, including the current
/// open interval (otherwise a task stuck in the queue would show zero).
pub fn rq_ns(t: usize) -> u64 {
    if t >= NTASKS { return 0; }
    let mut c = RQ_TSC[t].load(Ordering::Relaxed);
    let r = READY_TSC[t].load(Ordering::Relaxed);
    if r != 0 && task_alive(t) && running_on(t).is_none() { c = c.wrapping_add(crate::tsc::rdtsc().wrapping_sub(r)); }
    crate::tsc::cycles_to_ns(c, crate::tsc::calibrate_cached())
}

/// The current OPEN interval of the readiness of the task `t`, ns: how long it has been waiting for
/// the processor right now (0 — it is not waiting). The threshold of a steal's cooling by this
/// number (`talaria.stealbal=5`) tells "it waits for its turn" from "it starves".
pub fn rq_open_ns(t: usize) -> u64 {
    if t >= NTASKS { return 0; }
    let r = READY_TSC[t].load(Ordering::Relaxed);
    if r == 0 || running_on(t).is_some() { return 0; }
    crate::tsc::cycles_to_ns(crate::tsc::rdtsc().wrapping_sub(r), crate::tsc::calibrate_cached())
}
/// Is the task `t` running right now (and on which core).
pub fn running_on(t: usize) -> Option<usize> {
    let c = cpu_of(t);
    if c < MAXCPU && crate::percpu::cur_task_of(c) == t as u64 { Some(c) } else { None }
}
/// The nanoseconds of processor time of the task, INCLUDING the current interval.
/// The current interval is added up here rather than by a tick — otherwise a dedicated core
/// (there is no timer) would forever show 0.
pub fn run_ns(t: usize) -> u64 {
    if t >= NTASKS { return 0; }
    let mut c = RUN_TSC[t].load(Ordering::Relaxed);
    if let Some(cpu) = running_on(t) {
        let t0 = ON_SINCE[cpu].load(Ordering::Relaxed);
        if t0 != 0 { c = c.wrapping_add(crate::tsc::rdtsc().wrapping_sub(t0)); }
    }
    crate::tsc::cycles_to_ns(c, crate::tsc::calibrate_cached())
}
/// The same in TSC cycles — for the stages that count in cycles themselves (`netprof`).
pub fn run_cycles(t: usize) -> u64 {
    if t >= NTASKS { return 0; }
    let mut c = RUN_TSC[t].load(Ordering::Relaxed);
    if let Some(cpu) = running_on(t) {
        let t0 = ON_SINCE[cpu].load(Ordering::Relaxed);
        if t0 != 0 { c = c.wrapping_add(crate::tsc::rdtsc().wrapping_sub(t0)); }
    }
    c
}
/// How many nanoseconds the slot has been occupied by this task (the thread's uptime).
pub fn age_ns(t: usize) -> u64 {
    if t >= NTASKS { return 0; }
    let b = BORN[t].load(Ordering::Relaxed);
    if b == 0 { return crate::tsc::mono_ns(); }
    crate::tsc::cycles_to_ns(crate::tsc::rdtsc().wrapping_sub(b), crate::tsc::calibrate_cached())
}
/// The context switches of a task and of a core.
pub fn switches(t: usize) -> u64 { if t < NTASKS { NSWITCH[t].load(Ordering::Relaxed) } else { 0 } }
pub fn cpu_switches(c: usize) -> u64 { if c < MAXCPU { CPU_SWITCH[c].load(Ordering::Relaxed) } else { 0 } }
/// The context switches in total over the machine.
pub fn switches_total() -> u64 { (0..MAXCPU).map(cpu_switches).sum() }

/// The task's state for `ps`/`top`: 0 dead, 1 being assembled, 2 running,
/// 3 ready, 4 sleeping on a futex word.
pub fn state_of(t: usize) -> u8 {
    if t >= NTASKS { return 0; }
    if !task_alive(t) { return 0; }
    if PENDING[t].load(Ordering::Relaxed) { return 1; }
    if running_on(t).is_some() { return 2; }
    if crate::sync::waiting_on(t) != 0 { return 4; }
    3
}

/// How many times an idling core took work from a neighbour (it is visible in `stat:`).
pub static STOLEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many tasks on the core `cpu` are ready to run (not dead, not sleeping on a futex).
pub fn runnable_on(cpu: usize) -> usize {
    let n = NREADY.load(Ordering::SeqCst);
    let mut k = 0;
    for t in 0..n {
        if CPU_OF[t].load(Ordering::SeqCst) as usize != cpu { continue; }
        if not_ready(t) { continue; }
        if crate::sync::blocked_for_sched(t) { continue; }
        k += 1;
    }
    k
}
/// How many ready ones there are on EVERY core — in ONE walk over the task table.
///
/// A separate function rather than `runnable_on` in a loop over the cores: that one costs a walk per
/// every core AND spins the skip counter inside `blocked_for_sched`. The version
/// that called it per every core at every wakeup was directly harmful —
/// a `LIKE` over a table went from 12 ms to 41 (session 23). Here the predicate is
/// `runnable_peek`, it has no side effects.
pub fn runnable_counts(out: &mut [u8; MAXCPU]) {
    *out = [0; MAXCPU];
    let n = NREADY.load(Ordering::Relaxed);
    for t in 0..n {
        let c = CPU_OF[t].load(Ordering::Relaxed) as usize;
        if c >= MAXCPU { continue; }
        if not_ready(t) { continue; }
        if !crate::sync::runnable_peek(t) { continue; }
        out[c] = out[c].saturating_add(1);
    }
}
static mut AP_LOOP_RSP: [u64; MAXCPU] = [0; MAXCPU];
/// The top of a core's pickup loop's stack (it is set on the BSP in `smp::start_ap` before the INIT).
/// Previously it was a static array `[[u8; 16384]; MAXCPU]`: with MAXCPU=4 it
/// cost 64 KB, with 64 a megabyte of .bss on EVERY machine, including single-core ones.
static AP_STACK: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
pub fn set_ap_stack(cpu: usize, top: u64) { if cpu < MAXCPU { AP_STACK[cpu].store(top, Ordering::SeqCst); } }
fn ap_stack_top(cpu: usize) -> u64 { AP_STACK[cpu.min(MAXCPU - 1)].load(Ordering::SeqCst) & !0xF }
/// An AP's return into the pickup loop after a task's exit: a fresh stack, the loop anew.
#[unsafe(no_mangle)]
pub extern "C" fn ap_reenter() -> ! { ap_loop(crate::percpu::cpu_id()) }
/// Leave a dead task into the AP's pickup loop (the task's stack is no longer needed).
///
/// # Safety
/// C2 + C3: it switches to this AP's OWN stack and never returns, so the caller must
/// have nothing left on the stack it came in on, and `cpu` must be this core.
unsafe fn ap_exit_to_loop(cpu: usize) -> ! {
    let top = ap_stack_top(cpu);
    core::arch::asm!("mov rsp, {}", "call ap_reenter", in(reg) top, options(noreturn));
}
/// Enter a saved context (like the tail of timer_stub_sched): 15 GP + an iretq.
///
/// # Safety
/// C2 + C3: `sp` must point at a frame this core SAVED for a task it has claimed (15
/// general-purpose registers plus the iretq frame). Entering an unclaimed context means
/// two cores in one thread; entering a frame that is not one means an iretq into
/// garbage. It does not return — the stack it was called on is abandoned.
unsafe fn enter_ctx(sp: u64) -> ! {
    // cli — as in start(): the entry must be atomic with respect to the ticks/IPIs
    core::arch::asm!(
        "cli",
        "mov rsp, {}",
        "pop r15", "pop r14", "pop r13", "pop r12", "pop r11", "pop r10", "pop r9", "pop r8",
        "pop rbp", "pop rdi", "pop rsi", "pop rdx", "pop rcx", "pop rbx", "pop rax",
        "iretq",
        in(reg) sp, options(noreturn)
    );
}

/// A migration of THE CURRENT task (from its syscall) to the core cpu: build a fresh
/// context of the return from the syscall (rax = gp[0]) on a new kernel stack and hand
/// the task to the target core. After that the original core leaves this context:
///   * the BSP goes into limbo (the scheduler does not save the RSP of the tasks with CPU_OF!=0 and does not
///     choose them, a tick will take it away from here forever);
///   * an AP goes straight into its pickup loop: there is nothing to wait for a tick for, while on a dedicated
///     core there will be no tick at all.
/// fpu is the fxsave area of the current syscall (the user state before the entry).
/// # Safety
/// C1 + C2 + C3: it is called from the CURRENT task's syscall path; `rip`/`ursp`/
/// `rflags`/`gp`/`fpu` must be that task's user state at the entry, and `cpu` an online
/// core allowed by the task's mask. It does not return: the task continues on the target
/// core.
pub unsafe fn migrate_current_to(cpu: usize, rip: u64, ursp: u64, rflags: u64, gp: [u64; 15], fpu: u64) -> ! {
    let t = current_id();
    let from = crate::percpu::cpu_id();
    let kpages = 4u64;
    let kstack = crate::mem::alloc_contig(kpages).expect("migrate kstack") + kpages * 4096;
    let mut sp = kstack & !0xF;
    let push = |sp: &mut u64, v: u64| { *sp -= 8; core::ptr::write(*sp as *mut u64, v); };
    push(&mut sp, 0x23); push(&mut sp, ursp); push(&mut sp, rflags | 0x202); push(&mut sp, 0x2B); push(&mut sp, rip);
    for k in 0..15 { push(&mut sp, gp[k]); } // gp[0]=rax is pushed first (popped last)
    let old_kstack = KSTACK[t]; let old_pages = KSZ[t];
    RSP[t] = sp; KSTACK[t] = kstack; KSZ[t] = kpages; ctx_saved(t);
    FSBASE[t] = rdmsr(IA32_FS_BASE);
    set_fpu(t, fpu);
    if old_kstack != 0 && old_pages != 0 { queue_dead_kstack(t, old_kstack - old_pages * 4096, old_pages); }
    AP_TAKEN[t].store(false, Ordering::SeqCst); // nobody executes the task any more
    if from == 0 { BSP_LIMBO.store(true, Ordering::SeqCst); } // the BSP's tick must not save this context
    core::sync::atomic::fence(Ordering::SeqCst);
    CPU_OF[t].store(cpu as u8, Ordering::SeqCst); // from this moment the task belongs to the target core
    if cpu != 0 { ap_ring(cpu); }
    crate::affinity::note_migration();
    if from != 0 {
        crate::percpu::set_cur_task(u64::MAX);
        ap_exit_to_loop(from); // an AP: the task's context is no longer ours — back into the pickup loop
    }
    // limbo: the BSP timer will take the scheduler away from this context and will never come back
    loop {
        crate::paging::halt_enter();
        core::arch::asm!("sti; hlt", options(nostack, preserves_flags));
        crate::paging::halt_exit();
    }
}

/// The leader of the process the task t belongs to.
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn proc_of(t: usize) -> usize { unsafe { core::ptr::read_volatile(core::ptr::addr_of!(PROC[t.min(NTASKS - 1)])) as usize } }
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn ppid_of(p: usize) -> i32 { unsafe { core::ptr::read_volatile(core::ptr::addr_of!(PPID[p])) } }
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn set_ppid(p: usize, pp: i32) { unsafe { PPID[p] = pp; } }
/// The number of live tasks of the process p (the leader + the threads).
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn proc_task_count(p: usize) -> usize { let n = NREADY.load(Ordering::SeqCst); let mut c = 0; for i in 0..n { if unsafe { PROC[i] as usize == p && !DEAD[i] } { c += 1; } } c }

/// wait4: find a finished (DEAD) unreaped task. want<0 — any; otherwise tid=want-1.
/// It returns (tid, code) or None. It marks REAPED.
pub fn reap(want: i64) -> Option<(usize, i32)> {
    let n = NREADY.load(Ordering::SeqCst);
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe {
        for i in 0..n {
            if want >= 0 && i != (want as usize).wrapping_sub(1) { continue; }
            if core::ptr::read_volatile(core::ptr::addr_of!(DEAD[i]))
                && !core::ptr::read_volatile(core::ptr::addr_of!(REAPED[i]))
                && !dying(i)
                && (proc_of(i) != i || crate::proc::exit_notified(i)) {
                core::ptr::write_volatile(core::ptr::addr_of_mut!(REAPED[i]), true);
                return Some((i, core::ptr::read_volatile(core::ptr::addr_of!(EXITCODE[i]))));
            }
        }
    }
    None
}

/// Set a process's private heap (for a per-process brk). base=0 -> the common heap.
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn set_heap(tid: usize, base: u64, top: u64) { unsafe { HEAP_B[tid] = base; HEAP_BR[tid] = base; HEAP_TP[tid] = top; } }
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn heap_base(tid: usize) -> u64 { unsafe { core::ptr::read_volatile(core::ptr::addr_of!(HEAP_B[tid])) } }
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn heap_top(tid: usize) -> u64 { unsafe { core::ptr::read_volatile(core::ptr::addr_of!(HEAP_TP[tid])) } }
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn heap_brk(tid: usize) -> u64 { unsafe { core::ptr::read_volatile(core::ptr::addr_of!(HEAP_BR[tid])) } }
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn set_heap_brk(tid: usize, v: u64) { unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(HEAP_BR[tid]), v); } }

/// Create a ring-3 thread with a full set of registers (for clone). gp goes in the order
/// [rax,rbx,rcx,rdx,rsi,rdi,rbp,r8,r9,r10,r11,r12,r13,r14,r15]. It returns the tid.
/// Find the slot of a finished task for reuse (a thread of the same process — right
/// after DEAD; a process — only after wait4/REAPED). It frees its kernel stack.
/// Is the task executing RIGHT NOW on some core. A finished
/// (DEAD) task stays on its kernel stack until the nearest switch:
/// on the BSP it spins `sti; hlt`, and every tick WRITES an interrupt frame
/// onto that stack. If the slot is reused meanwhile, the stack will move into the free list and
/// will be given to somebody else — an interrupt will start writing into foreign memory. That is exactly how
/// "random" corruption was caught with workloads that often create and finish threads
/// (ClickHouse's thread pool, the JVM's compiler and GC threads).
fn task_on_cpu(i: usize) -> bool {
    let online = crate::percpu::ONLINE.load(Ordering::SeqCst);
    for c in 0..crate::percpu::MAXCPU {
        // A BSP in idling (limbo) does not reset its current task — its sign of
        // idling is a separate one. Without this reservation a dead thread that
        // ran last on the BSP was listed as "on a core" forever, and the wait for the sisters
        // (an exec, a crash of the workload) stood a full second every time.
        if c == 0 && BSP_IDLE.load(Ordering::SeqCst) { continue; }
        if online & (1 << c) != 0 && crate::percpu::cur_task_of(c) == i as u64 { return true; }
    }
    false
}

/// How many times a task was not created because of the `NTASKS` ceiling (fork/clone/a thread).
pub static TASKS_NONE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Report the exhaustion of the task table — not more often than once every five seconds, but not
/// once per boot either: silence after the first time hides exactly what
/// one needs to know about production — did we run into it once or are we holding at the ceiling.
pub fn warn_no_tasks() {
    TASKS_NONE.fetch_add(1, Ordering::Relaxed);
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = crate::tsc::rdtsc();
    let next = NEXT.load(Ordering::Relaxed);
    if next != 0 && now.wrapping_sub(next) >= (1u64 << 62) { return; }
    let hz = crate::tsc::calibrate_cached().max(1);
    NEXT.store(now.wrapping_add(hz.saturating_mul(5)), Ordering::Relaxed);
    let (mut live, mut dead) = (0usize, 0usize);
    let n = NREADY.load(Ordering::SeqCst).min(NTASKS);
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    for t in 0..n { if unsafe { core::ptr::read_volatile(core::ptr::addr_of!(DEAD[t])) } { dead += 1; } else { live += 1; } }
    crate::println!("sched: ATTENTION: the task table has run out ({} out of {}: live {}, dead-unreaped {}); fork/clone answer EAGAIN, refusals {}. The usual reason is that a process does not reap its children (wait/waitpid).",
        n, NTASKS, live, dead, TASKS_NONE.load(Ordering::Relaxed));
}

/// # Safety
/// C1: it hands out a task slot whose previous occupant has been reaped (its stack and
/// address space already returned); the caller fills the row before anybody can choose
/// the slot.
unsafe fn reuse_slot() -> Option<usize> {
    let n = NREADY.load(Ordering::SeqCst);
    let cur = current_id();
    let my_proc = proc_of(cur); // by the PROC leader rather than by CR3: the CR3 of a freed AS may coincide with a new one
    for i in 0..n {
        if i == cur || !DEAD[i] || task_on_cpu(i) || dying(i) { continue; } // it still stands on its own stack (or is being run to the end)
        if REAPED[i] || proc_of(i) == my_proc {
            if KSTACK[i] != 0 && KSZ[i] != 0 { crate::mem::free_contig(KSTACK[i] - KSZ[i] * 4096, KSZ[i]); }
            PENDING[i].store(true, Ordering::SeqCst); // the slot is occupied, but the task is not assembled yet
            KSTACK[i] = 0; KSZ[i] = 0; DEAD[i] = false; GONE[i] = false; EXITREG_OK[i] = false; REAPED[i] = false; CTID[i] = 0; SIGRSP[i] = 0;
            CPU_OF[i].store(0, Ordering::SeqCst); AP_TAKEN[i].store(false, Ordering::SeqCst);
            COMM[i] = [0; 16];              // a dead thread's name is not inherited by a new one
            crate::affinity::forget(i);     // nor are its layout marks
            return Some(i);
        }
    }
    None
}
static mut KSZ: [u64; NTASKS] = [0; NTASKS];  // the pages of a task's kernel stack (for the freeing when the slot is reused)
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn set_kstack_pages(tid: usize, pages: u64) { unsafe { KSZ[tid] = pages; } }
/// The top of a task's kernel stack (rsp0). It is needed by the return from a signal handler:
/// it builds an iret frame ABOVE the current top, where the syscall's stub will no longer
/// return.
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn kstack_top(tid: usize) -> u64 { unsafe { core::ptr::read_volatile(core::ptr::addr_of!(KSTACK[tid])) } }

/// The pages of a kernel stack per task. It was 4 (16 KB) — that is too little for large
/// workloads: the kernel executes syscalls on it, and on top of a syscall an
/// exception (a #PF of a demand page) with its own frame and fxsave area may land.
/// ClickHouse caught a corruption of the return address on that and took the kernel into garbage
/// (a #GP at an address from an early area of the image). 16 pages = 64 KB per task.
pub const KSTACK_PAGES: u64 = 16;
/// Allocate a task's kernel stack: it returns THE TOP. The bottom page stays
/// a guard one — an overflow gives an honest #PF rather than a silent corruption of a neighbour.
pub fn alloc_kstack() -> Option<u64> {
    let base = crate::mem::alloc_contig(KSTACK_PAGES)?;
    crate::paging::guard(base); // the stack's bottom is a guard page
    Some(base + KSTACK_PAGES * 4096)
}

/// # Safety
/// C1 + C3: it fills a FRESH task slot from a caller-supplied user context (this is
/// `clone`'s path: the frame is the parent's, with the child's rsp and rax=0).
/// `kstack_top` must be the top of a kernel stack allocated for this slot, and `rip`/`rsp`
/// must be user addresses mapped in the target address space.
pub unsafe fn spawn_user_ctx(rip: u64, rsp: u64, rflags: u64, fsbase: u64, kstack_top: u64, gp: [u64; 15]) -> usize {
    let i = match reuse_slot() { Some(i) => i, None => {
        if NREADY.load(Ordering::SeqCst) >= NTASKS { return usize::MAX; } // the task limit -> an EAGAIN at the caller
        let i = NREADY.load(Ordering::SeqCst);
        PENDING[i].store(true, Ordering::SeqCst); // BEFORE NREADY grows: otherwise the slot is visible with an empty RSP
        NREADY.fetch_add(1, Ordering::SeqCst); i } };
    let mut sp = kstack_top & !0xF;
    let push = |sp: &mut u64, v: u64| { *sp -= 8; core::ptr::write(*sp as *mut u64, v); };
    push(&mut sp, 0x23); push(&mut sp, rsp); push(&mut sp, rflags); push(&mut sp, 0x2B); push(&mut sp, rip);
    for k in 0..15 { push(&mut sp, gp[k]); }
    RSP[i] = sp; KSTACK[i] = kstack_top; FSBASE[i] = fsbase; DEAD[i] = false; GONE[i] = false; EXITREG_OK[i] = false; ctx_saved(i);
    PROC[i] = PROC[current_id()]; // a thread: the same process
    i
}
/// THIS core's current task (per-CPU gs:32): on the BSP it is the scheduler's CURRENT,
/// on an AP it is its pinned task. The syscalls and proc.rs call from here.
pub fn current_id() -> usize {
    let t = crate::percpu::cur_task();
    if t == u64::MAX { CURRENT.load(Ordering::Relaxed) } else { t as usize }
}

/// Is the task dead (the slot is freed or marked dead). It is needed by those who
/// hold a resource ON BEHALF OF a task and must be able to take it away if the owner has disappeared:
/// otherwise a disappeared owner takes the resource away with it forever (see `l1::DevCell`).
pub fn task_dead(t: usize) -> bool {
    if t >= NTASKS || t >= NREADY.load(Ordering::Relaxed) { return true; }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(DEAD[t])) }
}

/// The number of the scheduler's created tasks (for the console's `ps`).
pub fn task_count() -> usize { NREADY.load(Ordering::Relaxed) }
/// Is the task i alive (not DEAD).
/// The task's saved rsp (0 — it was not saved) and the rip it will continue from:
/// in the switch frame there are first 15 GP registers, then the hardware iret frame.
/// Only for the diagnostics: for an EXECUTING task the value is stale.
pub fn saved_rip(i: usize) -> u64 { saved_slot(i, 15) }
/// The stack pointer and the frame pointer of a STOPPED thread — from the same
/// saved frame. They are needed by a core dump: the threads that did not crash have no other
/// registers, while these three are enough for gdb to show WHERE the thread stood.
/// The offsets are `timer_stub_sched`'s layout (see its prologue): 8=rbp, 15=rip,
/// 18=rsp.
pub fn saved_user_rsp(i: usize) -> u64 { saved_slot(i, 18) }
pub fn saved_user_rbp(i: usize) -> u64 { saved_slot(i, 8) }
/// A thread's FS_BASE (TLS): without it gdb will not find the `__thread` variables.
pub fn saved_fsbase(i: usize) -> u64 {
    if i >= NTASKS { return 0; }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(FSBASE[i])) }
}
/// The task's saved context in its entirety (15 GP in the order r15..rax, rip, cs,
/// rflags, rsp, ss) — for the debugger. None — there is no task or it has never been
/// taken off a core (the context is not on its kernel stack).
pub fn saved_ctx(i: usize) -> Option<[u64; 20]> {
    if i >= NTASKS || !task_alive(i) { return None; }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe {
        let sp = core::ptr::read_volatile(core::ptr::addr_of!(RSP[i]));
        let top = core::ptr::read_volatile(core::ptr::addr_of!(KSTACK[i]));
        let pages = { let k = core::ptr::read_volatile(core::ptr::addr_of!(KSZ[i])); if k != 0 { k } else { KSTACK_PAGES } };
        if sp == 0 || top == 0 || sp < top - pages * 4096 || sp + 20 * 8 > top { return None; }
        let mut c = [0u64; 20];
        for k in 0..20 { c[k] = core::ptr::read_volatile((sp + k as u64 * 8) as *const u64); }
        Some(c)
    }
}
fn saved_slot(i: usize, k: u64) -> u64 {
    if i >= NTASKS { return 0; }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    let sp = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(RSP[i])) };
    if sp == 0 { return 0; }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe { core::ptr::read_volatile((sp + k * 8) as *const u64) }
}
/// The task already occupies a slot but is still being assembled (see PENDING). It must not be touched.
pub fn task_pending(i: usize) -> bool { i < NTASKS && PENDING[i].load(Ordering::SeqCst) }
/// The exit code written down for the task (the one it was killed with).
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn exit_code_of(i: usize) -> i32 { if i >= NTASKS { 0 } else { unsafe { core::ptr::read_volatile(core::ptr::addr_of!(EXITCODE[i])) } } }
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn task_alive(i: usize) -> bool { unsafe { i < NTASKS && !core::ptr::read_volatile(core::ptr::addr_of!(DEAD[i])) } }
/// The maximum of the tasks (for the iteration in `ps`).
pub fn max_tasks() -> usize { NTASKS }
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn set_ctid(tid: usize, addr: u64) { unsafe { CTID[tid] = addr; } }
/// Set a task's FS_BASE (TLS) — a fork child inherits the parent's TLS.
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn set_fsbase(tid: usize, v: u64) { unsafe { FSBASE[tid] = v; } }
/// The CR3 of the current active task (for the inheritance by the threads through clone).
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn task_cr3(tid: usize) -> u64 { unsafe { core::ptr::read_volatile(core::ptr::addr_of!(CR3[tid])) } }
// SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it touches,
// and what crosses cores is atomic (see the module's note).
pub fn set_task_cr3(tid: usize, cr3: u64) { unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(CR3[tid]), cr3); } }
/// Zero the ctid word (CLONE_CHILD_CLEARTID) in the user memory of the active AS if the page is mapped and
/// writable (musl 1.1 keeps the ctid on a thread's stack and removes the stack with a munmap before the exit — like put_user in Linux,
/// a write into an unmapped page is silently skipped).
/// # Safety
/// C1: `ct` is the `clear_child_tid` address in the process's memory (the thread stored it
/// itself), and it is cleared while that process's address space is still active — a thread
/// that has already left does not clear anything.
unsafe fn clear_ctid(ct: u64, leader: usize) {
    let (present, _, _, _) = crate::paging::query(ct);
    if present { core::ptr::write_volatile(ct as *mut i32, 0); } // RO/COW -> a #PF in ring 0 -> cow_fault
    // CLONE_CHILD_CLEARTID: a futex wake of those waiting for a join (Linux's semantics); the store itself
    // already wakes the mwait sleepers, the wake finishes the job with WOKEN/an IPI for those sleeping on the BSP
    crate::sync::wake_addr(ct, usize::MAX, leader, false);
}
/// Finish the current thread: clear the ctid (for a join), mark it dead, give up the processor forever.
pub fn exit_current() -> ! { exit_current_code(0) }

/// Finish the current task with an exit code (for wait4).
pub fn exit_current_code(code: i32) -> ! {
    let cur = current_id();
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe {
        let ct = CTID[cur];
        if ct != 0 { clear_ctid(ct, proc_of(cur)); CTID[cur] = 0; } // clear tid -> a join will wake up
        core::ptr::write_volatile(core::ptr::addr_of_mut!(EXITCODE[cur]), code);
        DEAD[cur] = true;
        core::ptr::write_volatile(core::ptr::addr_of_mut!(GONE[cur]), true);
    }
    crate::affinity::release(cur); // a dedicated core returns into circulation
    let cpu = crate::percpu::cpu_id();
    // SAFETY: C4 — this core's machine state (the registers/MSRs the switch is made of).
    if cpu != 0 { unsafe { ap_exit_to_loop(cpu); } } // an AP: the core returns to the pickup loop
    // SAFETY: C4 — this core's machine state (the registers/MSRs the switch is made of).
    loop { unsafe { core::arch::asm!("sti; hlt"); } } // until the first tick — it is not scheduled any more
}
/// exit_group: mark ALL the tasks of the process (all the threads) as finished with a code (the current one
/// too). It does not return control to the scheduler — the caller hands it over to the supervisor.
pub fn exit_all(code: i32) {
    let n = NREADY.load(Ordering::SeqCst);
    let cur = current_id();
    let my_proc = proc_of(cur); // by the PROC leader rather than by CR3 (see reuse_slot)
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe {
        for i in 0..n {
            if proc_of(i) != my_proc { continue; } // another process — we do not touch it
            let ct = CTID[i]; if ct != 0 { clear_ctid(ct, proc_of(i)); CTID[i] = 0; }
            core::ptr::write_volatile(core::ptr::addr_of_mut!(EXITCODE[i]), code);
            DEAD[i] = true;
            crate::affinity::release(i);
        }
    }
}
/// Finish ALL THE OTHER threads of the process and WAIT until they physically leave
/// the cores. That is how `execve` works in Linux: after a successful exec a process is left with
/// one thread — the calling one.
///
/// Why exactly waiting is needed. Marking a sister DEAD is not enough: it may be executing
/// on another core at that moment — sleeping in an mwait inside a futex wait or spinning
/// in the kernel. If the address space is changed right away and its stack released, it
/// will keep working in A REMOVED AS. So: we marked them -> we knocked on the cores where
/// the sisters are executing right now (their waits wake up and see "I am dead") ->
/// we waited until none of them is listed as current on any core.
///
/// We wait no longer than a second: an exec has no right to hang forever because of a thread
/// stuck in the kernel; about such a case we print honestly.
pub fn exit_siblings(code: i32) {
    let marked = kill_siblings(code);
    if marked > 0 { crate::println!("exec: the process's threads finished: {}", marked); }
}
/// The same without printing: mark the sisters dead and wait until they leave
/// the cores. It returns how many threads were marked. It is needed both by `execve` and by a crash of
/// a multi-threaded workload under the supervisor (`idt`): the supervisor frees
/// the kernel stacks and the address space, and not a single sister must be executing
/// on them at that moment.
pub fn kill_siblings(code: i32) -> usize {
    let n = NREADY.load(Ordering::SeqCst);
    let cur = current_id();
    let my_proc = proc_of(cur);
    let mut marked = 0usize;
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe {
        for i in 0..n {
            if i == cur || proc_of(i) != my_proc || DEAD[i] { continue; }
            let ct = CTID[i]; if ct != 0 { clear_ctid(ct, proc_of(i)); CTID[i] = 0; }
            core::ptr::write_volatile(core::ptr::addr_of_mut!(EXITCODE[i]), code);
            DEAD[i] = true;
            crate::affinity::release(i);
            marked += 1;
        }
    }
    if marked == 0 { return 0; }
    let left = drain_proc(1000);
    if left > 0 {
        let online = crate::percpu::ONLINE.load(Ordering::SeqCst);
        let mut who = [0usize; 4]; let mut k = 0;
        for c in 0..crate::percpu::MAXCPU {
            let t = crate::percpu::cur_task_of(c) as usize;
            if k < 4 && online & (1 << c) != 0 && t < NTASKS && t != cur && proc_of(t) == my_proc { who[k] = t * 100 + c; k += 1; }
        }
        crate::println!("sched: {} threads of the process did not stop within 1 s — I go on (the task*100+the core: {:?})", left, &who[..k]);
    }
    marked
}
/// Wait until the other threads of THE CURRENT process, already marked dead,
/// stop: leave the cores and run to the end what they started in the kernel. The cores on
/// which they stand get an entry into the scheduler (an IPI on the timer's vector):
/// a thread preempted in ring 3 is taken off by exactly that — a dedicated core has no tick,
/// without a call it would keep running. The caller yields its own core if there is
/// somebody to run to the end on it — otherwise on one core it would wait for itself. It returns
/// how many threads did not stop within `ms` milliseconds.
pub fn drain_proc(ms: u64) -> usize {
    let n = NREADY.load(Ordering::SeqCst);
    let cur = current_id();
    let my_proc = proc_of(cur);
    let online = crate::percpu::ONLINE.load(Ordering::SeqCst);
    let me_cpu = crate::percpu::cpu_id();
    for c in 0..crate::percpu::MAXCPU {
        if c == me_cpu || online & (1 << c) == 0 { continue; }
        let t = crate::percpu::cur_task_of(c) as usize;
        if t < NTASKS && t != cur && proc_of(t) == my_proc {
            crate::smp::send_ipi(crate::smp::apic_of(c), crate::idt::TIMER_VECTOR as u8);
        }
    }
    let hz = crate::tsc::calibrate_cached();
    let t0 = crate::tsc::rdtsc();
    loop {
        let mut left = 0usize;
        for i in 0..n {
            if i == cur || proc_of(i) != my_proc { continue; }
            if dying(i) { left += 1; }
        }
        if left == 0 { return 0; }
        if crate::tsc::rdtsc().wrapping_sub(t0) > hz / 1000 * ms { return left; }
        if cpu_has_other_runnable(me_cpu, cur) { yield_now(); }
        // SAFETY: C4 — this core's machine state (the registers/MSRs the switch is made of).
        else { unsafe { core::arch::asm!("sti; pause; cli", options(nostack)); } }
    }
}
/// Before the supervisor's `reset()`: wait until not a single AP is executing a task
/// (the core's current task is "none"). `reset()` gives the tasks' kernel stacks into the
/// free list, while a new instance builds its address space from the same frames:
/// an AP that lingered on a dead task would execute the kernel on a freed
/// stack and with the old CR3 — under KVM that is a "KVM internal error" with CR2 == RIP in
/// the kernel's text (that is how a restart of `examples/mdpipe` on four cores fell).
/// It returns the number of the APs that did not leave within `ms` milliseconds.
pub fn wait_aps_off_tasks(ms: u64) -> usize {
    let online = crate::percpu::ONLINE.load(Ordering::SeqCst);
    // Enter the scheduler on the APs that stand on tasks: a dead thread in ring 3
    // is taken off there, a sleeping one in the kernel wakes up and sees the death (a dedicated
    // core has no tick at all — without a call it would stand on the task forever).
    for c in 1..crate::percpu::MAXCPU {
        if online & (1 << c) != 0 && (crate::percpu::cur_task_of(c) as usize) < NTASKS {
            crate::smp::send_ipi(crate::smp::apic_of(c), crate::idt::TIMER_VECTOR as u8);
        }
    }
    let hz = crate::tsc::calibrate_cached();
    let t0 = crate::tsc::rdtsc();
    loop {
        let mut busy = 0usize;
        for c in 1..crate::percpu::MAXCPU {
            if online & (1 << c) == 0 { continue; }
            if (crate::percpu::cur_task_of(c) as usize) < NTASKS { busy += 1; }
        }
        if busy == 0 { return 0; }
        if crate::tsc::rdtsc().wrapping_sub(t0) > hz / 1000 * ms { return busy; }
        core::hint::spin_loop();
    }
}
/// Mark ALL the tasks dead (a restart of the workload by the operator/the watchdog):
/// after that they stop in the same way as at an `exit_group`, while the supervisor
/// gets control when they all stop (the BSP, "all the tasks have finished").
pub fn kill_all(code: i32) {
    let n = NREADY.load(Ordering::SeqCst);
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe {
        for i in 0..n {
            if DEAD[i] { continue; }
            core::ptr::write_volatile(core::ptr::addr_of_mut!(EXITCODE[i]), code);
            DEAD[i] = true;
            crate::affinity::release(i);
        }
    }
    let online = crate::percpu::ONLINE.load(Ordering::SeqCst);
    for c in 1..crate::percpu::MAXCPU {
        if online & (1 << c) != 0 && (crate::percpu::cur_task_of(c) as usize) < NTASKS {
            crate::smp::send_ipi(crate::smp::apic_of(c), crate::idt::TIMER_VECTOR as u8);
        }
    }
}
/// AN IDLING AP HOLDS THE CR3 OF ITS LAST TASK. A departure into the pickup loop
/// does not change the address space (a reload of CR3 at every idling would cost
/// a TLB flush at every wakeup). But before freeing the workload's page
/// tables, the supervisor must take all the cores off them: otherwise the frames
/// go to a new instance while an AP keeps executing the kernel through tables
/// that have already become something else — under KVM that is a "KVM internal error" with
/// CR2 == RIP in the kernel's text (that is how a restart of `examples/mdpipe` by the operator
/// on four cores fell). The cores move onto the kernel's CR3 in the pickup loop and answer;
/// it returns how many did not answer within `ms` milliseconds.
static KCR3_REQ: AtomicU64 = AtomicU64::new(0);
static KCR3_ACK: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
pub fn aps_to_kernel_cr3(ms: u64) -> usize {
    let g = KCR3_REQ.fetch_add(1, Ordering::SeqCst) + 1;
    let cpus = SCHED_CPUS.load(Ordering::SeqCst) & !1;
    for c in 1..MAXCPU { if cpus & (1 << c) != 0 { ap_ring(c); crate::sync::kick(c as u32); } }
    let hz = crate::tsc::calibrate_cached();
    let t0 = crate::tsc::rdtsc();
    loop {
        let mut left = 0usize;
        for c in 1..MAXCPU { if cpus & (1 << c) != 0 && KCR3_ACK[c].load(Ordering::SeqCst) < g { left += 1; } }
        if left == 0 { return 0; }
        if crate::tsc::rdtsc().wrapping_sub(t0) > hz / 1000 * ms { return left; }
        core::hint::spin_loop();
    }
}
/// A reset of the scheduler before a restart of the workload (supervise): all the tasks are forgotten.
pub fn reset() {
    NREADY.store(0, Ordering::SeqCst);
    CURRENT.store(0, Ordering::SeqCst);
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe {
        for i in 0..NTASKS {
            // the kernel stacks of the crashed instance's tasks go into the free list (nobody executes on them:
            // reset is called by the supervisor after a longjmp, a cli, the timer is stopped)
            if KSTACK[i] != 0 && KSZ[i] != 0 { crate::mem::free_contig(KSTACK[i] - KSZ[i] * 4096, KSZ[i]); }
            DEAD[i] = false; GONE[i] = false; EXITREG_OK[i] = false; REAPED[i] = false; CTID[i] = 0; CR3[i] = 0; HEAP_B[i] = 0; HEAP_BR[i] = 0; HEAP_TP[i] = 0; EXITCODE[i] = 0; KSTACK[i] = 0; KSZ[i] = 0; FSBASE[i] = 0; PROC[i] = i as u8; PPID[i] = -1; FPU_VALID[i] = false; DEAD_AS[i] = 0; CPU_OF[i].store(0, Ordering::SeqCst); AP_TAKEN[i].store(false, Ordering::SeqCst); COMM[i] = [0; 16]; }
    }
    crate::percpu::set_sched_cr3(0);
    BSP_IDLE.store(false, Ordering::SeqCst);
    crate::affinity::reset();
}
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static NREADY: AtomicUsize = AtomicUsize::new(0);
/// A hint to the scheduler: whom to choose next (a directed switch of a futex wake,
/// PLAN-SYNC). usize::MAX = there is none. It is set by the waker (the BSP or an IPI from an AP).
/// THE HINT IS PER CORE rather than one per machine.
///
/// The hint's meaning is "let the core c switch to the task t right now, without waiting for
/// the rotation". The slot was ONE for the whole machine, and that broke in two ways at once:
///
/// * two wakeups that happened simultaneously on different cores overwrote each
///   other — the second hint survived, the first was lost, and its task waited for
///   the common rotation, that is, until a tick (500 us);
/// * the BSP's scheduler took the slot UNCONDITIONALLY (a `swap`), even when the hint
///   was addressed to an AP: the check "this is my task" went already AFTER the taking.
///
/// The measurement on ClickHouse (60 parallel queries, four threads): 4578 hints,
/// of them OVERWRITTEN BY A FOREIGN ONE 693 — fifteen percent. Every lost one costs
/// the woken thread a quantum, while the pipeline's threads wake each other several times
/// per query: while one waits, the other three cores stand still.
///
/// It is exactly the same error and exactly the same cure as with `SOFT_YIELD` a floor below —
/// "two cores would eat one common flag from each other".
#[repr(align(64))]
pub struct HintSlot(pub AtomicUsize);
pub static HINT: [HintSlot; MAXCPU] = [const { HintSlot(AtomicUsize::new(usize::MAX)) }; MAXCPU];
/// The hint's diagnostics. SEPARATELY, BECAUSE ONE NUMBER LIED.
///
/// There was one counter — "overwritten by a foreign one", and on the stand it showed 85 % (13.2 million
/// out of 15.5). That read as "five sixths of the scheduler's decisions are lost", and
/// exactly so it stood in session 28's plan. In fact three DIFFERENT events were added
/// into one number, and the loss is only the first of them:
///
/// * `HINT_CLOBBER` — a hint for ANOTHER task that is still
///   applicable was overwritten: that is exactly the loss, the addressee will go into the common rotation;
/// * `HINT_SAME` — the same task was hinted at once more (a socket's owner is woken
///   several times per frame). There is nothing to lose here: the hint is the same one;
/// * `HINT_STALE` — A STALE hint was overwritten, whose addressee is no longer
///   applicable (it left the core, fell asleep again, died). Not a loss either — that
///   hint could not be applied anyway.
///
/// The stale ones piled up because the slot was NOT cleaned EVERYWHERE: the BSP took it
/// with a `swap` always, while an AP's scheduler did so only when the hint was suitable, and
/// an AP's pickup loop did not look at the slot at all. One unsuitable hint stayed
/// in the slot FOREVER, and every next one on that core counted as "overwritten".
pub static HINT_CLOBBER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static HINT_SET: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static HINT_SAME: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static HINT_STALE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The hint was APPLIED: by the scheduler (a preemption) and by the pickup loop (an empty core).
pub static HINT_USED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static HINT_PICKUP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Is the hint `t` suitable for the core `cpu` RIGHT NOW. Without side effects:
/// `runnable_peek` rather than `blocked_for_sched` (that one counts the skips and declares
/// the readiness — a counter must not do such things).
#[inline]
fn hint_live(cpu: usize, t: usize) -> bool {
    t < NREADY.load(Ordering::Relaxed) && !not_ready(t)
        && CPU_OF[t].load(Ordering::Relaxed) as usize == cpu
        && crate::sync::runnable_peek(t)
}
/// Set a hint for the core `cpu`.
#[inline]
pub fn hint_set_on(cpu: usize, t: usize) {
    if cpu >= MAXCPU { return; }
    HINT_SET.fetch_add(1, Ordering::Relaxed);
    let prev = HINT[cpu].0.swap(t, Ordering::SeqCst);
    if prev == usize::MAX { return; }
    if prev == t { HINT_SAME.fetch_add(1, Ordering::Relaxed); }
    else if hint_live(cpu, prev) { HINT_CLOBBER.fetch_add(1, Ordering::Relaxed); }
    else { HINT_STALE.fetch_add(1, Ordering::Relaxed); }
}
/// Set a hint for OUR OWN core.
#[inline]
pub fn hint_set(t: usize) { hint_set_on(crate::percpu::cpu_id(), t); }
/// Give the remainder of the slice to the task t IMMEDIATELY (a soft interrupt into the scheduler).
/// It is called from the waker's syscall (its context will be saved and continued later).
/// A software entry into the scheduler (an int 0x41 from yield_*): an EOI is NOT needed (the LAPIC's ISR
/// bit is not raised by a soft int), while on hardware without APICv (Sandy Bridge) every
/// write into the xAPIC MMIO = a vmexit of ~1.5 us — that was half of the same-core futex RTT.
/// There is no race: all the callers are a syscall/kernel context with IF=0 (FMASK=0x200),
/// a hardware tick cannot cut in between the store and the int 0x41.
/// The flag is PER CORE: the scheduler spins on every core of the pool, and two cores would eat
/// one common flag from each other — an EOI would go to the wrong interrupt, while the ISR bit
/// of the timer's vector would stay raised forever (see the A12 analysis below).
#[repr(align(64))]
pub struct YieldFlag(pub AtomicU64);
pub static SOFT_YIELD: [YieldFlag; MAXCPU] = [const { YieldFlag(AtomicU64::new(0)) }; MAXCPU];
pub fn yield_to(t: usize) {
    hint_set(t);
    soft_yield();
}
/// Give the CPU to the scheduler immediately (a rotation; those sleeping on a futex are skipped).
pub fn yield_now() { soft_yield(); }

/// A12: a software entry into the scheduler (`int 0x41`) with A CLOSED race window.
///
/// The handler tells a software yield from a real timer interrupt by
/// the flag `SOFT_YIELD` and sends an EOI only to a real one. Between the setting of the flag
/// and the `int` instruction itself a real timer interrupt fits: it eats
/// the flag, the EOI does not go out, the ISR bit of the timer's vector stays raised — and the LAPIC
/// NEVER AGAIN delivers that vector. The preemption dies forever, and
/// silently at that: a thread waits for its partners, whom nobody gives a core.
///
/// Under QEMU the window is negligible and the tick is rare, so for three generations that was not
/// visible; on `c6i.metal` the production mode lay down at the very first launch — in the log there was exactly
/// one entry into the scheduler and silence.
///
/// We fix it by closing the window: a `cli` before the flag is set. The interrupt gate `int 0x41`
/// will save RFLAGS already with IF=0, so after the `iret` the interrupts will stay
/// closed until our explicit `sti` — and by that moment the flag is removed and a deferred
/// tick will come as a real one, with its own EOI.
/// Everything goes in ONE asm block: the compiler is entitled to reorder separated blocks between
/// themselves (and `options(nomem)` directly allows that), and the race window would open
/// anew. `pushfq`/`popfq` at the same time restore the original IF: an interrupt gate
/// saves RFLAGS already with IF=0, after the `iret` the interrupts are closed until the `popfq`.
/// The core's number is taken INSIDE the same block (gs:24) — between a read from the outside and
/// the `cli` a tick would manage to preempt the task, and the flag would land on the wrong core.
#[inline]
fn soft_yield() {
    // SAFETY: C4 — this core's machine state (the registers/MSRs the switch is made of).
    unsafe {
        core::arch::asm!(
            "pushfq",
            "cli",
            "mov {c}, gs:[24]",      // the core's logical number
            "shl {c}, 6",            // the step of a SOFT_YIELD element is 64 bytes (a cache line)
            "add {c}, {f}",
            "mov qword ptr [{c}], 1",
            "int 0x41",              // TIMER_VECTOR
            "popfq",
            f = in(reg) SOFT_YIELD.as_ptr(),
            c = out(reg) _,
        );
    }
}
/// Is there another task on the BSP ready to execute (besides me)? If yes —
/// the one waiting on a futex must give up the CPU (yield_now) rather than sleep in an mwait holding the core.
pub fn bsp_has_other_runnable(me: usize) -> bool { cpu_has_other_runnable(0, me) }

/// The same for ANY core of the scheduler: on A SHARED core a waiting thread must give up
/// the processor to its neighbours rather than sleep on in an mwait holding the core. On A DEDICATED core
/// there are no neighbours by construction, and the function will honestly return false — that is, the
/// run-to-completion path stays exactly as it was.
pub fn cpu_has_other_runnable(cpu: usize, me: usize) -> bool {
    let n = NREADY.load(Ordering::Relaxed);
    for t in 0..n {
        if t == me { continue; }
        if CPU_OF[t].load(Ordering::Relaxed) as usize != cpu { continue; }
        // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
        // touches, and what crosses cores is atomic (see the module's note).
        if unsafe { core::ptr::read_volatile(core::ptr::addr_of!(DEAD[t])) } {
            // a thread being run to the end needs the core just as much as a live one
            if dying(t) && !task_on_cpu(t) { return true; }
            continue;
        }
        if crate::sync::runnable_peek(t) { return true; }
    }
    false
}
/// The BSP is idling: the main thread has moved to a dedicated core (`pin=main:N`),
/// and the control plane executes not a single task. A separate flag is needed
/// because the BSP's "current task" means nothing at that moment: saving a frame into
/// it (this is the idle loop's frame) means spoiling a foreign context.
pub static BSP_IDLE: AtomicBool = AtomicBool::new(false);

/// Create a task: an initial context on its stack (15 zero registers + an iret frame).
/// # Safety
/// C1 + C3: a RING-0 task (the kernel's own thread): `entry` never returns and
/// `stack_top` must be the top of a stack nobody else uses.
pub unsafe fn spawn(entry: extern "C" fn() -> !, stack_top: u64) {
    let i = NREADY.fetch_add(1, Ordering::SeqCst);
    let mut sp = stack_top & !0xF;
    // the iret frame: ss, rsp, rflags, cs, rip
    let push = |sp: &mut u64, v: u64| { *sp -= 8; core::ptr::write(*sp as *mut u64, v); };
    push(&mut sp, 0x10);            // SS (kernel data)
    // The task's RSP: the entry into entry is as after a call, rsp = 8 (mod 16). With rsp = 0
    // (mod 16) the compiler puts the xmm spills at an address aligned "to 8",
    // and the very first movaps gives a #GP (it was caught in ioyacct: obs::comm).
    push(&mut sp, (stack_top & !0xF) - 8);
    push(&mut sp, 0x202);           // RFLAGS (IF=1)
    push(&mut sp, 0x08);            // CS (kernel code)
    push(&mut sp, entry as usize as u64); // RIP
    for _ in 0..15 { push(&mut sp, 0); } // r15..rax = 0
    RSP[i] = sp; KSTACK[i] = 0; FSBASE[i] = 0; PROC[i] = i as u8; PPID[i] = -1; ctx_saved(i);
    acct_born(i);
}

/// # Safety
/// C4: a model-specific register of THIS core; the caller must know it exists on this CPU.
unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    core::arch::asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nostack, preserves_flags));
    (hi as u64) << 32 | lo as u64
}
/// # Safety
/// C4: as `rdmsr`, and the value must be one this register accepts — a wrong write changes
/// the machine silently.
unsafe fn wrmsr(msr: u32, v: u64) {
    core::arch::asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nostack, preserves_flags));
}
const IA32_FS_BASE: u32 = 0xC000_0100;

/// Create a ring-3 thread: an initial iret frame in ring 3 (SS/RSP/RFLAGS IF=1/CS/RIP)
/// on its kernel stack; rdi = arg. The scheduler gives it a time slice on a par with the kernel.
/// # Safety
/// C1 + C3, as `spawn_user_ctx`: a fresh slot, a kernel stack of its own, user addresses
/// mapped in the address space the task will run in.
pub unsafe fn spawn_user(entry: u64, ustack_top: u64, kstack_top: u64, arg_rdi: u64, fs_base: u64) {
    let i = NREADY.fetch_add(1, Ordering::SeqCst);
    let mut sp = kstack_top & !0xF;
    let push = |sp: &mut u64, v: u64| { *sp -= 8; core::ptr::write(*sp as *mut u64, v); };
    push(&mut sp, 0x23);           // SS (user data DPL3)
    push(&mut sp, ustack_top);     // the user's RSP
    push(&mut sp, 0x202);          // RFLAGS (IF=1 — the timer preempts)
    push(&mut sp, 0x2B);           // CS (user code64 DPL3)
    push(&mut sp, entry);          // RIP
    for k in 1..=15 { push(&mut sp, if k == 6 { arg_rdi } else { 0 }); } // rdi = push#6
    RSP[i] = sp; KSTACK[i] = kstack_top; FSBASE[i] = fs_base; PROC[i] = i as u8; PPID[i] = -1; ctx_saved(i);
    acct_born(i);
    crate::proc::init_proc(i);
}

/// Create a ring-3 PROCESS task with ITS OWN address space (cr3). At a
/// switch to it the scheduler loads its CR3. entry/ustack are in its AS.
/// # Safety
/// C1 + C3, and `cr3` must be a COMPLETE page table of the new process (its kernel half
/// copied): the task will be entered under it.
pub unsafe fn spawn_process(entry: u64, ustack_top: u64, kstack_top: u64, cr3: u64) -> usize {
    spawn_process_ctx(entry, ustack_top, 0x202, kstack_top, cr3, 0)
}

/// fork: a process in its own AS with ALL the parent's registers (gp in the order
/// [rax,rbx,rcx,rdx,rsi,rdi,rbp,r8..r15]; rax=0 — the fork's return in the child).
/// # Safety
/// C1 + C3, as `spawn_process`; the registers are the forking thread's frame, and the
/// child's rax is set to 0 by the caller.
pub unsafe fn spawn_process_regs(entry: u64, ustack_top: u64, rflags: u64, kstack_top: u64, cr3: u64, gp: [u64; 15]) -> usize {
    let i = match reuse_slot() { Some(i) => i, None => {
        // THE TASK CEILING IS A REFUSAL RATHER THAN A WRITE PAST THE BOUNDARY.
        //
        // The check stood only at the creation of A THREAD (`spawn_user_ctx`), while fork went
        // past it: with `NREADY >= NTASKS` the index was taken past the end of the arrays, and
        // `PENDING[i]`, `RSP[i]`, `KSTACK[i]`, `CR3[i]`, `PROC[i]`, `PPID[i]`
        // were written into the kernel's foreign memory — by an ordinary `fork()` from ring 3. From the outside that is
        // a mute hang of THE WHOLE machine: the tick stops, the console is silent,
        // there is no diagnosis. It is caught by a server with a fork per connection that does not
        // reap its children: the 192 slots run out at the 190th connection (the gate `forkcap`).
        if NREADY.load(Ordering::SeqCst) >= NTASKS { warn_no_tasks(); return usize::MAX; }
        let i = NREADY.load(Ordering::SeqCst);
        PENDING[i].store(true, Ordering::SeqCst); // BEFORE NREADY grows (see PENDING)
        NREADY.fetch_add(1, Ordering::SeqCst); i } };
    let mut sp = kstack_top & !0xF;
    let push = |sp: &mut u64, v: u64| { *sp -= 8; core::ptr::write(*sp as *mut u64, v); };
    push(&mut sp, 0x23); push(&mut sp, ustack_top); push(&mut sp, rflags | 0x2); push(&mut sp, 0x2B); push(&mut sp, entry);
    for k in 0..15 { push(&mut sp, gp[k]); } // rax is pushed first (popped last)
    RSP[i] = sp; KSTACK[i] = kstack_top; FSBASE[i] = 0; DEAD[i] = false; GONE[i] = false; EXITREG_OK[i] = false; CR3[i] = cr3; ctx_saved(i);
    PROC[i] = i as u8; PPID[i] = -1;
    i
}

/// Like spawn_process, but with given rflags and rax (for a fork: the child starts
/// at the point of the return from the syscall with rax=0). It returns the task's index.
/// # Safety
/// C1 + C3, as `spawn_process_regs`, with the full saved context.
pub unsafe fn spawn_process_ctx(entry: u64, ustack_top: u64, rflags: u64, kstack_top: u64, cr3: u64, rax: u64) -> usize {
    let i = match reuse_slot() { Some(i) => i, None => {
        if NREADY.load(Ordering::SeqCst) >= NTASKS { warn_no_tasks(); return usize::MAX; } // see spawn_process_regs
        NREADY.fetch_add(1, Ordering::SeqCst) } };
    let mut sp = kstack_top & !0xF;
    let push = |sp: &mut u64, v: u64| { *sp -= 8; core::ptr::write(*sp as *mut u64, v); };
    push(&mut sp, 0x23); push(&mut sp, ustack_top); push(&mut sp, rflags | 0x2); push(&mut sp, 0x2B); push(&mut sp, entry);
    // 15 GP registers; the stub pops r15 first, rax last -> we push rax FIRST.
    push(&mut sp, rax);                   // rax (popped last)
    for _ in 0..14 { push(&mut sp, 0); }  // rbx..r15 = 0
    RSP[i] = sp; KSTACK[i] = kstack_top; FSBASE[i] = 0; DEAD[i] = false; GONE[i] = false; EXITREG_OK[i] = false; CR3[i] = cr3; ctx_saved(i);
    PROC[i] = i as u8; PPID[i] = -1; // a new process: the leader is itself; the parent is set by fork
    PENDING[i].store(false, Ordering::SeqCst); // here the task is assembled entirely
    i
}

/// The switch (it is called from timer_stub_sched): save the current task's rsp,
/// move to the next one. An EOI. It returns the next task's rsp.
#[unsafe(no_mangle)]
extern "C" fn rust_schedule(cur_rsp: u64) -> u64 {
    // A12: while we are in the handler, the trace to the disk is NOT flushed (see trace.rs).
    crate::trace::irq_enter();
    let t0 = crate::tsc::rdtsc();
    let r = rust_schedule_inner(cur_rsp);
    let d = crate::tsc::rdtsc().wrapping_sub(t0);
    // The FIRST tick prints the timer's state line into the serial — that is milliseconds,
    // and in an average over a thousand ticks it alone gives more than all the rest together.
    // We count from the second one: the value must answer the question "how much a tick costs"
    // rather than "was there printing at the start".
    if TICK_N.fetch_add(1, Ordering::Relaxed) != 0 { TICK_CY.fetch_add(d, Ordering::Relaxed); }
    let mut m = TICK_MAX.load(Ordering::Relaxed);
    while d > m { match TICK_MAX.compare_exchange_weak(m, d, Ordering::Relaxed, Ordering::Relaxed) { Ok(_) => break, Err(v) => m = v } }
    crate::trace::irq_leave();
    r
}

/// THE PROFILE OF THE TICK'S PHASES (`talaria.tickprof=1`).
///
/// "A tick costs 2.3 us" is not a diagnosis but a question. The total price is made up of
/// things that are cured differently: the FPU save, the acknowledgement of the interrupt,
/// the clock's discipline, the cleanup, the housekeeping, the choice of the next task and the
/// switch itself. Session 19 broke it down by phases ONE time, by hand, and three
/// addends out of seven turned out to be superfluous; so that this analysis does not have to be
/// written anew, it now lives in the kernel behind a knob.
///
/// The overhead with the profile off is one atomic read per phase.
pub const TP_N: usize = 8;
pub static TP_ON: AtomicBool = AtomicBool::new(false);
pub static TP_SUM: [AtomicU64; TP_N] = [const { AtomicU64::new(0) }; TP_N];
pub static TP_CNT: [AtomicU64; TP_N] = [const { AtomicU64::new(0) }; TP_N];
pub static TP_NAMES: [&str; TP_N] = [
    "the FPU save", "the EOI and the timer", "the clock", "the AS cleanup",
    "the housekeeping", "the accounting", "the choice", "the switch",
];
#[inline(always)]
fn tp(ph: usize, t: &mut u64) {
    if !TP_ON.load(Ordering::Relaxed) { return; }
    let now = crate::tsc::rdtsc();
    TP_SUM[ph].fetch_add(now.wrapping_sub(*t), Ordering::Relaxed);
    TP_CNT[ph].fetch_add(1, Ordering::Relaxed);
    *t = now;
}

/// THE PRICE OF THE TICK ITSELF (TSC cycles, the sum/the number/the maximum) — `stat:` prints
/// the average. The slice sets HOW MANY times a second this price is paid, so without
/// it "the slice is shorter" and "the tick is more expensive" are indistinguishable, while they are cured differently.
pub static TICK_CY: AtomicU64 = AtomicU64::new(0);
pub static TICK_N: AtomicU64 = AtomicU64::new(0);
pub static TICK_MAX: AtomicU64 = AtomicU64::new(0);

/// The same as `stay`, for the BSP: restore the FPU of the interrupted task if it
/// was saved (not idle and the task did not move to an AP — see the start of `rust_schedule_inner`).
#[inline]
fn bsp_stay(idle: bool, cur0: usize, cur_rsp: u64) -> u64 {
    if !idle && cur0 < NTASKS && CPU_OF[cur0].load(Ordering::SeqCst) == 0 { return stay(cur0, cur_rsp); }
    cur_rsp
}
fn rust_schedule_inner(cur_rsp: u64) -> u64 {
    // The scheduler spins on EVERY core of the common pool. The queues are separate:
    // a core chooses only its own tasks (CPU_OF == cpu), so there is neither a lock
    // nor a common "current one" here.
    let cpu = crate::percpu::cpu_id();
    if cpu != 0 { return ap_schedule(cpu, cur_rsp); }
    crate::gdb::apply_if_changed(0);
    let mut tpt = if TP_ON.load(Ordering::Relaxed) { crate::tsc::rdtsc() } else { 0 };
    // The interrupted task's FPU/SSE must be saved FIRST of all (the kernel is built with SSE and
    // clobbers xmm). We do NOT touch a migrated task (CPU_OF!=0): its FPU[t] is the image
    // for the AP, while the current BSP context is limbo (an fxsave/fxrstor race tears the image).
    // A BSP that is idling (the main thread has moved to a dedicated core): there is NO current
    // task, and touching the "current one's" RSP/FPU is not allowed — that is a foreign context.
    let idle = BSP_IDLE.load(Ordering::SeqCst);
    let cur0 = CURRENT.load(Ordering::Relaxed);
    let limbo = BSP_LIMBO.load(Ordering::SeqCst);
    if !idle && !limbo {
        if CPU_OF[cur0].load(Ordering::SeqCst) != 0 { BSP_SAVE_FOREIGN.fetch_add(1, Ordering::Relaxed); }
        // SAFETY: C2 + C3 — a context switch: the task has been claimed by this core, and the call
        // does not return onto this stack.
        unsafe { fpu_save(core::ptr::addr_of_mut!(FPU[cur0]) as *mut u8); FPU_VALID[cur0] = true; }
    }
    // A12: the first tick goes into the log once. The scheduler is preemptive: without ticks
    // a second thread will never get a core, while the first one will wait for it forever, and
    // from the outside that looks like "the workload is silent".
    if !FIRST_TICK.swap(true, Ordering::Relaxed) {
        // A12: we print the timer's STATE rather than only the fact of the entry. On a machine without
        // a console that is the only chance to tell "we entered by a real tick" from
        // "we entered by a software yield, while the timer is not programmed/is masked".
        crate::println!("sched: the first entry ({}) — LVT={:#x} DCR={:#x} initial={} current={} ISR={}",
            if SOFT_YIELD[0].0.load(Ordering::Relaxed) != 0 { "a software yield" } else { "a timer interrupt" },
            crate::smp::lapic_peek(0x320), crate::smp::lapic_peek(0x3E0),
            crate::smp::lapic_peek(0x380), crate::smp::lapic_peek(0x390),
            crate::smp::isr_pending(crate::idt::TIMER_VECTOR as u8));
    }
    tp(0, &mut tpt);
    // An EOI only for a real timer interrupt: a software int 0x41 (yield_*)
    // does not raise the ISR bit, while an acknowledgement costs money: a write into the xAPIC window without
    // APICv is an exit to the host with an emulation of the instruction, measured at 10.2 us (see
    // `smp::lapic_eoi`, which removes that exit with the KVM PV EOI protocol).
    let hw_tick = SOFT_YIELD[0].0.swap(0, Ordering::Relaxed) == 0;
    if hw_tick { tick_t0(0); tick_rip_note(0, cur_rsp); } else { sy_t0(0); }
    if hw_tick {
        // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
        // touches, and what crosses cores is atomic (see the module's note).
        unsafe { crate::smp::lapic_eoi(); } // EOI
        // A12: we count the REAL timer interrupts SEPARATELY. A software yield
        // (an int 0x41) comes in here too, and by a common counter "the scheduler spins"
        // is indistinguishable from "there is no preemption at all" — and that is exactly what happened on
        // metal, when the wait tier masked the timer's LVT.
        HW_TICKS.fetch_add(1, Ordering::Relaxed);
    }
    tp(1, &mut tpt);
    // the diagnostics (talaria.strace=N): once every ~4000 ticks — who is on the CPU and where (the rip from the iret frame)
    static TICKS: AtomicUsize = AtomicUsize::new(0);
    let tk = TICKS.fetch_add(1, Ordering::Relaxed);
    if tk % 2000 == 1999 && crate::syscall::strace_on() {
        // SAFETY: C3 — the interrupt/syscall FRAME of the thread this core interrupted: it lies
        // on that thread's kernel stack, which is alive for as long as the thread is, and the
        // offsets are the ones the stub pushed.
        let rip = unsafe { core::ptr::read_volatile((cur_rsp + 15 * 8) as *const u64) };
        // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
        // touches, and what crosses cores is atomic (see the module's note).
        let cs = unsafe { core::ptr::read_volatile((cur_rsp + 16 * 8) as *const u64) };
        // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
        // touches, and what crosses cores is atomic (see the module's note).
        crate::println!("[sched] the tick {}: t{} rip={:#x} cs={:#x} live={}", tk + 1, cur0, rip, cs, { let mut c = 0; for i in 0..NREADY.load(Ordering::Relaxed) { if unsafe { !DEAD[i] } { c += 1; } } c });
        crate::sync::dump_waiters();
    }
    // The wall-clock discipline by an external clock. BEFORE the early return "there are no tasks":
    // the clock must run correctly while idling too — the file system's mtime and the
    // stamps that the kernel hands out apart from the workload stand on it. The path is cheap and
    // bounded by construction (tsc::wall_tick), otherwise it would have no place here.
    crate::tsc::wall_tick();
    tp(2, &mut tpt);
    let n = NREADY.load(Ordering::SeqCst);
    if n == 0 { return bsp_stay(idle, cur0, cur_rsp); }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe { reap_dead_spaces(crate::paging::current_cr3()); }
    tp(3, &mut tpt);
    // A12: the kernel's pulse. `talaria.heartbeat=<seconds>` — a line from the timer's tick once every
    // N seconds. On a machine without a console that is the only way to tell "the kernel
    // is alive while the workload stands still" from "the kernel is dead": a break of the log looks the same.
    let hb = HEARTBEAT_TICKS.load(Ordering::Relaxed);
    if hb != 0 && (tk as u64).is_multiple_of(hb) {
        // We count in TICKS rather than in time: the pulse must be visible even when
        // the kernel's clock is doubtful, and — most importantly — its ABSENCE must mean
        // exactly one thing: there is no preemption any more. A binding to seconds smeared that
        // (the first tick comes earlier than the first second, while further on there may be no ticks
        // at all — and it is exactly that case that must be caught).
        let n = NREADY.load(Ordering::Relaxed);
        let mut alive = 0;
        // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
        // touches, and what crosses cores is atomic (see the module's note).
        for t in 0..n { if unsafe { !DEAD[t] } { alive += 1; } }
        // A12: the RFLAGS of the interrupted context (IF) and the IRR — next to the ticks.
        // SAFETY: C3 — the interrupt/syscall FRAME of the thread this core interrupted: it lies
        // on that thread's kernel stack, which is alive for as long as the thread is, and the
        // offsets are the ones the stub pushed.
        let rfl = unsafe { core::ptr::read_volatile((cur_rsp + 17 * 8) as *const u64) };
        crate::println!("hb: the tick {} (of the timer {}), {} ms, tasks {} (live {}), the current one {}, IF={} IRR={}{}",
            tk, HW_TICKS.load(Ordering::Relaxed), crate::tsc::mono_ns() / 1_000_000,
            n, alive, CURRENT.load(Ordering::Relaxed),
            (rfl >> 9) & 1, crate::smp::irr_pending(crate::idt::TIMER_VECTOR as u8),
            if crate::smp::isr_pending(crate::idt::TIMER_VECTOR as u8) {
                " — THE TIMER'S ISR IS STUCK (the EOI was not sent: there will be no preemption any more)"
            } else { "" });
    }
    let stop = STOP_AT_NS.load(Ordering::Relaxed);
    if stop != 0 && !STOP_SENT.load(Ordering::Relaxed) && crate::tsc::mono_ns() >= stop {
        STOP_SENT.store(true, Ordering::Relaxed);
        crate::println!("talaria: talaria.stop — a SIGTERM to the process pid=1 (a clean stop of the workload)");
        crate::proc::send_proc(0, 15);
    }
    // THE HOUSEKEEPING AND THE CONSOLE ON TOP OF THE WORKLOAD. Under the supervisor that is done by
    // `supervise::sched_tick` (the watchdog, the status line and the restart at the
    // operator's request are there too). Without the supervisor it is the console, and that is not a luxury:
    // `talaria.mode=proc` (the mode of PostgreSQL, MariaDB, MongoDB and ClickHouse)
    // does not bring the supervisor up, and before this change the operator's console there did not
    // answer AT ALL — a typed command silently piled up in the port.
    //
    // NOT AT EVERY TICK. Polling COM1 is an access to a legacy port, that is,
    // an exit into the hypervisor; with a slice of 0.5 ms it would happen two thousand times a
    // second. A human will not notice the difference between 0.5 and 8 ms on a key press, while
    // the kernel on a latency-critical machine will. So the polling goes with a step of
    // about eight milliseconds computed from the effective slice.
    //
    // (This same polling once revealed a race in the installation of page tables — two cores
    // lost each other's work because a new table was installed by a simple
    // write. It is closed with a CAS in `paging`, the gate `memrace` is clean; the details are
    // in `docs/TODO.md`.)
    if crate::supervise::RECOVERY_ARMED.load(Ordering::Relaxed) { crate::supervise::sched_tick(); }
    else if crate::console::poll_due(tk as u64) { crate::console::poll_tick(); }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe { reap_dead_kstacks(); } // the old stacks of the migrated ones (the BSP is no longer on them)
    crate::affinity::balance_tick(); // the spreading/levelling over the pool's cores (the clock is the TSC, not the ticks)
    crate::obs::periodic_tick();              // talaria.top=<ms>: a snapshot into the log (off by default)
    crate::acpi::button_tick();               // the power button (stop-instances) and the term of a planned stop
    tp(4, &mut tpt);
    let cur = CURRENT.load(Ordering::Relaxed);
    // We count only the REAL ticks: a software yield (an int 0x41) comes in here too,
    // and without this check the profile showed not "where the workload spends its time" but
    // "from where it is most often called to give up the processor" — that is, the yield itself.
    if hw_tick {
        note_cpu_tick(0, !idle); crate::smp::timer_tick_reload();
        tick_gap_note(0, if idle { u64::MAX } else { cur as u64 }, cur_rsp);
        // The tick's period is by THIS core's demand: shorter where somebody waits for the network
        // (otherwise nobody would see a frame until the end of a foreign quantum), longer where
        // there is nobody to preempt (a tick is a preemption; in solitude it is only a tail).
        let solo = idle || !cpu_has_other_runnable(0, cur0);
        crate::smp::retune_timer(crate::smp::want_period_ns(0, solo));
        if !solo { push_steal(0); }
        runhist_note();
    }
    if hw_tick && !idle && cur < NTASKS { TICKS_ON[cur].fetch_add(1, Ordering::Relaxed); prof_note(cur_rsp); stuck_note(0, cur, cur_rsp); }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe {
        // We do not touch a task in limbo (see `BSP_LIMBO`): its RSP points at a fresh
        // context for an AP, the current BSP context dies. One cannot judge by `CPU_OF!=0` —
        // that is also the window of a move with a rollback.
        if !idle && !limbo {
            RSP[cur] = cur_rsp; ctx_saved(cur);
            FSBASE[cur] = rdmsr(IA32_FS_BASE); // save the current thread's TLS
            park_dead_in_user(cur, cur_rsp);
        }
    }
    tp(5, &mut tpt);
    // THE NETWORK STACK'S LOCK is held by this core — we do not preempt. The lock is owned by
    // THE CORE (see netsys), and a preemption would carry the task to another core, leaving
    // the lock "occupied by this one": the next entry into the stack would wait for an owner that
    // is no longer on this core. The section is short (one pass of the stack), so the price
    // of a refusal is one cycle of the scheduler rather than a lost preemption.
    let tk = if hw_tick { SW_TICK } else { 0 };
    if crate::netsys::holds_lock(0) { strace_note(0, cur0, cur0, tk | SW_STAY_LOCK, usize::MAX); return bsp_stay(idle, cur0, cur_rsp); }
    // THE FS/DEVICE LOCK is held by THE CURRENT task, and the tick came to us through its
    // interrupt window (`fs::io_window`, `talaria.iowin=1`) — we promised to return
    // the core to the owner; with `iowin=2` there is no promise and the owner may be carried away.
    if !idle && crate::fs::stay_for_locks(cur0) { strace_note(0, cur0, cur0, tk | SW_STAY_LOCK, usize::MAX); return bsp_stay(idle, cur0, cur_rsp); }
    // A directed switch: the waker's hint (a futex wake "into the hands") has priority
    let hint = HINT[0].0.swap(usize::MAX, Ordering::SeqCst);
    let hint_ok = hint < n && (hint != cur || idle)
        && !not_ready(hint) && CPU_OF[hint].load(Ordering::SeqCst) == 0
        && !crate::sync::blocked_for_sched(hint);
    // A tick without a hint caught a task that has been running for less than `talaria.tickmin` —
    // we leave it (see `TICKMIN_NS`). A hint is stronger than the threshold.
    if hw_tick && !hint_ok && !idle && !limbo && tickmin_stay(0) { strace_note(0, cur0, cur0, tk | SW_STAY_MIN, hint); return bsp_stay(idle, cur0, cur_rsp); }
    // The BSP chooses only its own tasks (CPU_OF==0), those sleeping on a futex word are skipped
    let mut next = if hint_ok { hint } else { (cur + 1) % n };
    let mut tries = 0;
    if !hint_ok {
        // The same policy as with an AP (`talaria.reqfirst=`, see `pick_next`), and with
        // the same two reservations: only A REAL tick (a task that gave up the core
        // itself cannot be executing — it cannot be "left"), and not in
        // idle/limbo, where `cur` is no longer the task that runs.
        let guard = hw_tick && !idle && !limbo;
        let (pick, stay_req) = if guard { pick_next(0, cur, n) } else { (scan_next(0, cur, n, false), false) };
        if stay_req { strace_note(0, cur0, cur0, tk | SW_STAY_REQ, hint); return bsp_stay(idle, cur0, cur_rsp); }
        match pick { Some(t) => next = t, None => tries = n }
    }
    tp(6, &mut tpt);
    let bsp_pickable = hint_ok || tries < n || (!not_ready(next) && CPU_OF[next].load(Ordering::SeqCst) == 0 && !crate::sync::blocked_for_sched(next));
    if !bsp_pickable {
        let mut any_alive = false;
        // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
        // touches, and what crosses cores is atomic (see the module's note).
        for i in 0..n { if unsafe { !DEAD[i] } || dying(i) { any_alive = true; break; } }
        // the live ones are only on the APs — the BSP stays in the current context (limbo/idle)
        if any_alive { strace_note(0, cur0, cur0, tk | SW_STAY_NONE, hint); return bsp_stay(idle, cur0, cur_rsp); }
    }
    if !bsp_pickable { // all the tasks are dead
        crate::println!("sched: all the {} task(s) have finished (COW copies: {})", n, crate::paging::cow_count());
        crate::metrics::dump(); // A2.1: the counters after a real workload (they grow correctly)
        crate::sync::dump_slow(); // the diagnostics: the slow futex waits (strace on)
        // Under the supervisor: return control to it (a longjmp from the timer's context —
        // the supervisor's stack (the boot stack) is intact, the tasks live on their own kernel stacks).
        if crate::supervise::RECOVERY_ARMED.load(Ordering::SeqCst) {
            crate::supervise::RECOVERY_ARMED.store(false, Ordering::SeqCst);
            crate::supervise::CRASH_VEC.store(0, Ordering::SeqCst);
            // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row
            // it touches, and what crosses cores is atomic (see the module's note).
            unsafe { crate::supervise::tal_longjmp(core::ptr::addr_of_mut!(crate::supervise::RECOVERY), 2); }
        }
        // A10: the workload has finished — that is THE END OF A SUITE ITEM rather than "the kernel has hung".
        // Previously there was a bare `loop { hlt }` here: on bare metal that meant that
        // the whole suite of modes stalled at the very first item with a user workload (smp2),
        // while the log's last lines — together with the test's result — never reached the
        // disk, because the trace's flush is done exactly by park. From the outside that looked
        // like a hang of the kernel inside a mode's branch. `park` without a suite behaves
        // exactly like the former loop (a flush of the trace + a hlt).
        crate::suite::park();
    }
    // ANNOUNCE the choice and verify it with the thieves. On the BSP a task's occupancy is marked not
    // with the AP_TAKEN flag but with the CURRENT field, so previously it was forbidden
    // to steal at all — and two threads that settled on the BSP could not be spread out by anybody: the speedup
    // on four cores came out now 3.9x, now 2.3x, by lot. Now the sides
    // announce themselves in the opposite order (the BSP: CURRENT, then a read of AP_TAKEN;
    // a thief: AP_TAKEN, then a read of CURRENT) — with any interleaving at least one
    // sees the other, and both cannot back off silently: the task simply waits for the next
    // round.
    // We are still on `cur`'s kernel stack — see `STACK_BUSY`; at a back-off the flag is removed right here.
    let leaving = !idle && next != cur;
    if leaving { STACK_BUSY[cur].store(true, Ordering::SeqCst); crate::percpu::set_off_stack(cur as u64); }
    CURRENT.store(next, Ordering::SeqCst);
    if idle { BSP_IDLE.store(false, Ordering::SeqCst); }
    core::sync::atomic::fence(Ordering::SeqCst);
    // The second side of the same protocol is the move (`migrate_idle_task`): it
    // sets CPU_OF and then reads CURRENT; we set CURRENT and then read
    // CPU_OF. We saw a foreign core — the task is already promised to it, we back off.
    if AP_TAKEN[next].load(Ordering::SeqCst) || CPU_OF[next].load(Ordering::SeqCst) != 0 {
        CURRENT.store(cur, Ordering::SeqCst);      // a thief or a move managed it — we stay where we were
        if idle { BSP_IDLE.store(true, Ordering::SeqCst); }
        if leaving { STACK_BUSY[cur].store(false, Ordering::SeqCst); crate::percpu::set_off_stack(u64::MAX); }
        strace_note(0, cur0, cur0, tk | SW_STAY_TAKEN, hint); return bsp_stay(idle, cur0, cur_rsp);
    }
    if next != cur { stack_wait(next); }
    strace_note(0, cur0, next, tk | if hint_ok { SW_HINT } else if hint < n { SW_HINT_BAD } else { SW_ROT }, hint);
    if !idle && next != cur && CPU_OF[cur].load(Ordering::SeqCst) == 0 {
        crate::sync::demote_preempted_sleeper(cur); // a sleeper in kwait loses the BSP: state->2 + a recheck
    }
    if !idle && next != cur {
        BSP_LIMBO.store(false, Ordering::SeqCst);    // we have left the old stack — the limbo is over
        note_left_cpu(cur);                          // see `LAST_OFF`: the BSP's cache is still its own
        acct_run(0, cur); acct_switch(0, cur);       // the time on the processor and the switch go into the top/ps counters
        note_ready_off(cur);                         // preempted or fallen asleep — for `RQ_TSC` those are different things
    }
    // A12: the first N switches go into the log. The pulse is printed BEFORE the decision and does not
    // say WHERE the scheduler went; on metal everything stalls exactly at the first
    // transition to a freshly created thread, and that is the only thing that was missing.
    {
        let left = SWITCH_TRACE.load(Ordering::Relaxed);
        if left > 0 {
            SWITCH_TRACE.store(left - 1, Ordering::Relaxed);
            // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row
            // it touches, and what crosses cores is atomic (see the module's note).
            let rfl = unsafe { core::ptr::read_volatile((RSP[next] + 17 * 8) as *const u64) };
            crate::println!("a12: the switch t{} -> t{} rsp={:#x} cr3={:#x} rip={:#x} rflags={:#x}",
                // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose
                // row it touches, and what crosses cores is atomic (see the module's note).
                cur, next, unsafe { RSP[next] }, unsafe { CR3[next] },
                // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose
                // row it touches, and what crosses cores is atomic (see the module's note).
                unsafe { core::ptr::read_volatile((RSP[next] + 15 * 8) as *const u64) }, rfl);
        }
    }
    crate::percpu::set_cur_task(next as u64);
    note_on_cpu(0);
    note_run_start(next);
    // SAFETY: C4 — this core's machine state (the registers/MSRs the switch is made of).
    unsafe {
        if KSTACK[next] != 0 { crate::user::set_rsp0(KSTACK[next]); } // the thread's kernel stack
        wrmsr(IA32_FS_BASE, FSBASE[next]);                            // restore the TLS
        // We do NOT switch CR3 here: rust_schedule may be executing on the user stack of
        // the interrupted task (e.g. the ring0 exit loop), which is not in the target AS.
        // We give CR3 to the stub — it will switch AFTER the transition to the target's kernel stack
        // (RSP[next] is in the common GB0, mapped in all the ASes). 0 = do not switch (threads).
        // We reload CR3 ONLY at a change of the address space. Previously it
        // was written always, while a `mov cr3` is a full TLB flush (we do not use
        // PCID): a switch between the threads of ONE process, that is, the most
        // frequent case, cost the workload its whole TLB. On streaming queries
        // (ClickHouse) that is noticeably more expensive than the switch itself.
        // ...except for the case when the core was asleep while a neighbour was meanwhile removing
        // mappings without calling (`paging::TLB_LAZY`): then we load CR3 even with
        // the same AS — and that is exactly the deferred flush.
        let cur_cr3 = crate::paging::current_cr3() & !0xFFF;
        let same = CR3[next] == cur_cr3 || CR3[next] == 0;
        crate::percpu::set_sched_cr3(if same && !crate::paging::lazy_tlb_take() { 0 } else if CR3[next] == 0 { cur_cr3 } else { CR3[next] });
        sigrsp_switch(cur, next); // the user-rsp substitution belongs to the task
        if FPU_VALID[next] { fpu_restore(core::ptr::addr_of!(FPU[next]) as *const u8); } // the target's FPU/SSE/AVX
        tp(7, &mut tpt);
        ctx_enter(next, 0, RSP[next]);
        RSP[next]
    }
}

/// A return INTO THE SAME task after its FPU/SSE was saved: restore
/// the registers that the tick managed to clobber (see the start of `ap_schedule`).
#[inline]
fn stay(cur: usize, cur_rsp: u64) -> u64 {
    // SAFETY: C2 + C3 — a context switch: the task has been claimed by this core, and the call does
    // not return onto this stack.
    if cur < NTASKS && unsafe { FPU_VALID[cur] } {
        // SAFETY: C2 + C3 — a context switch: the task has been claimed by this core, and the call
        // does not return onto this stack.
        unsafe { fpu_restore(core::ptr::addr_of!(FPU[cur]) as *const u8); }
    }
    cur_rsp
}
/// The round robin of a core OF THE COMMON POOL other than the BSP. It differs from the BSP path by
/// what is absent here: neither the supervisor, nor the housekeeping, nor "all the tasks have finished" —
/// that is the control plane's business. It is empty on this core — we return into the pickup loop.
/// SESSION 32'S INSTRUMENT: THE SCHEDULER'S LATEST DECISIONS PER CORE.
///
/// It is needed in order to answer the question "why did a ready reader not get a core" —
/// the counters say only that the core switched 1-3 times past it during its
/// wait. A ring per core, a record at every entry into the scheduler (and at
/// "we stay" too): the cycle, from where, to where, WHY and what the hint was.
/// It is read by `netsys::dl_close` for a slow delivery (see `DlDump`).
#[derive(Clone, Copy)]
pub struct SchedEv { pub tsc: u64, pub rip: u64, pub from: u8, pub to: u8, pub why: u8, pub hint: u8 }
pub const SEV0: SchedEv = SchedEv { tsc: 0, rip: 0, from: 255, to: 255, why: 0, hint: 255 };
/// Where the tick caught the interrupted task: 3 — in ring 3, otherwise an address in the kernel
/// (resolved by `addr2line` over the kernel's binary). It is written at the start of the tick's
/// handler, it is read by `strace_note` for the records with the `SW_TICK` bit.
pub static TICK_RIP: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
#[inline]
pub fn tick_rip_note(cpu: usize, frame: u64) {
    if cpu >= MAXCPU || frame == 0 { return; }
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    let (rip, cs) = unsafe {
        (core::ptr::read_volatile((frame + 15 * 8) as *const u64),
         core::ptr::read_volatile((frame + 16 * 8) as *const u64))
    };
    TICK_RIP[cpu].store(if cs & 3 == 3 { 3 } else { rip }, Ordering::Relaxed);
}
pub const STRACE_N: usize = 128;
pub static mut STRACE: [[SchedEv; STRACE_N]; MAXCPU] = [[SEV0; STRACE_N]; MAXCPU];
pub static STRACE_POS: [AtomicUsize; MAXCPU] = [const { AtomicUsize::new(0) }; MAXCPU];
/// The reasons (`why`): bit 0x80 — a real tick (otherwise a software yield);
/// the low ones: 1 the hint was applied, 2 the hint was rejected (a foreign core / not
/// ready) -> a rotation, 3 a rotation without a hint, 4 we stayed: the stack's lock,
/// 5 we stayed: there are no ready ones of our own, 6 we stayed: the task was intercepted, 7 a steal.
pub const SW_HINT: u8 = 1; pub const SW_HINT_BAD: u8 = 2; pub const SW_ROT: u8 = 3;
pub const SW_STAY_LOCK: u8 = 4; pub const SW_STAY_NONE: u8 = 5; pub const SW_STAY_TAKEN: u8 = 6; pub const SW_STEAL: u8 = 7;
/// 8 we stayed: the tick caught a task that has been running for less than `talaria.tickmin` (see `TICKMIN_NS`).
pub const SW_STAY_MIN: u8 = 8;
/// 9 we stayed: the task HAS A REQUEST IN PROGRESS, while the only candidate is a network waiter
/// (`talaria.reqfirst=`, see `REQFIRST_NS`).
pub const SW_STAY_REQ: u8 = 9;
pub const SW_TICK: u8 = 0x80;
/// `talaria.tickmin=<ns>` — THE MINIMUM RUN BEFORE A ROTATION BY A TICK. Session 32:
/// a core's timer is periodic (100 us with a live network waiter), and the tick came in
/// an arbitrary phase of a request: the analyses "the thread was taken off the core in the middle of read->write"
/// showed `[+12 a tick t11->t12 a rotation @r3]` — a mysql worker ran 12-14
/// us in ring 3 and was taken off by a rotation; the rotation returned the core to it only after
/// a full round of spinners and hints (t12->t0->t12->t13->t0->t11, 70-85 us).
/// That was exactly the class "a low rate, a wakeup": the p99 of mysql pk r7566 was 163 us
/// against Linux's 83 with an equal median. A rotation by a tick without a hint now
/// does not take off a task that has run less than the threshold; a hint (a futex wake "into the hands",
/// a preemption at a wakeup) stays in force — the threshold is no obstacle to it.
/// 0 — a rotation unconditionally (as it was before session 32).
///
/// THE DEFAULT OF 50 US IS BY MEASUREMENT rather than by taste. PostgreSQL select1 r4051 on
/// the c6i.xlarge stand (smp=2, nine tasks on two cores): p99 80 us with 0 and
/// 67 with 50 us (Linux 63), p99.9 90 -> 77; the class of turnarounds of 40-60 us 3.7 % ->
/// 0.07 %, the wait of a ready reader of 20-50 us 5.9 % -> 1.1 %; the tick left a task
/// 744 thousand times over a run — that is, almost every tick on a core with waiters
/// caught a task that had run less than the threshold (the waiters yield every
/// 5 us themselves) and took it off for nothing. The threshold's price is bounded from above: a foreign ready
/// task waits no longer than one extra tick period, while a hint and a preemption
/// at a wakeup do not obey the threshold. The median meanwhile is +2 us (51 -> 53).
pub static TICKMIN_NS: AtomicU64 = AtomicU64::new(50_000);
/// STEP 2 OF SESSION 43: "A REQUEST IN PROGRESS DOES NOT YIELD THE CORE TO A NETWORK WAITER".
///
/// Step 1's instrument (`talaria.appprof=1`, `netprof::dump_sched`) named the neighbour
/// that takes the core from a thread IN THE MIDDLE OF A REQUEST, with a number: under the workload
/// `mongodb insert` it is in 94-100 % of the rotations A NETWORK WAITER, in 0-6 % another thread
/// in a request, and not once a kernel thread or ring 3 without a request. Which means the policy
/// exists and it is a narrow one: taking a thread off for the sake of a waiter is not needed, while for the sake of another
/// thread in a request it may and must be done (otherwise it is a long quantum, and that is measured and
/// harmful: `tickmin=200000` WIDENED the band, session 42).
///
/// The protection is bounded from above BY THE TIME SINCE THE REQUEST'S START (`talaria.reqfirst=`,
/// 0 — off): a request's own work at `p99.9` is 113 us, so
/// the landmark is 150-200 us. A hung thread does not hold the core: the ceiling expires, and
/// the waiter gets its own.
pub static REQFIRST_NS: AtomicU64 = AtomicU64::new(0);
/// The cycle of the request's start per thread (0 — there is no request in progress). It is raised in the same place
/// where the instrument's `read->write` stage is (a `read`/`recvmsg` returned data), it is removed
/// at the entry into `write`/`sendmsg`. The price is one store per boundary, always.
static REQ_T0: [AtomicU64; NTASKS] = [const { AtomicU64::new(0) }; NTASKS];
/// How many times the policy left a thread on a core (per core) and how many times the ceiling
/// had already expired — "the reason before / its disappearance after" by the same instrument.
pub static REQFIRST_STAY: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
pub static REQFIRST_CAP: AtomicU64 = AtomicU64::new(0);
#[inline] pub fn req_begin() {
    let t = current_id();
    if t < NTASKS { REQ_T0[t].store(crate::tsc::rdtsc(), Ordering::Relaxed); }
}
#[inline] pub fn req_end() {
    let t = current_id();
    if t < NTASKS { REQ_T0[t].store(0, Ordering::Relaxed); }
}
/// Does the thread have a request in progress AND has the protection's ceiling not expired.
#[inline]
pub fn req_guarded(t: usize) -> bool {
    let cap = REQFIRST_NS.load(Ordering::Relaxed);
    if cap == 0 || t >= NTASKS { return false; }
    let t0 = REQ_T0[t].load(Ordering::Relaxed);
    if t0 == 0 { return false; }
    let hz = crate::tsc::calibrate_cached().max(1);
    let ns = crate::tsc::rdtsc().wrapping_sub(t0).saturating_mul(1_000) / (hz / 1_000_000).max(1);
    if ns >= cap { REQFIRST_CAP.fetch_add(1, Ordering::Relaxed); return false; }
    true
}
/// THE GATE `reqfirst`: the policy's decision in one line — "leave the core to the thread `cur`
/// because the candidate `cand` is a network waiter". It is separated from the circular search
/// on purpose: the starvation is checked exactly here (the ceiling has expired — the policy stays silent).
pub fn reqfirst_probe(cur: usize, cand: usize) -> bool {
    req_guarded(cur) && crate::netsys::is_net_waiter(cand)
}
/// Raise/remove the sign "a request in progress" for A FOREIGN thread (the gate only: on a live
/// machine it is set by `netprof::mark_app_in` for the current one).
pub fn req_probe(t: usize, on: bool) {
    if t >= NTASKS { return; }
    REQ_T0[t].store(if on { crate::tsc::rdtsc() } else { 0 }, Ordering::Relaxed);
}

/// The scheduler's tables as `rqpick` asks about them: the rotation and the policy
/// `reqfirst` live there (they are tested on the host), the state is here.
struct Runq(usize);
impl crate::rqpick::Threads for Runq {
    fn n(&self) -> usize { self.0 }
    fn ready(&self, t: usize) -> bool { !not_ready(t) && !crate::sync::blocked_for_sched(t) }
    fn cpu(&self, t: usize) -> usize { CPU_OF[t].load(Ordering::SeqCst) as usize }
    fn net_waiter(&self, t: usize) -> bool { crate::netsys::is_net_waiter(t) }
}

/// The circular search for this core's next READY task, starting from `cur + 1`.
/// `skip_waiter` is the policy `talaria.reqfirst=`: we skip the network waiters.
fn scan_next(cpu: usize, cur: usize, n: usize, skip_waiter: bool) -> Option<usize> {
    crate::rqpick::scan(&Runq(n), cpu, cur, skip_waiter)
}
/// The policy's decision for the core `cpu` that is taking the thread `cur` off: whom to choose.
/// `Some(t)` — that task; `None` — there are no ready ones of our own (further on as before:
/// a steal or "we stay"). `stay` — the policy shielded the candidate, we stay.
fn pick_next(cpu: usize, cur: usize, n: usize) -> (Option<usize>, bool) {
    match crate::rqpick::pick(&Runq(n), cpu, cur, req_guarded(cur)) {
        crate::rqpick::Pick::Take(t) => (Some(t), false),
        crate::rqpick::Pick::Stay => {
            if cpu < MAXCPU { REQFIRST_STAY[cpu].fetch_add(1, Ordering::Relaxed); }
            (None, true)
        }
        crate::rqpick::Pick::Nothing => (None, false),
    }
}

/// How many times a tick left a task on a core because of `TICKMIN_NS` (per core) —
/// the instrument "the reason before / its disappearance after".
pub static TICKMIN_STAY: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
#[inline]
fn tickmin_stay(cpu: usize) -> bool {
    let m = TICKMIN_NS.load(Ordering::Relaxed);
    if m == 0 || cpu >= MAXCPU { return false; }
    if ran_ns_on(cpu) >= m { return false; }
    TICKMIN_STAY[cpu].fetch_add(1, Ordering::Relaxed);
    true
}
/// THE TICK'S LENGTH: from the entry into the handler (`TICK_T0`) to the writing of the decision. Session 32:
/// all the analyses of slow deliveries ran into a tick that entered BEFORE the frame and
/// decided 120-200 us later (the flushing of a `stat` block); an instrument must see itself.
/// The buckets: <5, <20, <50, <100, <200, 500+ us.
static TICK_T0: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
pub static TICK_DUR_HIST: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
pub static TICK_DUR_MAX: AtomicU64 = AtomicU64::new(0);
/// The worst tick: the core | the tick's rip << 8.
pub static TICK_DUR_MAX_CTX: AtomicU64 = AtomicU64::new(0);
#[inline] fn tick_t0(cpu: usize) { if cpu < MAXCPU { TICK_T0[cpu].store(crate::tsc::rdtsc(), Ordering::Relaxed); } }
fn tick_dur_note(cpu: usize, now: u64) {
    let t0 = TICK_T0[cpu].swap(0, Ordering::Relaxed);
    if t0 == 0 { return; }
    let us = now.wrapping_sub(t0) / (crate::tsc::calibrate_cached() / 1_000_000).max(1);
    let b = match us { 0..=4 => 0, 5..=19 => 1, 20..=49 => 2, 50..=99 => 3, 100..=499 => 4, _ => 5 };
    TICK_DUR_HIST[b].fetch_add(1, Ordering::Relaxed);
    if us > TICK_DUR_MAX.load(Ordering::Relaxed) {
        TICK_DUR_MAX.store(us, Ordering::Relaxed);
        TICK_DUR_MAX_CTX.store(cpu as u64 | TICK_RIP[cpu].load(Ordering::Relaxed) << 8, Ordering::Relaxed);
    }
}
/// A HISTOGRAM OF THE DURATIONS (the same buckets as at the tick's length) with the worst
/// value and its context. Session 32: the analyses of postgresql's slow deliveries
/// showed a gap of 100-140 us between a waiter's "yield" phase and the scheduler's
/// decision on core 0 — while "the tick's length" counts the hardware ticks alone, and
/// the waiters' software `int 0x41` (170 thousand a second per core) did not get into it.
pub struct DurHist { pub hist: [AtomicU64; 6], pub max: AtomicU64, pub ctx: AtomicU64 }
impl DurHist {
    pub const fn new() -> Self { DurHist { hist: [const { AtomicU64::new(0) }; 6], max: AtomicU64::new(0), ctx: AtomicU64::new(0) } }
    pub fn note_cycles(&self, cy: u64, ctx: u64) {
        let us = cy / (crate::tsc::calibrate_cached() / 1_000_000).max(1);
        let b = match us { 0..=4 => 0, 5..=19 => 1, 20..=49 => 2, 50..=99 => 3, 100..=499 => 4, _ => 5 };
        self.hist[b].fetch_add(1, Ordering::Relaxed);
        if us > self.max.load(Ordering::Relaxed) { self.max.store(us, Ordering::Relaxed); self.ctx.store(ctx, Ordering::Relaxed); }
    }
    pub fn line(&self, what: &str) {
        let h: [u64; 6] = core::array::from_fn(|i| self.hist[i].load(Ordering::Relaxed));
        let c = self.ctx.load(Ordering::Relaxed);
        crate::println!("stat: {}, us: <5:{} <20:{} <50:{} <100:{} <500:{} 500+:{}; the worst {} us on core {} t{}->t{}",
            what, h[0], h[1], h[2], h[3], h[4], h[5], self.max.load(Ordering::Relaxed), c & 0xFF, (c >> 8) & 0xFF, (c >> 16) & 0xFF);
    }
}
/// A software yield: from the entry into the `int 0x41` handler to the writing of the decision.
static SY_T0: [AtomicU64; MAXCPU] = [const { AtomicU64::new(0) }; MAXCPU];
pub static SY_DUR: DurHist = DurHist::new();
#[inline] fn sy_t0(cpu: usize) { if cpu < MAXCPU { SY_T0[cpu].store(crate::tsc::rdtsc(), Ordering::Relaxed); } }
fn sy_dur_note(cpu: usize, now: u64, from: usize, to: usize) {
    let t0 = SY_T0[cpu].swap(0, Ordering::Relaxed);
    if t0 == 0 { return; }
    SY_DUR.note_cycles(now.wrapping_sub(t0), cpu as u64 | (from.min(255) as u64) << 8 | (to.min(255) as u64) << 16);
}
/// A network waiter's yield window: how long `ready_peer` went (the poll of the ready neighbours)
/// before the yield itself — the second half of the same gap (see `netsys::yield_window`).
pub static RP_DUR: DurHist = DurHist::new();
/// The wait for the card (`DevCell::take_bounded`) in a receive pass (the ctx "t"=1) and
/// in a call (4): after ceb2f53 both WAIT for the holder instead of a silent skip,
/// and the price of the wait was not counted anywhere.
pub static DEV_WAIT: DurHist = DurHist::new();
pub fn tick_dur_dump() {
    let h: [u64; 6] = core::array::from_fn(|i| TICK_DUR_HIST[i].load(Ordering::Relaxed));
    let c = TICK_DUR_MAX_CTX.load(Ordering::Relaxed);
    crate::println!("stat: the tick's length (the entry -> the decision), us: <5:{} <20:{} <50:{} <100:{} <500:{} 500+:{}; the worst {} us on core {} @{:#x}",
        h[0], h[1], h[2], h[3], h[4], h[5], TICK_DUR_MAX.load(Ordering::Relaxed), c & 0xFF, c >> 8);
    SY_DUR.line("the length of a software yield (the entry -> the decision)");
    RP_DUR.line("the yield window: ready_peer before the yield");
    DEV_WAIT.line("the wait for the card under a holder (1 a receive / 4 a call)");
}
/// WHERE A SOFTWARE YIELD COMES FROM (per core, it is read and extinguished by `strace_note`; for
/// the records without the tick's bit it is put into the `rip` field). Session 32: the app analyses
/// showed that a mysql worker gives up the core itself 16-18 us after a read
/// ("a yield rotation") and gets it back after a full round — by the ring of decisions
/// alone one cannot say whose yield it is: a network waiter's window, a parking on
/// a futex, a `sched_yield` (InnoDB's spin latches) or the FS lock.
pub const YO_NET: u8 = 1; pub const YO_FUTEX: u8 = 2; pub const YO_SYSYIELD: u8 = 3; pub const YO_FS: u8 = 4;
pub const YO_SWTCH: u8 = 5; pub const YO_WAKE: u8 = 6;
pub const YO_NAMES: [&str; 7] = ["?", "the network", "futex", "sched_yield", "the fs", "swtchto", "wake"];
static YIELD_ORIGIN: [AtomicU8; MAXCPU] = [const { AtomicU8::new(0) }; MAXCPU];
#[inline]
pub fn yield_origin(k: u8) {
    let c = crate::percpu::cpu_id();
    if c < MAXCPU { YIELD_ORIGIN[c].store(k, Ordering::Relaxed); }
}
#[inline]
pub fn strace_note(cpu: usize, from: usize, to: usize, why: u8, hint: usize) {
    if cpu >= MAXCPU { return; }
    // Step 1's instrument of session 43: a thread is taken off a core in the middle of a request — who
    // got the core (see `netprof::app_sched_note`). It is off — one read of
    // an atomic; "we stayed" (from == to) does not count as a rotation.
    if from != to { crate::netprof::app_sched_note(cpu, from, to); }
    let now = crate::tsc::rdtsc();
    if why & SW_TICK != 0 { tick_dur_note(cpu, now); } else { sy_dur_note(cpu, now, from, to); }
    let p = STRACE_POS[cpu].fetch_add(1, Ordering::Relaxed);
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    unsafe {
        STRACE[cpu][p % STRACE_N] = SchedEv { tsc: now,
            rip: if why & SW_TICK != 0 { TICK_RIP[cpu].load(Ordering::Relaxed) } else { YIELD_ORIGIN[cpu].swap(0, Ordering::Relaxed) as u64 },
            from: from.min(255) as u8, to: to.min(255) as u8, why, hint: hint.min(255) as u8 };
    }
}
/// Take from the core `cpu`'s ring the records from the cycle `since` (and `before` records before
/// it — for the context), the old ones first. It returns how many were put into `out`.
pub fn strace_take(cpu: usize, since: u64, before: usize, out: &mut [SchedEv]) -> usize {
    if cpu >= MAXCPU || out.is_empty() { return 0; }
    let end = STRACE_POS[cpu].load(Ordering::Relaxed);
    let start = end.saturating_sub(STRACE_N);
    // the first record with tsc >= since
    let mut first = end;
    let mut k = start;
    while k < end {
        // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
        // touches, and what crosses cores is atomic (see the module's note).
        let e = unsafe { STRACE[cpu][k % STRACE_N] };
        if e.tsc.wrapping_sub(since) < (1u64 << 62) { first = k; break; }
        k += 1;
    }
    let mut from = first.saturating_sub(before).max(start);
    let mut n = 0;
    while from < end && n < out.len() {
        // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
        // touches, and what crosses cores is atomic (see the module's note).
        out[n] = unsafe { STRACE[cpu][from % STRACE_N] };
        n += 1; from += 1;
    }
    n
}

fn ap_schedule(cpu: usize, cur_rsp: u64) -> u64 {
    // We are in the scheduler again: the CR3 switch from the previous entry is finished.
    AP_CR3_PREV[cpu].store(0, Ordering::Relaxed);
    crate::gdb::apply_if_changed(cpu); // the debugger's breakpoints go on all the cores
    // An EOI only to a real interrupt (a software `int 0x41` does not raise the ISR bit).
    let hw_tick = SOFT_YIELD[cpu].0.swap(0, Ordering::Relaxed) == 0;
    if hw_tick { tick_t0(cpu); } else { sy_t0(cpu); }
    if hw_tick {
        // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
        // touches, and what crosses cores is atomic (see the module's note).
        unsafe { crate::smp::lapic_eoi(); }
    }
    if hw_tick { tick_rip_note(cpu, cur_rsp); }
    let cur = crate::percpu::cur_task();
    // SESSION 32'S INSTRUMENT: a tick that caught the core ON THE PICKUP LOOP'S STACK but with the
    // current task already named. Between `set_cur_task(t)` and the `cli` in `enter_ctx`
    // the loop lives with IF=1: a tick here takes the loop's frame for the task's context and
    // puts it into `RSP[t]` — the task's real context is lost.
    {
        let top = ap_stack_top(cpu);
        if cur != u64::MAX && top != 0 && cur_rsp <= top && cur_rsp > top - 4 * 4096 {
            let n = LOOP_TICKS.fetch_add(1, Ordering::Relaxed);
            if n < 4 {
                crate::println!("sched: A TICK ON THE PICKUP LOOP'S STACK of core {} with cur_task=t{}: rsp={:#x} the top {:#x} hw_tick={} RSP[t]={:#x}",
                    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task
                    // whose row it touches, and what crosses cores is atomic (see the module's
                    // note).
                    cpu, cur, cur_rsp, top, hw_tick, unsafe { RSP[cur as usize] });
            }
        }
    }
    // THE INTERRUPTED TASK'S FPU/SSE COMES FIRST OF ALL, BEFORE ANY CODE OF THE TICK. The kernel is built
    // with SSE, and the compiler is free to zero an array on the stack through xmm0 in any
    // function that the tick calls "in passing". Previously the save stood lower,
    // after the EOI, the timer's adjustment and the counting of the ready ones, and it rested on the fact that
    // THOSE functions do not touch xmm. The steal push (`push_steal`) broke that silent
    // condition with one line `[0u8; MAXCPU]`: the workload got back
    // xmm0 = 0 and fell into a SIGSEGV at the address 0x8 (SObjectizer, `find_handler`)
    // with ZERO moves — the mechanism itself had not done anything yet, the corruption was given by
    // the counting of the candidates. `prof_note` under `talaria.prof` is of the same breed.
    // The second rule is `stay`: since the state has been saved, a return INTO THE SAME task
    // must restore it, otherwise everything the tick computed after the save
    // stays in its registers.
    if cur != u64::MAX && (cur as usize) < NTASKS {
        // SAFETY: C2 + C3 — a context switch: the task has been claimed by this core, and the call
        // does not return onto this stack.
        unsafe { fpu_save(core::ptr::addr_of_mut!(FPU[cur as usize]) as *mut u8); FPU_VALID[cur as usize] = true; }
    }
    if hw_tick {
        note_cpu_tick(cpu, cur != u64::MAX); crate::smp::timer_tick_reload();
        tick_gap_note(cpu, cur, cur_rsp);
        // A portion of the collected diagnostics block — from the tick of ANY core rather than
        // only of the BSP: with `talaria.sync=hlt` a waiting task holds its own core, and
        // the BSP's ticks were not enough for the whole block (the snapshots were skipped). The frequency
        // is kept by `stat_flush` itself (not more often than once every 5 ms per machine).
        crate::obs::stat_flush(150_000);
        let solo = cur == u64::MAX || !cpu_has_other_runnable(cpu, cur as usize);
        crate::smp::retune_timer(crate::smp::want_period_ns(cpu, solo));
        if !solo { push_steal(cpu); }
    }
    if hw_tick && cur != u64::MAX && (cur as usize) < NTASKS { TICKS_ON[cur as usize].fetch_add(1, Ordering::Relaxed); prof_note(cur_rsp); stuck_note(cpu, cur as usize, cur_rsp); }
    let n = NREADY.load(Ordering::SeqCst);
    // The core is idling in the pickup loop — there is nothing to switch (and it is NOT ALLOWED: cur_rsp
    // points at the loop's own stack rather than at a task's frame).
    if cur == u64::MAX { return cur_rsp; }
    let cur = cur as usize;
    // The network stack's lock is held by this core — we do not preempt (see the BSP's tick).
    let tk = if hw_tick { SW_TICK } else { 0 };
    if crate::netsys::holds_lock(cpu) { strace_note(cpu, cur, cur, tk | SW_STAY_LOCK, usize::MAX); return stay(cur, cur_rsp); }
    // The same as with the BSP: the owner of the FS/device lock opened an interrupt window
    // (`fs::io_window`) — the tick arrives, but the core stays with it (`iowin=1`).
    if crate::fs::stay_for_locks(cur) { strace_note(cpu, cur, cur, tk | SW_STAY_LOCK, usize::MAX); return stay(cur, cur_rsp); }
    if cur >= n { return stay(cur, cur_rsp); }
    // SAFETY: C4 — this core's machine state (the registers/MSRs the switch is made of).
    unsafe {
        RSP[cur] = cur_rsp; ctx_saved(cur);
        FSBASE[cur] = rdmsr(IA32_FS_BASE);
    }
    park_dead_in_user(cur, cur_rsp);
    // The waker's hint (a directed switch) applies only to ITS OWN core.
    // THE SLOT IS TAKEN ALWAYS rather than only when the hint is suitable. Previously
    // an unsuitable one lay there until the next suitable one — that is, forever, if
    // the addressee no longer woke up on that core — and every next
    // hint to the core counted as "overwritten by a foreign one". The BSP a floor above always took
    // the slot with a `swap`; here was the second half of the same error.
    // Nothing can be lost by this: a hint is set for an ALREADY woken task
    // (`WOKEN` is raised before `hint_set_on`), while an unsuitable one is not applicable later either.
    let hint = HINT[cpu.min(MAXCPU - 1)].0.swap(usize::MAX, Ordering::SeqCst);
    let hint_ok = hint < n && hint != cur && CPU_OF[hint].load(Ordering::SeqCst) as usize == cpu
        && !not_ready(hint) && !crate::sync::blocked_for_sched(hint);
    if hint_ok { HINT_USED.fetch_add(1, Ordering::Relaxed); }
    // A tick without a hint caught a task that has been running for less than `talaria.tickmin` —
    // we leave it (see `TICKMIN_NS`). A hint is stronger than the threshold.
    if hw_tick && !hint_ok && tickmin_stay(cpu) { strace_note(cpu, cur, cur, tk | SW_STAY_MIN, hint); return stay(cur, cur_rsp); }
    let mut why = tk | if hint_ok { SW_HINT } else if hint < n { SW_HINT_BAD } else { SW_ROT };
    let mut next = if hint_ok { hint } else { (cur + 1) % n };
    let mut claimed = false; // the task is already marked as occupied by us (the steal path)
    if !hint_ok {
        // THE POLICY `talaria.reqfirst=`: a thread with a request in progress does not yield
        // the core TO A NETWORK WAITER (see `pick_next`). It is off — the former circular
        // search word for word. Only A REAL tick: a task that gave up the core
        // itself (a yield, a wait) must not be left on the core — it will not run.
        let (pick, stay_req) = if hw_tick { pick_next(cpu, cur, n) } else { (scan_next(cpu, cur, n, false), false) };
        if stay_req { strace_note(cpu, cur, cur, tk | SW_STAY_REQ, hint); return stay(cur, cur_rsp); }
        let mut tries = 0;
        match pick { Some(t) => next = t, None => tries = n }
        if tries >= n {
            // There are no ready tasks of our own. If we ourselves sleep on a futex — the core is now
            // going to idle while a neighbour has a queue: we take the work from it.
            // Waiting for the periodic levelling (once every 32 ms) for a burst of
            // work makes no sense — ClickHouse lost three times as much on that.
            if crate::sync::blocked_for_sched(cur) {
                match crate::affinity::steal_candidate(cpu) {
                    // Is the donor the BSP? It declares its occupancy through CURRENT rather than
                    // through AP_TAKEN: we verify AFTER we have marked ourselves
                    // (the second half of the protocol, see the BSP's path and `ap_claim_steal`).
                    Some(t) if ap_claim_steal(t, cpu) => {
                        next = t;
                        claimed = true;
                        why = tk | SW_STEAL;
                    }
                    _ => { strace_note(cpu, cur, cur, tk | SW_STAY_NONE, hint); return stay(cur, cur_rsp) },
                }
            } else { strace_note(cpu, cur, cur, tk | SW_STAY_NONE, hint); return stay(cur, cur_rsp); } // we work ourselves
        }
    }
    if next == cur { strace_note(cpu, cur, cur, why, hint); return stay(cur, cur_rsp); }
    // OCCUPY the task atomically. With the appearance of work stealing it could be taken at this very
    // moment by a neighbour: it sets CPU_OF to itself and AP_TAKEN, while our rotation
    // chose a candidate ONLY by CPU_OF — and two cores would go to execute one
    // and the same context. We did not get it — we stay on our own.
    if !claimed && !ap_claim(next, cpu) { strace_note(cpu, cur, cur, tk | SW_STAY_TAKEN, hint); return stay(cur, cur_rsp); }
    stack_wait(next); // the core that released it may still be standing on its stack
    strace_note(cpu, cur, next, why, hint);
    crate::sync::demote_preempted_sleeper(cur); // a sleeper in kwait loses the core: state->2 + a recheck
    note_left_cpu(cur);                          // this core's cache is still its own — let the thieves wait
    acct_run(cpu, cur); acct_switch(cpu, cur);   // the same on a secondary core (see `RUN_TSC`)
    note_ready_off(cur);
    // The context is saved, but WE ARE STILL ON ITS STACK — the flag will be removed by the stub (see `STACK_BUSY`).
    STACK_BUSY[cur].store(true, Ordering::SeqCst); crate::percpu::set_off_stack(cur as u64);
    AP_TAKEN[cur].store(false, Ordering::SeqCst); // the task may be rehung onto another core
    // The task could have been rehung while it WAS EXECUTING: the balancer checks
    // "it is not executing" and moves `CPU_OF`, while between these two actions
    // we managed to enter it. The target core saw it occupied at that moment,
    // skipped it and fell asleep on its door — we ring once more, otherwise the task would stay
    // nobody's until the next foreign move.
    let owner = CPU_OF[cur].load(Ordering::SeqCst) as usize;
    // SAFETY: C1 — the task-indexed tables of the scheduler: this core owns the task whose row it
    // touches, and what crosses cores is atomic (see the module's note).
    if owner != cpu && owner != 0 && !unsafe { DEAD[cur] } { ap_ring(owner); }
    crate::percpu::set_cur_task(next as u64);
    note_on_cpu(cpu);
    note_run_start(next);
    // SAFETY: C4 — this core's machine state (the registers/MSRs the switch is made of).
    unsafe {
        if KSTACK[next] != 0 { crate::user::set_rsp0(KSTACK[next]); }
        wrmsr(IA32_FS_BASE, FSBASE[next]);
        // see above: one address space — we do not touch CR3, we do not lose the TLB
        let cur_cr3 = crate::paging::current_cr3() & !0xFFF;
        let same = CR3[next] == cur_cr3 || CR3[next] == 0;
        let to = if same && !crate::paging::lazy_tlb_take() { 0 } else if CR3[next] == 0 { cur_cr3 } else { CR3[next] };
        note_cr3(cpu, to);
        crate::percpu::set_sched_cr3(to); // the stub will switch AFTER the transition to the target's stack; with the TLB_LAZY mark — even with the same AS
        sigrsp_switch(cur, next);
        if FPU_VALID[next] { fpu_restore(core::ptr::addr_of!(FPU[next]) as *const u8); }
        ctx_enter(next, cpu, RSP[next]);
        RSP[next]
    }
}

unsafe extern "C" { fn timer_stub_sched(); }
/// Redirect the LAPIC timer to the scheduler (a context switch).
pub fn install() {
    crate::idt::set_vector(crate::idt::TIMER_VECTOR, timer_stub_sched as *const () as u64);
    crate::affinity::apply_stashed(); // the layout from the command line — the scheduler's cores are already sorted out
}
/// A12: the LAPIC timer's first tick — it is printed ONCE. Without it "the workload
/// is silent" is indistinguishable from "the timer does not tick": the scheduler is preemptive, and without
/// a tick a second thread will never get a core, while the first one will wait for it forever.
pub static FIRST_TICK: AtomicBool = AtomicBool::new(false);
/// `talaria.heartbeat=<seconds>` in nanoseconds (0 — off) and the time of the previous
/// pulse; the tick counter — so that one line shows whether the preemption is alive.
/// `talaria.heartbeat=N` — print the pulse every N ticks of the scheduler (0 — no).
pub static HEARTBEAT_TICKS: AtomicU64 = AtomicU64::new(0);
/// The real interrupts of the LAPIC timer (without the software yields). A zero with a live
/// scheduler = there is no preemption: see A12.
pub static HW_TICKS: AtomicU64 = AtomicU64::new(0);
/// A12: how many more context switches to print (`talaria.switches=N`).
pub static SWITCH_TRACE: AtomicU64 = AtomicU64::new(0);

/// The control plane without a thread of its own: the workload's main thread has moved to a
/// dedicated core (`pin=main:N`), the BSP stays idle. A timer's tick will enter
/// here, see `BSP_IDLE` and pick up a task as soon as one appears on the BSP —
/// while "all the tasks have finished" will work from here just as from a task.
/// # Safety
/// C2 + C3: it takes this core into the idle context and does not return; it must be
/// called with no task claimed by this core.
pub unsafe fn start_idle() -> ! {
    BSP_IDLE.store(true, Ordering::SeqCst);
    crate::percpu::set_cur_task(u64::MAX);
    crate::println!("sched: the control plane is idling — the workload has gone to the dedicated cores");
    loop {
        crate::obs::stat_drain();   // an idle BSP pays the printing debt too (see ap_loop)
        // ...and drives the stack just the same while the network is alive (see `netsys::idle_poll`).
        // There is nowhere to leave from here — the BSP's tick will take it away — so `bail` is always false.
        if crate::netsys::idle_poll(|| false) { continue; }
        crate::paging::halt_enter();
        core::arch::asm!("sti; hlt", options(nostack, preserves_flags));
        crate::paging::halt_exit();
    }
}

/// Enter the first task (RSP[0]) — switch from the BSP's boot context.
/// # Safety
/// C2 + C3: the first entry into a task on this core — the scheduler's tables must be
/// filled and the task claimed. It does not return: the boot stack is abandoned.
pub unsafe fn start() -> ! {
    if CR3[0] != 0 { crate::paging::switch(CR3[0]); } // the first process's address space
    // talaria.gdb=break: the breakpoints are set BEFORE the first instruction, already under the workload's AS
    // (the debugger reads its memory by its addresses).
    crate::gdb::maybe_break_at_start();
    crate::percpu::set_cur_task(0);
    let sp = RSP[0];
    // A12: the kernel's last line before ring 3. On a machine without a console telling
    // "we did not reach the entry" from "we entered and went silent" is impossible by anything else: on c6i.metal
    // the production path mode=cell reached this place and printed nothing further.
    crate::println!("sched: the entry into the first task (cr3={:#x} rsp={:#x})", CR3[0], sp);
    // The pop order must coincide with timer_stub_sched (r15 first, rax last) —
    // the frame is built with rax at the bottom (it is popped last). Otherwise rax<->r15 are swapped.
    // cli: a tick caught BETWEEN the mov rsp and the iretq would save the entry's context into
    // RSP[0] over the user frame (an eternal entry into itself). Under PVH the window was not caught,
    // under UEFI it is caught deterministically: a deferred interrupt of the firmware waits in the IRR,
    // it strikes at the very first window with IF=1. The task's IF will be restored by the iretq from the frame.
    core::arch::asm!(
        "cli",
        "mov rsp, {}",
        "pop r15", "pop r14", "pop r13", "pop r12", "pop r11", "pop r10", "pop r9", "pop r8",
        "pop rbp", "pop rdi", "pop rsi", "pop rdx", "pop rcx", "pop rbx", "pop rax",
        "iretq",
        in(reg) sp, options(noreturn)
    );
}

core::arch::global_asm!(r#"
.section .text
.global timer_stub_sched
timer_stub_sched:
    push %rax
    push %rbx
    push %rcx
    push %rdx
    push %rsi
    push %rdi
    push %rbp
    push %r8
    push %r9
    push %r10
    push %r11
    push %r12
    push %r13
    push %r14
    push %r15
    mov %rsp, %rdi          /* cur_rsp -> rust_schedule */
    call rust_schedule      /* rax = next rsp */
    mov %rax, %rsp          /* we have moved onto the target's kernel stack (in the common GB0) */
    mov %gs:56, %rcx        /* the task whose stack we have just left (MAX = none) */
    cmp $-1, %rcx
    je 3f
    lea STACK_BUSY(%rip), %rdx
    movb $0, (%rdx,%rcx)    /* the stack is free — the taking core may enter (see stack_wait) */
    movq $-1, %gs:56
3:
    mov %gs:40, %rcx        /* the CR3 of THIS core's target (0 = do not switch) */
    test %rcx, %rcx
    jz 2f
    mov %rcx, %cr3          /* switch the address space here — the stack is already a common one */
2:
    pop %r15
    pop %r14
    pop %r13
    pop %r12
    pop %r11
    pop %r10
    pop %r9
    pop %r8
    pop %rbp
    pop %rdi
    pop %rsi
    pop %rdx
    pop %rcx
    pop %rbx
    pop %rax
    iretq
"#, options(att_syntax));
