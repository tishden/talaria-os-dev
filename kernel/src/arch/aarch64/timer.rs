//! The generic timer, virtual view (`CNTV_*_EL0`): per core, deadline-only — the
//! tick is a compare value re-armed from the handler, the way the x86 kernel's
//! TSC-deadline mode works (docs/ARM64-PLAN.md §4.9). The counter is the clock
//! (`cpu::counter`), its frequency is a register, and the PPI (27 on every machine
//! seen so far, but the device tree says) is enabled per core through the GIC.

use core::sync::atomic::{AtomicU64, Ordering};

/// Ticks taken on every core, for the bring-up's proof and the self-checks.
pub static TICKS: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];

/// The counter's ticks in `ns` nanoseconds (rounded up, at least 1).
pub fn ticks_for_ns(ns: u64) -> u64 {
    let hz = super::cpu::counter_hz();
    ((ns as u128 * hz as u128).div_ceil(1_000_000_000)).max(1) as u64
}

/// Arm this core's timer to fire `ns` from now and unmask it.
pub fn arm(ns: u64) {
    let cval = super::cpu::counter().wrapping_add(ticks_for_ns(ns));
    unsafe {
        core::arch::asm!(
            "msr cntv_cval_el0, {c}",
            "mov {t}, #1",
            "msr cntv_ctl_el0, {t}",
            "isb",
            c = in(reg) cval, t = out(reg) _, options(nomem, nostack),
        );
    }
}

/// Stop this core's timer.
pub fn stop() {
    unsafe { core::arch::asm!("msr cntv_ctl_el0, xzr", "isb", options(nomem, nostack)); }
}

/// The handler's body: count the tick and re-arm with the same period.
pub fn on_tick(cpu: usize, period_ns: u64) {
    if cpu < 64 { TICKS[cpu].fetch_add(1, Ordering::Relaxed); }
    arm(period_ns);
}
