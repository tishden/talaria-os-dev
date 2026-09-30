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

/// The frame the stubs save: x0..x30, then the saved sp, elr, spsr, esr, far.
#[repr(C)]
pub struct Frame {
    pub x: [u64; 31],
    pub sp: u64,
    pub elr: u64,
    pub spsr: u64,
    pub esr: u64,
    pub far: u64,
}

core::arch::global_asm!(r#"
.macro SAVE_FRAME
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
    add x1, sp, #288
    stp x30, x1, [sp, #240]
    mrs x2, elr_el1
    mrs x3, spsr_el1
    stp x2, x3, [sp, #256]
    mrs x2, esr_el1
    mrs x3, far_el1
    stp x2, x3, [sp, #272]
.endm

.macro RESTORE_FRAME
    ldp x2, x3, [sp, #256]
    msr elr_el1, x2
    msr spsr_el1, x3
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

.macro VECTOR kind
    .balign 0x80
    b vec_\kind
.endm

.section .text
.balign 0x800
.global vectors_el1
vectors_el1:
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
    SAVE_FRAME
    mov x0, #\kind
    mov x1, sp
    bl trap
    b .
.endm

.macro STUB_IRQ kind
vec_\kind:
    SAVE_FRAME
    mov x0, #\kind
    mov x1, sp
    bl irq
    RESTORE_FRAME
    eret
.endm

.macro STUB_SYNC_EL0 kind
vec_\kind:
    SAVE_FRAME
    /* the workload's FP/SIMD state, 528 bytes below the frame — when FPSAVE_ON says so
       (the svccost probe measures the entry with and without; the kernel is hard-float,
       so with it OFF a handler that touches a V register corrupts the workload's) */
    sub sp, sp, #528
    adrp x9, FPSAVE_ON
    ldr w9, [x9, :lo12:FPSAVE_ON]
    cbz w9, 1f
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
1:
    mov x0, #\kind
    add x1, sp, #528
    bl sync_el0
    adrp x9, FPSAVE_ON
    ldr w9, [x9, :lo12:FPSAVE_ON]
    cbz w9, 2f
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
2:
    add sp, sp, #528
    RESTORE_FRAME
    eret
.endm

.macro STUB_SYNC_EL1 kind
vec_\kind:
    SAVE_FRAME
    mov x0, #\kind
    mov x1, sp
    bl sync_el1
    RESTORE_FRAME
    eret
.endm

STUB_FATAL 0
STUB_IRQ 1
STUB_FATAL 2
STUB_FATAL 3
STUB_SYNC_EL1 4
STUB_IRQ 5
STUB_FATAL 6
STUB_FATAL 7

STUB_SYNC_EL0 8
STUB_IRQ 9
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

#[unsafe(no_mangle)]
extern "C" fn trap(kind: u64, f: &Frame) -> ! {
    crate::println!("[trap] {}: esr={:#x} ({}) far={:#x} elr={:#x} spsr={:#x} sp={:#x}",
        kind_name(kind), f.esr, ec_name(f.esr), f.far, f.elr, f.spsr, f.sp);
    for i in (0..30).step_by(2) {
        crate::println!("[trap]   x{:<2}={:#018x} x{:<2}={:#018x}", i, f.x[i], i + 1, f.x[i + 1]);
    }
    crate::println!("[trap]   x30={:#018x}", f.x[30]);
    cpu::halt_irq_off_forever()
}

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
        f.x[0] = f.x[8].wrapping_add(1);
        return;
    }
    let back = EL0_RETURN.swap(0, Ordering::Relaxed);
    if ec == 0x3C && f.esr & 0xFFFF == 2 && back != 0 {
        // `brk #2`: the probe reports two words (x8, x9) and resumes the kernel.
        PROBE_X8.store(f.x[8], Ordering::Relaxed);
        PROBE_X9.store(f.x[9], Ordering::Relaxed);
        f.elr = back;
        f.spsr = 0x3C5;
        return;
    }
    if ec == 0x3C && f.esr & 0xFFFF == 0 && back != 0 {
        // `brk #0` with a continuation: resume the kernel there, at EL1h, interrupts masked.
        f.elr = back;
        f.spsr = 0x3C5;
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
extern "C" fn sync_el1(kind: u64, f: &mut Frame) {
    let ec = (f.esr >> 26) & 0x3F;
    let cpu = cpu::cpu_index().min(63);
    let fix = FIXUP[cpu].load(Ordering::Relaxed);
    if ec == 0x25 && fix != 0 {
        if f.esr & 0x3F == 0x30 { CONFLICTS[cpu].fetch_add(1, Ordering::Relaxed); }
        HIT[cpu].store(1, Ordering::Relaxed);
        FIXUP[cpu].store(0, Ordering::Relaxed);
        f.elr = fix;
        return;
    }
    trap(kind, f)
}

/// The IRQ path: acknowledge, dispatch by INTID, end. Nothing here prints — the
/// console has a lock and the interrupted code may hold it.
#[unsafe(no_mangle)]
extern "C" fn irq(_kind: u64, _f: &Frame) {
    let cpu = cpu::cpu_index();
    loop {
        let id = gic::ack();
        if id == gic::SPURIOUS { break; }
        if id == TIMER_PPI.load(Ordering::Relaxed) {
            timer::on_tick(cpu, TICK_NS.load(Ordering::Relaxed));
        } else if id < 16 {
            SGIS[cpu.min(63)].fetch_add(1, Ordering::Relaxed);
        } else {
            UNCLAIMED[cpu.min(63)].fetch_add(1, Ordering::Relaxed);
        }
        gic::eoi(id);
    }
}
