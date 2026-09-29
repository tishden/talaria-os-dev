//! The exception vectors of phase 1: every one of the sixteen entries saves the
//! register file and calls `trap`, which prints the syndrome (`ESR_EL1`), the faulting
//! address (`FAR_EL1`) and the return address (`ELR_EL1`) and halts. The point is the
//! same as the x86 IDT's on day one: instead of a silent reset — a diagnosis on the
//! console. The syscall (`SVC`) and page-fault paths take these over in phase 3
//! (docs/ARM64-PLAN.md §4.5).
//!
//! The table is 2 KiB aligned, sixteen entries 0x80 bytes apart, in four groups
//! (the current EL with SP_EL0, the current EL with SP_EL1, a lower EL in AArch64, a
//! lower EL in AArch32), each of Sync / IRQ / FIQ / SError.

use super::cpu;

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
.macro VECTOR kind
    .balign 0x80
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
    mov x0, #\kind
    mov x1, sp
    bl trap
    b .
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
"#);

/// Point `VBAR_EL1` at the table.
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
