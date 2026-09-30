//! The counter as the clock, under the TSC's names — `arch::tsc` on arm64. `rdtsc` is
//! `CNTVCT_EL0`, the frequency is `CNTFRQ_EL0` (a register, no calibration: 1.05 GHz on
//! Graviton3, 62.5 MHz under QEMU's TCG), the wall clock and the servo are the shared
//! `wallclock.rs`. There is no cheap external reference here yet (x86 has kvmclock):
//! `ref_enable` says no, `have_ref` follows, the clock drifts by the quartz until the
//! ENA's PHC or an Arm generic-timer paravirt source is wired in (phase 6).

pub use crate::wallclock::{SRC_PHC, set_sync_ms, have_ref, source_name, wall_stats,
    publish_vdso_now, feed_ref, wall_tick, wall_init, wall_ns, mono_ns, servo_selftest, cycles_to_ns};

#[inline(always)]
pub fn rdtsc() -> u64 { super::cpu::counter() }
pub fn calibrate() -> u64 { super::cpu::counter_hz() }
pub fn set_hz(_hz: u64) {}
pub fn hz() -> u64 { super::cpu::counter_hz() }
pub fn calibrate_cached() -> u64 { super::cpu::counter_hz() }
/// No external reference on this machine yet.
pub fn ref_enable() -> bool { false }
pub fn ref_read() -> Option<(u64, u64)> { None }
/// `RNDR` (FEAT_RNG) is the arm64 twin; not read until the feature check is written.
pub fn rdrand64() -> Option<u64> { None }
