//! The counter as the clock, under the TSC's names — `arch::tsc` on arm64. `rdtsc` is
//! `CNTVCT_EL0`, the frequency is `CNTFRQ_EL0` (a register, no calibration: 1.05 GHz on
//! Graviton3, 62.5 MHz under QEMU's TCG), the wall clock and the servo are the shared
//! `wallclock.rs`. There is no cheap external reference here yet (x86 has kvmclock):
//! `ref_enable` says no, `have_ref` follows, the clock drifts by the quartz until the
//! ENA's PHC or an Arm generic-timer paravirt source is wired in (phase 6). The BASE at boot
//! does have a source: the PL031 real-time clock the device tree names (`rtc_read`) — whole
//! seconds, good for "what day is it" (TLS certificates, file times), not for a discipline.

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

/// The PL031's window (0 = the tree named none); `kmain` records it from the device tree.
pub static RTC_BASE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The firmware's clock as the UEFI stub read it (seconds since the epoch; 0 — not given).
pub static BOOT_EPOCH: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The wall clock's base from the machine's real-time clock: (unix ns, the counter at the
/// read). The PL031's data register counts whole seconds since the epoch; without one (a
/// machine described by ACPI) it is the time the UEFI stub asked the firmware for — taken
/// at the counter's zero, the stub ran a moment after it. A value before 2020 is a clock
/// nobody set and is refused.
pub fn rtc_read() -> Option<(u64, u64)> {
    let b = RTC_BASE.load(core::sync::atomic::Ordering::Relaxed);
    if b != 0 {
        // SAFETY: the window is the device tree's, in the device gigabyte every table maps
        // (through the devices' alias once the MMU is on).
        let s = unsafe { core::ptr::read_volatile(super::mmu::mmio_va(b) as *const u32) } as u64;
        if s >= 1_577_836_800 { return Some((s * 1_000_000_000, rdtsc())); }
    }
    let e = BOOT_EPOCH.load(core::sync::atomic::Ordering::Relaxed);
    if e >= 1_577_836_800 { return Some((e * 1_000_000_000, 0)); }
    None
}
/// What `rtc_read` read, for the boot line.
pub fn rtc_name() -> &'static str {
    if RTC_BASE.load(core::sync::atomic::Ordering::Relaxed) != 0 { "the RTC (pl031)" } else { "the firmware's clock (UEFI GetTime)" }
}
