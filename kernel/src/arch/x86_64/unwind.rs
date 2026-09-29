//! The one load a profiler may not trust: a frame pointer read from a foreign stack. On
//! x86-64 the page-fault handler consults a per-core FIXUP slot — if the faulting
//! instruction's address is the load below, it resumes at the label after it instead
//! of killing the machine (`idt.rs`, `obs/unwind.rs`). This file is the load itself.

/// Load the word at `addr`, or 0 if the load faults: `fixup_slot` receives the resume
/// address for the duration of the load (the #PF handler reads it), then is cleared.
///
/// # Safety
/// `fixup_slot` must be THIS core's slot (the handler looks up the slot of the core that
/// faulted); nothing else may be in flight on it.
pub unsafe fn guarded_load(addr: u64, fixup_slot: *mut u64) -> u64 {
    let v: u64;
    core::arch::asm!(
        "lea {t}, [rip + 2f]",   // where to return if the load fails
        "mov [{s}], {t}",
        "xor {v:e}, {v:e}",      // on the failed path the value must be zero
        "mov {v}, [{a}]",
        "2:",
        "mov qword ptr [{s}], 0",
        t = out(reg) _,
        s = in(reg) fixup_slot,
        v = out(reg) v,
        a = in(reg) addr,
        options(nostack),
    );
    v
}
