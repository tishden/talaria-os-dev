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

STUB_FATAL 0
STUB_IRQ 1
STUB_FATAL 2
STUB_FATAL 3
STUB_FATAL 4
STUB_IRQ 5
STUB_FATAL 6
STUB_FATAL 7
STUB_FATAL 8
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

/// The timer's PPI (from the tree; 27 is the virtual timer everywhere seen so far).
pub static TIMER_PPI: AtomicU32 = AtomicU32::new(27);
/// The tick's period while the arch bring-up owns the timer (the scheduler's knobs
/// take over when `sched.rs` comes across).
pub static TICK_NS: AtomicU64 = AtomicU64::new(1_000_000);
/// SGIs received, per core: the IPI path's proof.
pub static SGIS: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
/// Interrupts nobody claimed, per core.
pub static UNCLAIMED: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];

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
