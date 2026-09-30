//! The guarded load a profiler makes on a foreign stack (`obs::unwind`): the same contract
//! as on x86 — the resume address goes into the per-core slot the fault handler reads, the
//! load runs, the slot is cleared; a data abort inside resumes at the label with the
//! destination as the zero it was set to. `sync_el1` consults `obs::unwind::fault_fixup`
//! for that slot, next to the bring-up's own `trap::FIXUP`.
///
/// # Safety
/// `fixup_slot` must be this core's slot of `obs::unwind`, and nothing else may be armed
/// in it at the time.
pub unsafe fn guarded_load(addr: u64, fixup_slot: *mut u64) -> u64 {
    let v: u64;
    core::arch::asm!(
        "adr {t}, 2f",
        "str {t}, [{s}]",
        "mov {v}, #0",
        "ldr {v}, [{a}]",
        "2:",
        "str xzr, [{s}]",
        t = out(reg) _, s = in(reg) fixup_slot, v = out(reg) v, a = in(reg) addr,
        options(nostack),
    );
    v
}
