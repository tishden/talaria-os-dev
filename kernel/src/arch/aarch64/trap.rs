//! The exception vectors: sixteen entries, 0x80 bytes apart, in four groups (the
//! current EL with SP_EL0, the current EL with SP_EL1, a lower EL in AArch64, a lower
//! EL in AArch32), each of Sync / IRQ / FIQ / SError. An entry only branches out: a
//! save-and-call sequence does not fit in 32 instructions, so the stubs live after
//! the table. Two kinds of stub: an IRQ saves the register file, calls `irq`, restores
//! it and returns with `eret`; everything else calls `trap`, which prints the
//! syndrome (`ESR_EL1`), the faulting address (`FAR_EL1`), the return address
//! (`ELR_EL1`) and the registers, and halts — a diagnosis instead of a silent reset,
//! the x86 IDT's first job. The `SVC` and page-fault paths take the sync entries over
//! in phase 3 (docs/ARM64-PLAN.md §4.5).

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use super::{cpu, gic, timer};

/// The frame the stubs save: x0..x30, then the saved sp, elr, spsr, esr, far — 36 words.
/// ONE frame for every entry (an `svc`, a fault, an interrupt, from EL0 or EL1), which is
/// what docs/ARM64-PLAN.md §4.5 asked for: it plays `syscall::Frame` for the dispatcher,
/// `idt::ExcFrame` for the fault paths, and the saved context the scheduler switches. The
/// field names are the neutral ones the shared code reads (`nr`, `a1..a6`, `pc`, `flags`,
/// `usp`); the rest of the file is reachable by number through `x()`/`set_x()`.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Frame {
    pub a1: u64, pub a2: u64, pub a3: u64, pub a4: u64, pub a5: u64, pub a6: u64,  // x0..x5
    pub x6: u64, pub x7: u64,
    pub nr: u64,                                                                   // x8
    pub x9_18: [u64; 10],
    pub x19_28: [u64; 10],                                                         // callee-saved
    pub fp: u64,                                                                   // x29
    pub lr: u64,                                                                   // x30
    /// The interrupted stack: `SP_EL0` for an EL0 entry (restored on the way out — a signal's
    /// stack substitution is a write here), the EL1 sp before the frame for an EL1 entry.
    pub usp: u64,
    pub pc: u64,      // ELR_EL1
    pub flags: u64,   // SPSR_EL1
    pub esr: u64,
    pub far: u64,
}
impl Frame {
    /// Register `x<i>` (0..=30) by number.
    #[inline] pub fn x(&self, i: usize) -> u64 {
        debug_assert!(i < 31);
        unsafe { core::ptr::read((self as *const Frame as *const u64).add(i)) }
    }
    #[inline] pub fn set_x(&mut self, i: usize, v: u64) {
        debug_assert!(i < 31);
        unsafe { core::ptr::write((self as *mut Frame as *mut u64).add(i), v) }
    }
    /// A copy with the number and the arguments replaced (the dispatcher's translated copy).
    pub fn with_args(&self, nr: u64, a: [u64; 6]) -> Frame {
        let mut f = Frame { ..*self };
        f.nr = nr; f.a1 = a[0]; f.a2 = a[1]; f.a3 = a[2]; f.a4 = a[3]; f.a5 = a[4]; f.a6 = a[5];
        f
    }
}

core::arch::global_asm!(r#"
/* SAVE_FRAME el0: the 36-word frame (trap::Frame). The `usp` slot is SP_EL0 for an entry
   from EL0 and the EL1 sp before the frame for an entry from EL1. */
.macro SAVE_FRAME el0
    sub sp, sp, #288
    stp x0, x1, [sp, #0]
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    .if \el0
    mrs x1, sp_el0
    .else
    add x1, sp, #288
    .endif
    stp x30, x1, [sp, #240]
    mrs x2, elr_el1
    mrs x3, spsr_el1
    stp x2, x3, [sp, #256]
    mrs x2, esr_el1
    mrs x3, far_el1
    stp x2, x3, [sp, #272]
.endm

/* RESTORE_FRAME: by the FRAME's origin, not the stub's — a scheduling stub may enter a
   frame another kind of entry saved. SPSR.M == 0 (EL0t): SP_EL0 comes from the slot (a
   signal's stack substitution is a write there); an EL1 frame leaves SP_EL1 to the add.
   IRQ and FIQ are masked FIRST: the `svc` path comes back with them open (a parked
   syscall enables them to be woken), and an IRQ taken after the `msr elr_el1` below
   rewrites ELR/SPSR with ITS return — a pc inside this very macro — so the `eret` re-runs
   the tail of the restore with sp already past the frame (the netirq diskserver fault
   under KVM: the loads hit the page above the kernel stack). TCG never shows it: it
   checks interrupts at a translation block's end, and the restore is one block. */
.macro RESTORE_FRAME
    msr daifset, #3
    ldp x2, x3, [sp, #256]
    msr elr_el1, x2
    msr spsr_el1, x3
    and x4, x3, #0xF
    cbnz x4, 1f
    ldr x1, [sp, #248]
    msr sp_el0, x1
1:
    ldp x0, x1, [sp, #0]
    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x19, [sp, #144]
    ldp x20, x21, [sp, #160]
    ldp x22, x23, [sp, #176]
    ldp x24, x25, [sp, #192]
    ldp x26, x27, [sp, #208]
    ldp x28, x29, [sp, #224]
    ldr x30, [sp, #240]
    add sp, sp, #288
.endm

/* FP_SAVE: the 528-byte FP/SIMD state (q0..q31, fpcr, fpsr) below the frame, and its
   address into this core's block (+64) — `switch::fpu_save` copies from THERE, so the
   kernel's own use of the V registers between the entry and the scheduler cannot leak
   into a task's saved state. FP_RESTORE takes it back from sp. */
.macro FP_SAVE
    sub sp, sp, #528
    stp q0, q1, [sp, #0]
    stp q2, q3, [sp, #32]
    stp q4, q5, [sp, #64]
    stp q6, q7, [sp, #96]
    stp q8, q9, [sp, #128]
    stp q10, q11, [sp, #160]
    stp q12, q13, [sp, #192]
    stp q14, q15, [sp, #224]
    stp q16, q17, [sp, #256]
    stp q18, q19, [sp, #288]
    stp q20, q21, [sp, #320]
    stp q22, q23, [sp, #352]
    stp q24, q25, [sp, #384]
    stp q26, q27, [sp, #416]
    stp q28, q29, [sp, #448]
    stp q30, q31, [sp, #480]
    mrs x9, fpcr
    mrs x10, fpsr
    str x9, [sp, #512]
    str x10, [sp, #520]
    mrs x9, tpidr_el1
    cbz x9, 2f
    mov x10, sp
    str x10, [x9, #64]
2:
.endm
.macro FP_RESTORE
    ldr x9, [sp, #512]
    ldr x10, [sp, #520]
    msr fpcr, x9
    msr fpsr, x10
    ldp q0, q1, [sp, #0]
    ldp q2, q3, [sp, #32]
    ldp q4, q5, [sp, #64]
    ldp q6, q7, [sp, #96]
    ldp q8, q9, [sp, #128]
    ldp q10, q11, [sp, #160]
    ldp q12, q13, [sp, #192]
    ldp q14, q15, [sp, #224]
    ldp q16, q17, [sp, #256]
    ldp q18, q19, [sp, #288]
    ldp q20, q21, [sp, #320]
    ldp q22, q23, [sp, #352]
    ldp q24, q25, [sp, #384]
    ldp q26, q27, [sp, #416]
    ldp q28, q29, [sp, #448]
    ldp q30, q31, [sp, #480]
    add sp, sp, #528
.endm

/* FP_TAKE_PENDING: the FP/SIMD image the scheduler chose for the task being entered
   (`switch::fpu_restore` left its address in the core's block, +72) — loaded HERE, after
   the last Rust code of the switch, and the slot cleared. Clobbers x9-x11. */
.macro FP_TAKE_PENDING
    mrs x9, tpidr_el1
    cbz x9, 7f
    ldr x10, [x9, #72]
    cbz x10, 7f
    str xzr, [x9, #72]
    ldr x11, [x10, #512]
    msr fpcr, x11
    ldr x11, [x10, #520]
    msr fpsr, x11
    ldp q0, q1, [x10, #0]
    ldp q2, q3, [x10, #32]
    ldp q4, q5, [x10, #64]
    ldp q6, q7, [x10, #96]
    ldp q8, q9, [x10, #128]
    ldp q10, q11, [x10, #160]
    ldp q12, q13, [x10, #192]
    ldp q14, q15, [x10, #224]
    ldp q16, q17, [x10, #256]
    ldp q18, q19, [x10, #288]
    ldp q20, q21, [x10, #320]
    ldp q22, q23, [x10, #352]
    ldp q24, q25, [x10, #384]
    ldp q26, q27, [x10, #416]
    ldp q28, q29, [x10, #448]
    ldp q30, q31, [x10, #480]
7:
.endm

/* SCHED_TAIL: x0 = the frame to enter (from a handler that may have switched tasks),
   x19 = our FP area (our frame is 528 above it). The same frame: its FP state back from
   the area (and a pending image, if the scheduler named one for the task it then stayed
   on, is dropped — it is the same state). Another frame: the image the scheduler named
   (`fpu_restore` -> FP_TAKE_PENDING); free the stack we left (`off_stack` -> STACK_BUSY), switch the address
   space the scheduler chose (`sched_root`, +40) — the x86 tail of timer_stub_sched. */
.macro SCHED_TAIL
    add x1, x19, #528
    cmp x0, x1
    b.ne 5f
    mov sp, x19
    FP_RESTORE
    mrs x9, tpidr_el1
    cbz x9, 6f
    str xzr, [x9, #72]
    b 6f
5:
    mov sp, x0
    FP_TAKE_PENDING
    mrs x9, tpidr_el1
    ldr x10, [x9, #56]
    cmn x10, #1
    b.eq 3f
    adrp x11, STACK_BUSY
    add x11, x11, :lo12:STACK_BUSY
    add x11, x11, x10
    /* a store-RELEASE: the handler's last spills went onto the stack being freed, and a
       plain strb may become visible before them — the core that takes the stack would
       then have its own pushes overwritten (x86 is TSO; here it is the ordering) */
    stlrb wzr, [x11]
    mov x10, #-1
    str x10, [x9, #56]
3:
    ldr x10, [x9, #40]
    cbz x10, 6f
    msr ttbr0_el1, x10
    isb
    tlbi vmalle1
    dsb ish
    isb
6:
    RESTORE_FRAME
    eret
.endm

/* enter_frame_tail: sp = a saved frame (`switch::enter_saved_ctx`): the pending FP/SIMD
   image, the frame's restore, `eret`. */
.section .text
.global enter_frame_tail
enter_frame_tail:
    FP_TAKE_PENDING
    RESTORE_FRAME
    eret

.section .text
.balign 2048
.global vectors_el1
vectors_el1:
.macro VECTOR n
    .balign 128
    b vec_\n
.endm
    VECTOR 0
    VECTOR 1
    VECTOR 2
    VECTOR 3
    VECTOR 4
    VECTOR 5
    VECTOR 6
    VECTOR 7
    VECTOR 8
    VECTOR 9
    VECTOR 10
    VECTOR 11
    VECTOR 12
    VECTOR 13
    VECTOR 14
    VECTOR 15

.macro STUB_FATAL kind
vec_\kind:
    SAVE_FRAME 0
    mov x0, #\kind
    mov x1, sp
    bl trap
    b .
.endm

/* An IRQ, or a synchronous exception at EL1: the handler returns the frame to enter —
   the same one, or another task's after a tick that switched (`sched_tail_entry`). */
.macro STUB_SCHED kind, el0, handler
vec_\kind:
    SAVE_FRAME \el0
    FP_SAVE
    mov x19, sp
    mov x0, #\kind
    add x1, sp, #528
    bl \handler
    SCHED_TAIL
.endm

/* An `svc` or a fault from EL0: the dispatcher works on the frame in place; the FP/SIMD
   save obeys FPSAVE_ON (the svccost probe measures it off). */
.macro STUB_SYNC_EL0 kind
vec_\kind:
    SAVE_FRAME 1
    adrp x9, FPSAVE_ON
    ldr w9, [x9, :lo12:FPSAVE_ON]
    cbz w9, 1f
    FP_SAVE
    mov x0, #\kind
    add x1, sp, #528
    bl sync_el0
    FP_RESTORE
    b 2f
1:
    sub sp, sp, #528
    mov x0, #\kind
    add x1, sp, #528
    bl sync_el0
    add sp, sp, #528
2:
    RESTORE_FRAME
    eret
.endm

STUB_FATAL 0
STUB_SCHED 1, 0, irq_entry
STUB_FATAL 2
STUB_FATAL 3
STUB_SCHED 4, 0, sync_el1_entry
STUB_SCHED 5, 0, irq_entry
STUB_FATAL 6
STUB_FATAL 7

STUB_SYNC_EL0 8
STUB_SCHED 9, 1, irq_entry
STUB_FATAL 10
STUB_FATAL 11
STUB_FATAL 12
STUB_FATAL 13
STUB_FATAL 14
STUB_FATAL 15
"#);

/// Point `VBAR_EL1` at the table (on every core).
pub fn init() {
    unsafe extern "C" { static vectors_el1: u8; }
    unsafe {
        core::arch::asm!("msr vbar_el1, {}", "isb", in(reg) core::ptr::addr_of!(vectors_el1) as u64, options(nostack));
    }
}

fn kind_name(kind: u64) -> &'static str {
    const NAMES: [&str; 16] = [
        "sync from EL1t", "IRQ from EL1t", "FIQ from EL1t", "SError from EL1t",
        "sync from EL1h", "IRQ from EL1h", "FIQ from EL1h", "SError from EL1h",
        "sync from EL0 (AArch64)", "IRQ from EL0 (AArch64)", "FIQ from EL0 (AArch64)", "SError from EL0 (AArch64)",
        "sync from EL0 (AArch32)", "IRQ from EL0 (AArch32)", "FIQ from EL0 (AArch32)", "SError from EL0 (AArch32)",
    ];
    NAMES[(kind & 15) as usize]
}

/// The exception class (`ESR.EC`, bits 31:26) in words.
fn ec_name(esr: u64) -> &'static str {
    match (esr >> 26) & 0x3F {
        0x00 => "unknown", 0x01 => "WFI/WFE trapped", 0x07 => "FP/SIMD access (CPACR)", 0x0E => "illegal execution state",
        0x15 => "SVC (AArch64)", 0x18 => "MSR/MRS trapped", 0x19 => "SVE access", 0x20 => "instruction abort from a lower EL",
        0x21 => "instruction abort, same EL", 0x22 => "PC alignment", 0x24 => "data abort from a lower EL",
        0x25 => "data abort, same EL", 0x26 => "SP alignment", 0x2C => "FP exception", 0x2F => "SError",
        0x30 | 0x31 => "breakpoint", 0x32 | 0x33 => "software step", 0x34 | 0x35 => "watchpoint", 0x3C => "BRK",
        _ => "?",
    }
}

/// A fatal exception of the kernel's own. The first lines are the registers — they always
/// reach the console; then the block x86 prints on its `CRASH DUMP` path (`idt.rs`): the
/// task and its last syscall, what the abort was, the stack's words, the x29 chain, the
/// flight recorder, and the kernel's core dump when the operator asked for one. Every
/// later step reads more state than the one before it, so a SECOND fault inside the block
/// (a wild sp, a table the fault itself broke) stops at the registers already printed.
#[unsafe(no_mangle)]
extern "C" fn trap(kind: u64, f: &Frame) -> ! {
    static DEPTH: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
    let cpu = cpu::cpu_index().min(63);
    let depth = DEPTH[cpu].fetch_add(1, Ordering::Relaxed);
    // A COPY, taken before the first line: when the cause is two cores on one stack, the
    // other core's next interrupt frame lands on this one while it is being printed (the
    // first arm64 dump of such a crash showed two different pcs for one exception).
    let copy = *f;
    let f = &copy;
    crate::println!("[trap] {}: esr={:#x} ({}) far={:#x} elr={:#x} spsr={:#x} sp={:#x}",
        kind_name(kind), f.esr, ec_name(f.esr), f.far, f.pc, f.flags, f.usp);
    for i in (0..30).step_by(2) {
        crate::println!("[trap]   x{:<2}={:#018x} x{:<2}={:#018x}", i, f.x(i), i + 1, f.x(i + 1));
    }
    crate::println!("[trap]   x30={:#018x}", f.lr);
    // Before the per-core block exists there is no task, no scheduler and no FS to ask.
    if depth != 0 || super::percpu::block_ptr().is_null() {
        if depth != 0 { crate::println!("[trap] a fault inside the crash dump — halting on the registers above"); }
        cpu::halt_irq_off_forever()
    }
    let el0 = f.flags & 0xF == 0;
    crate::println!("\n╔═══ CRASH DUMP: {} — {} ═══", kind_name(kind), ec_name(f.esr));
    crate::println!("║ pc={:#018x} spsr={:#x} sp={:#018x} the mode: {}", f.pc, f.flags, f.usp, if el0 { "EL0 (user)" } else { "EL1 (kernel)" });
    let t = crate::sched::current_id();
    if t < crate::sched::NTASKS {
        let v = crate::syscall::LAST_SYS[t].load(Ordering::Relaxed);
        if v != u64::MAX { crate::println!("║ the task's last syscall: #{} (the return into {:#x})", v >> 48, v & 0xFFFF_FFFF_FFFF); }
        crate::println!("║ the task t{} (DEAD={}) its tables={:#x} ttbr0={:#x} core {}", t, !crate::sched::task_alive(t),
            crate::sched::task_cr3(t), crate::paging::current_cr3(), cpu);        // Whose stack this core is on: the task's own, or one that is no longer (or not yet) its.
        let (base, top, saved, on, limbo, bsp_cur) = crate::sched::crash_facts(t);
        crate::println!("║ its kernel stack {:#x}..{:#x} (the sp is {}), the saved context {:#x}, assigned to core {}; the boot core: task t{}{}",
            base, top, if (base..top).contains(&f.usp) { "INSIDE it" } else { "OUTSIDE it" }, saved, on, bsp_cur, if limbo { ", in limbo" } else { "" });
        for c in 0..super::smp::MAXCPU {
            let ct = super::percpu::cur_task_of(c);
            if ct != u64::MAX && crate::percpu::ONLINE.load(Ordering::Relaxed) & (1 << c) != 0 { crate::println!("║   core {} runs t{}", c, ct); }
        }
        crate::sched::dead_kstacks(|tid, b, pages| crate::println!("║   an old stack of t{} waits to be freed: {:#x}..{:#x}", tid, b, b + pages * 4096));        let (stale, bad, foreign, longw) = crate::sched::ctx_counters();
        crate::println!("║ the context instruments: entries into a used frame {}, not-a-frame {}, the boot core saved a foreign task {}, stack waits given up {} and waited out {}",
            stale, bad, foreign, longw & 0xFFFF_FFFF, longw >> 32);
        // What the scheduler of every core decided last (the newest at the right).
        let hz = cpu::counter_hz().max(1);
        let now = cpu::counter();
        for c in 0..super::smp::MAXCPU {
            if crate::percpu::ONLINE.load(Ordering::Relaxed) & (1 << c) == 0 { continue; }
            let mut ev = [crate::sched::SEV0; 10];
            let n = crate::sched::last_decisions(c, &mut ev);
            if n == 0 { continue; }
            crate::print!("║   core {} decided:", c);
            for e in &ev[..n] {
                crate::print!(" [-{}us {}t{}->t{} {}]", now.wrapping_sub(e.tsc).saturating_mul(1_000_000) / hz,
                    if e.why & crate::sched::SW_TICK != 0 { "tick " } else { "yield " }, e.from, e.to, e.why & 0x7F);
            }
            crate::println!("");
        }
    }
    let ec = (f.esr >> 26) & 0x3F;
    if matches!(ec, 0x20 | 0x21 | 0x24 | 0x25) {
        // The fault status code: bits 5:2 name the kind, bits 1:0 the table level.
        let fsc = f.esr & 0x3F;
        let what = match fsc >> 2 {
            0 => "an address size fault", 1 => "not mapped (a translation fault)", 2 => "the access flag",
            3 => "protection (a permission fault)", 4 => "a synchronous external abort", 12 => "a TLB conflict",
            _ => "another abort",
        };
        let rw = if ec == 0x20 || ec == 0x21 { "an instruction fetch" } else if f.esr & (1 << 6) != 0 { "a write" } else { "a read" };
        crate::println!("║ the abort: FAR={:#018x} ({}, level {}, {})", f.far, what, fsc & 3, rw);
    }
    // The stack's first words — only where the sp points into memory the kernel maps for
    // itself (a wild sp is the fault's likely CAUSE, and reading it would be the second fault).
    let sp = f.usp;
    if sp & 7 == 0 && sp >= 0x1000 && crate::paging::query(sp).0 && crate::paging::query(sp + 56).0 {
        crate::println!("║ the stack @ {:#x}:", sp);
        for i in 0..8u64 {
            let a = sp + i * 8;
            let v = unsafe { core::ptr::read_volatile(a as *const u64) };
            crate::println!("║   [{:#014x}] = {:#018x}", a, v);
        }
    } else {
        crate::println!("║ the stack: sp={:#x} is not a mapped, aligned address — not read", sp);
    }
    crate::obs::unwind::print_backtrace(if el0 { "workload" } else { "kernel" }, f.pc, f.fp, el0);
    crate::fr::dump();
    // The kernel's core dump — after the text: the text always arrives over the console, a
    // write to the disk at the moment of a fault may not.
    if !el0 { crate::coredump::dump_kernel(f, 11); }
    crate::println!("╚═══ the kernel has been halted ═══");
    // Only the boot core flushes the trace and reboots for the next item of a suite.
    if super::smp::my_logical() == 0 { crate::suite::park(); }
    cpu::halt_irq_off_forever()
}

/// How many raw EL1 exception lines have been written (`sync_el1_entry`).
static RAW_TRAPS: AtomicU64 = AtomicU64::new(0);
/// SVCs taken from EL0, for the bring-up's proof.
pub static SVCS: AtomicU64 = AtomicU64::new(0);
/// What a probe reported with `brk #2` (x8, x9).
pub static PROBE_X8: AtomicU64 = AtomicU64::new(0);
pub static PROBE_X9: AtomicU64 = AtomicU64::new(0);
/// Where a `brk #0` from EL0 returns the machine to (a kernel function, entered at EL1h
/// on the kernel stack the `eret` into EL0 left): the bring-up's probe sets it, so that
/// its deliberate end is not a halt. 0 — a `brk` from EL0 is fatal, as any other fault.
pub static EL0_RETURN: AtomicU64 = AtomicU64::new(0);

/// A synchronous exception from EL0 (AArch64): an `svc` or a fault of the workload. Phase 3
/// puts the Linux syscall dispatcher here (docs/ARM64-PLAN.md §4.5); today an SVC answers
/// with `x0 = x8 + 1` (so that a probe can see the round trip) and anything else — a data
/// or instruction abort, `brk`, an illegal instruction — is fatal with the full diagnosis,
/// which is what the probe's last step provokes on purpose.
#[unsafe(no_mangle)]
extern "C" fn sync_el0(kind: u64, f: &mut Frame) {
    let ec = (f.esr >> 26) & 0x3F;
    if ec == 0x15 {
        SVCS.fetch_add(1, Ordering::Relaxed);
        let r = crate::syscall::rust_syscall(f);
        f.a1 = r;
        return;
    }
    // The debugger's breakpoint, watchpoint or step in the workload (`gdb.rs`), before the
    // fault path turns it into a SIGTRAP.
    if super::gdb::on_exception(f, ec) { return; }
    if user_fault(f) { return; }
    let back = EL0_RETURN.swap(0, Ordering::Relaxed);
    if ec == 0x3C && f.esr & 0xFFFF == 2 && back != 0 {
        // `brk #2`: the probe reports two words (x8, x9) and resumes the kernel.
        PROBE_X8.store(f.nr, Ordering::Relaxed);
        PROBE_X9.store(f.x(9), Ordering::Relaxed);
        f.pc = back;
        f.flags = 0x3C5;
        return;
    }
    if ec == 0x3C && f.esr & 0xFFFF == 0 && back != 0 {
        // `brk #0` with a continuation: resume the kernel there, at EL1h, interrupts masked.
        f.pc = back;
        f.flags = 0x3C5;
        return;
    }
    trap(kind, f)
}

/// The timer's PPI (from the tree; 27 is the virtual timer everywhere seen so far).
pub static TIMER_PPI: AtomicU32 = AtomicU32::new(27);
/// The tick's period while the arch bring-up owns the timer (the scheduler's knobs
/// take over when `sched.rs` comes across).
pub static TICK_NS: AtomicU64 = AtomicU64::new(1_000_000);
/// SGIs received, per core: the IPI path's proof.
pub static SGIS: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
/// Interrupts nobody claimed, per core.
pub static UNCLAIMED: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];

/// Whether the EL0 sync stub saves the workload's FP/SIMD state (528 bytes) on entry.
/// ON by default — the kernel is built hard-float; the `svccost` probe turns it off to
/// measure what the save costs (docs/ARM64-PLAN.md §4.5: the lazy-save decision).
#[unsafe(no_mangle)]
pub static FPSAVE_ON: AtomicU32 = AtomicU32::new(1);

/// A guarded load's fixup (per core): the address to resume at if the load faults, and
/// whether it did — the arm64 twin of `arch::x86_64::unwind::guarded_load`, used by the
/// `bbm` probe to read a page another core is flipping.
pub static FIXUP: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
pub static HIT: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
/// TLB conflict aborts (DFSC 0x30) swallowed by a guarded load — the signature of a
/// missing break-before-make, which is exactly what the `bbm` probe hunts.
pub static CONFLICTS: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];

/// Load the word at `addr`, or `None` if the load faults. The fault handler (`sync_el1`)
/// resumes at the label after the load with the destination left as the zero it was set
/// to, and marks `HIT`.
pub fn guarded_load(addr: u64) -> Option<u64> {
    let cpu = cpu::cpu_index().min(63);
    HIT[cpu].store(0, Ordering::Relaxed);
    let v: u64;
    unsafe {
        core::arch::asm!(
            "adr {t}, 2f",
            "str {t}, [{s}]",
            "mov {v}, #0",
            "ldr {v}, [{a}]",
            "2:",
            "str xzr, [{s}]",
            t = out(reg) _, s = in(reg) FIXUP[cpu].as_ptr(), v = out(reg) v, a = in(reg) addr,
            options(nostack),
        );
    }
    if HIT[cpu].load(Ordering::Relaxed) != 0 { None } else { Some(v) }
}

/// A synchronous exception at EL1 (the kernel itself). A data abort inside a guarded load
/// resumes at its fixup; anything else is fatal.
#[unsafe(no_mangle)]
extern "C" fn sync_el1_entry(kind: u64, f: &mut Frame) -> u64 {
    let me = f as *mut Frame as u64;
    let ec = (f.esr >> 26) & 0x3F;
    // An exception BEFORE the per-core block exists (the bring-up, up to `percpu::init`),
    // in raw hex and before any logic: a fault whose handler faults again leaves this
    // line and nothing else (the console lock, the formatting and the tables are all
    // suspects at that stage — the poisoned TPIDR_EL1 of §10.4 left NOTHING).
    if ec != 0x15 && super::percpu::block_ptr().is_null() && RAW_TRAPS.fetch_add(1, Ordering::Relaxed) < 4 {
        for (tag, v) in [(b'E', f.esr), (b'L', f.pc), (b'F', f.far), (b'S', f.usp), (b'R', f.lr)] {
            super::uart::putb(b' '); super::uart::putb(tag);
            for i in (0..16).rev() { super::uart::putb(b"0123456789abcdef"[((v >> (i * 4)) & 0xF) as usize]); }
        }
        super::uart::putb(b'\n');
    }
    // `svc` at EL1 = the scheduler's soft yield (`switch::soft_yield_kick`): the tick's path
    // by hand, once the scheduler owns the tick.
    if ec == 0x15 {
        if super::idt::SCHED_TIMER.load(Ordering::Relaxed) != 0 { return unsafe { rust_schedule(me) }; }
        return me;
    }
    let cpu = cpu::cpu_index().min(63);
    let fix = FIXUP[cpu].load(Ordering::Relaxed);
    if ec == 0x25 && fix != 0 {
        if f.esr & 0x3F == 0x30 { CONFLICTS[cpu].fetch_add(1, Ordering::Relaxed); }
        HIT[cpu].store(1, Ordering::Relaxed);
        FIXUP[cpu].store(0, Ordering::Relaxed);
        f.pc = fix;
        return me;
    }
    if ec == 0x25 {
        // The profiler's guarded load on a foreign stack (`obs::unwind`): resume after it.
        if let Some(fixup) = crate::obs::unwind::fault_fixup() { f.pc = fixup; return me; }
        // The kernel writing into a user page the workload has not touched yet (a read into a
        // lazily reserved buffer): the first touch, as at EL0. A COW page written by the kernel
        // on the workload's behalf is copied the same way.
        let (_, no_page, write, _) = super::signal::fault_kind(f);
        if no_page && crate::paging::lazy_fault(f.far) { return me; }
        if write && !no_page && crate::paging::cow_fault(f.far) { crate::metrics::inc_cow_fault(); return me; }
    }
    // The debugger: its `brk`s in the kernel, a step of kernel code (`gdb.rs`).
    if super::gdb::on_exception(f, ec) { return me; }
    trap(kind, f)
}

unsafe extern "C" {
    /// The scheduler's switch (`sched.rs`, `#[no_mangle]`): the frame to enter for the frame
    /// interrupted.
    fn rust_schedule(cur_sp: u64) -> u64;
}

/// A fault "handled" by a retry that does not move: the same thread, pc, address and
/// syndrome again and again. A handler that answers "already resolved" while the core keeps
/// faulting (a stale TLB entry, an entry the walker sees and the hardware does not) is an
/// endless loop in which the thread looks alive to every other instrument — it is on the
/// core, at a plausible pc. Said once per 2^16 repeats, with what the tables hold.
fn note_retry(f: &Frame) {
    static LAST: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
    static RUN: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
    let cpu = cpu::cpu_index().min(63);
    let key = f.pc ^ f.far.rotate_left(17) ^ f.esr.rotate_left(41) ^ (crate::sched::current_id() as u64).rotate_left(56);
    if LAST[cpu].swap(key, Ordering::Relaxed) != key { RUN[cpu].store(0, Ordering::Relaxed); return; }
    let n = RUN[cpu].fetch_add(1, Ordering::Relaxed) + 1;
    if n & 0xFFFF == 0 {
        let root = crate::sched::task_cr3(crate::sched::current_id()) & 0x0000_ffff_ffff_f000;
        crate::println!("[fault] a retry that does not move: t{} pc={:#x} far={:#x} esr={:#x} x{} on core {}; the tables: {:#x?}",
            crate::sched::current_id(), f.pc, f.far, f.esr, n, cpu, crate::paging::phys_of_in(root, f.far));
    }
}

/// An EL0 `mrs` of the ID registers, answered as Linux's `emulate_mrs` does — the promise
/// `HWCAP_CPUID` (bit 11 of `AT_HWCAP`) makes. A library that sees the bit reads `MIDR_EL1`
/// and the `ID_AA64*` registers directly (OpenSSL's armcap does, at load: PostgreSQL from
/// Alpine died on its first instruction with a SIGILL), and the read traps to EL1 by the
/// architecture. The space is op0=3, op1=0, CRn=0: `MIDR`/`REVIDR` as they are, `MPIDR` as
/// Linux shows it (no topology), the feature registers with the fields this kernel does not
/// give a process cleared — SVE and SME (their state is not saved), MTE, pointer
/// authentication (no keys), the AMU/MPAM/RAS/GIC — and every other register of the space
/// as zero. `true` — the register is written and the instruction skipped.
fn emulate_id_read(f: &mut Frame) -> bool {
    let iss = f.esr & 0x01FF_FFFF;
    let (op0, op2, op1, crn, rt, crm, read) =
        ((iss >> 20) & 3, (iss >> 17) & 7, (iss >> 14) & 7, (iss >> 10) & 0xF, ((iss >> 5) & 0x1F) as usize, (iss >> 1) & 0xF, iss & 1 == 1);
    if !(op0 == 3 && op1 == 0 && crn == 0 && read) { return false; }
    macro_rules! rd { ($r:literal) => {{ let v: u64; unsafe { core::arch::asm!(concat!("mrs {}, ", $r), out(reg) v, options(nomem, nostack)); } v }} }
    // clr(v, [lo..]): the 4-bit fields at those offsets cleared
    let clr = |v: u64, fields: &[u32]| fields.iter().fold(v, |v, &lo| v & !(0xF << lo));
    let v = match (crm, op2) {
        (0, 0) => rd!("midr_el1"),
        (0, 5) => 1 << 31,                                   // MPIDR: the RES1 bit only
        (0, 6) => rd!("revidr_el1"),
        (4, 0) => clr(rd!("id_aa64pfr0_el1"), &[24, 28, 32, 36, 40, 44, 52]), // GIC RAS SVE SEL2 MPAM AMU RME
        (4, 1) => rd!("id_aa64pfr1_el1") & 0xFF,             // PFR1: BT and SSBS only (no MTE, no SME)
        (6, 0) => rd!("id_aa64isar0_el1"),
        (6, 1) => clr(rd!("id_aa64isar1_el1"), &[4, 8, 24, 28]), // APA API GPA GPI
        (6, 2) => clr(rd!("s3_0_c0_c6_2"), &[8, 12]),         // ISAR2: GPA3 APA3
        (7, 2) => rd!("id_aa64mmfr2_el1") & (0xF << 32),     // MMFR2: AT (the LSE2 alignment) only
        _ => 0,
    };
    if rt != 31 { f.set_x(rt, v); }
    f.pc += 4;
    true
}

/// A workload's fault at EL0, in the order the x86 handler takes (`idt::rust_exception`):
/// a write to a COW page is a copy and a retry; a first touch of a reserved page is a frame
/// and a retry; anything else is a SYNCHRONOUS SIGNAL — delivered to the process's handler
/// when it has one (the JVM catches a SIGSEGV on every implicit null check), else the
/// supervisor's recovery for a single-threaded test, else the process dies as under Linux.
/// `true` — the frame is fixed and the stub returns into it; `false` — not a fault of the
/// workload's (the bring-up's `brk` probes go on to their own handling).
fn user_fault(f: &mut Frame) -> bool {
    use core::sync::atomic::Ordering::SeqCst;
    let ec = (f.esr >> 26) & 0x3F;
    let (abort, no_page, write, _ifetch) = super::signal::fault_kind(f);
    let far = f.far;
    if abort {
        if ec == 0x24 { crate::metrics::inc_page_fault(); }
        if ec == 0x24 && write && !no_page && crate::paging::cow_fault(far) { crate::metrics::inc_cow_fault(); note_retry(f); return true; }
        if no_page && crate::paging::lazy_fault(far) { note_retry(f); return true; }
    }
    if ec == 0x18 && emulate_id_read(f) { return true; }
    let sig = match ec {
        0x20 | 0x21 | 0x24 | 0x25 => 11,    // an abort -> SIGSEGV
        0x00 | 0x0E | 0x18 | 0x19 => 4,     // an unknown/illegal instruction, a trapped system access, SVE -> SIGILL
        0x22 | 0x26 => 7,                   // a PC/SP alignment fault -> SIGBUS
        0x2C => 8,                          // an FP exception -> SIGFPE
        0x30..=0x35 => 5,                   // a breakpoint/step/watchpoint -> SIGTRAP
        0x3C if EL0_RETURN.load(Ordering::Relaxed) == 0 => 5, // `brk` with no bring-up continuation
        _ => return false,
    };
    let fpu = super::signal::fpu_area(f);
    if crate::syscall::deliver_exception_signal(f, sig, far, fpu) { return true; }
    crate::fr::record(crate::fr::FAULT, f.pc);
    let own_as_threads = crate::syscall::THREADED.load(SeqCst) && crate::sched::task_cr3(crate::sched::current_id()) != 0;
    if crate::supervise::RECOVERY_ARMED.load(SeqCst) && !own_as_threads {
        crate::supervise::CRASH_RIP.store(f.pc, SeqCst);
        crate::supervise::CRASH_VEC.store(ec, SeqCst);
        crate::supervise::RECOVERY_ARMED.store(false, SeqCst);
        unsafe { crate::supervise::tal_longjmp(core::ptr::addr_of_mut!(crate::supervise::RECOVERY), 1); }
    }
    if own_as_threads {
        // The thread pointer goes in the same line, as x86's FS base does: every access to
        // `errno` and the TLS goes through it, and a crash on a broken one cannot be told
        // from an honest bug by the pc and the address. There is no self-pointer at
        // [tpidr] on arm64 (the TLS lies ABOVE the thread pointer, the first word is the
        // dtv), so the verdict is only "mapped or not" — read through the tables, never by
        // a dereference here.
        let tp = unsafe { cpu::tls_base() };
        let root = crate::sched::task_cr3(crate::sched::current_id()) & 0x0000_ffff_ffff_f000;
        let tp_lost = tp != 0 && (root == 0 || crate::paging::phys_of_in(root, tp).is_none());
        crate::println!("\n[fault] EL0: esr={:#x} ({}) pc={:#x} far={:#x} sp={:#x} tpidr={:#x}{}",
            f.esr, ec_name(f.esr), f.pc, far, f.usp, tp, if tp_lost { " (tpidr IS NOT MAPPED) <-- THE TLS IS BROKEN" } else { "" });
        // An UNDEFINED instruction: the word the data side holds at the pc, through the
        // process's mapping and through the frame's identity address — an instruction
        // cache that fetched something else is the difference between the two prints
        // and what the process saw (docs/ARM64-PLAN.md §4.12).
        if ec == 0 {
            let user = if root != 0 { crate::paging::phys_of_in(root, f.pc) } else { None };
            let word = user.map(|ph| unsafe { core::ptr::read_volatile(ph as *const u32) });
            match (user, word) {
                (Some(ph), Some(w)) => crate::println!("[fault] EL0: the data side holds {:#010x} at the pc (the frame {:#x})", w, ph),
                _ => crate::println!("[fault] EL0: the pc is not mapped in the process's tables"),
            }
        }
        // The crashed thread's stack goes into the log, always (a core dump needs an FS and
        // the knob; the stack is wanted on a machine that has neither).
        crate::obs::unwind::print_backtrace("workload", f.pc, f.fp, true);
        // First stop the sisters, then write the dump — the order of `idt::rust_exception`
        // on x86: a thread that keeps running while the dump is written leaves garbage in
        // it. Under the supervisor the BSP hands control back once every task has stopped.
        let armed = crate::supervise::RECOVERY_ARMED.load(SeqCst);
        if armed { crate::supervise::crashed(f.pc, ec, sig); }
        crate::coredump::snapshot_threads();
        crate::sched::exit_all(128 + sig as i32);
        crate::sched::drain_proc(1000);
        crate::coredump::dump_process(f, sig as u32);
        if armed { crate::sched::exit_current_code(128 + sig as i32); } // a secondary goes into the pickup loop
        crate::syscall::kill_current_process(sig);
    }
    false
}

/// The IRQ path: acknowledge, dispatch by INTID, end. Nothing here prints — the
/// console has a lock and the interrupted code may hold it.
#[unsafe(no_mangle)]
extern "C" fn irq_entry(_kind: u64, f: &mut Frame) -> u64 {
    let me = f as *mut Frame as u64;
    let cpu = cpu::cpu_index();
    let mut tick = false;
    loop {
        let id = gic::ack();
        if id == gic::SPURIOUS { break; }
        if id == TIMER_PPI.load(Ordering::Relaxed) {
            // Re-arm the deadline (the x86 LAPIC reloads a periodic count itself) — unless the
            // scheduler borrowed the timer for a one-shot, which it re-arms itself.
            let p = super::smp::rearm_period_ns();
            if p != 0 { timer::on_tick(cpu, p); } else {
                // Nothing to re-arm here: a borrowed deadline or the slice's remainder fired, or
                // the core is dedicated (no tick). The generic timer is LEVEL-sensitive — with
                // the compare in the past and the timer enabled the line stays raised, and the
                // vector is re-entered the moment it ends (x86's TSC-deadline fires once). So
                // the timer is silenced here; the borrower gives it back (`lapic_timer_restore`)
                // and the scheduler re-arms the period after a remainder (`timer_tick_reload`).
                // Before this a borrowed 1 ms timeout cost a core over a million interrupts
                // (the `tsync` MPSC phase never finished on arm64).
                timer::stop();
                if cpu < 64 { timer::TICKS[cpu].fetch_add(1, Ordering::Relaxed); }
            }
            super::idt::TICKS.fetch_add(1, Ordering::Relaxed);
            tick = true;
        } else if id == super::idt::RESCHED_SGI {
            // Another core asks this one to enter its scheduler (`smp::send_ipi` on the
            // timer's vector): the tick's path, with the timer left as it is.
            super::idt::IPI_COUNT.fetch_add(1, Ordering::Relaxed);
            tick = true;
        } else if id >= super::its::LPI_BASE {
            // A message-signalled interrupt: the ITS translated a device's event into this LPI.
            super::its::on_lpi(id, cpu);
        } else if id as usize == super::idt::WAKE_VECTOR {
            // The wake-up SGI, and `sync::membarrier`'s acknowledgement: the sender waits
            // until every other core has passed a full barrier, so that the store the
            // interrupted code made before is visible (x86's `wake_stub` gets that from the
            // `iretq`; an exception on arm64 orders nothing by itself — the `dmb ish` here
            // does, as Linux's IPI handlers for membarrier). Before this branch the SGI was
            // only counted, and every membarrier with a second core online timed out at 1 ms.
            core::sync::atomic::fence(Ordering::SeqCst);
            crate::sync::WAKE_ACKS.fetch_add(1, Ordering::SeqCst);
            SGIS[cpu.min(63)].fetch_add(1, Ordering::Relaxed);
            super::idt::IPI_COUNT.fetch_add(1, Ordering::Relaxed);
        } else if id == super::gdb::PARK_SGI {
            // All-stop: another core talks to gdb; this one stands with its frame visible.
            super::gdb::park(f);
        } else if let Some(h) = super::idt::handler_of(id) {
            h();
        } else if id < 16 {
            SGIS[cpu.min(63)].fetch_add(1, Ordering::Relaxed);
            super::idt::IPI_COUNT.fetch_add(1, Ordering::Relaxed);
            if id as usize == super::idt::IPI_VECTOR { crate::pmu::ipi_check(); } // `perf poke <core>`: the counters are read only there
        } else {
            UNCLAIMED[cpu.min(63)].fetch_add(1, Ordering::Relaxed);
        }
        gic::eoi(id);
    }
    // The tick belongs to the scheduler once it is installed: the frame it returns is the
    // one the stub enters (the same, or another task's).
    if tick && super::idt::SCHED_TIMER.load(Ordering::Relaxed) != 0 { return unsafe { rust_schedule(me) }; }
    me
}

