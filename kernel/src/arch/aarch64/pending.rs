//! WHAT THE PORTED MODULES NEED FROM MODULES THAT HAVE NOT COME ACROSS YET.
//!
//! A shared module (`mem.rs`, `paging.rs`, `cmdline.rs`) names its neighbours through
//! `crate::` — `metrics::add_frames`, `acpi::nr_mem_regions`, `tsc::rdtsc`... When the
//! module comes over to arm64 before its neighbour does, the neighbour's name has to
//! resolve to SOMETHING, and this file is that something: the smallest honest stand-in
//! per function, each with the reason it is acceptable for now. `main.rs` maps the
//! names here on arm64 (`use arch::pending::metrics;`) exactly where x86 has
//! `mod metrics;`.
//!
//! This file is a LIST OF DEBTS, and it must shrink: every `pub mod` below goes away
//! when the real module is compiled for this target. Nothing here may grow a behaviour
//! of its own.

/// `metrics.rs` (1 148 lines, everything's counters). Frames are counted; nothing is printed.
pub mod metrics {
    use core::sync::atomic::{AtomicU64, Ordering};
    pub static FRAMES: AtomicU64 = AtomicU64::new(0);
    pub fn add_frames(n: u64) { FRAMES.fetch_add(n, Ordering::Relaxed); }
}

/// `acpi.rs`: no ACPI on the device-tree path — no NUMA regions, and the power-off is PSCI.
pub mod acpi {
    pub fn nr_mem_regions() -> usize { 0 }
    pub fn node_of_phys(_addr: u64) -> u8 { 0 }
    pub fn mem_region(_i: usize) -> (u64, u64, u8) { (0, 0, 0) }
    pub fn poweroff() { super::super::psci::system_off(); }
}

/// `tsc.rs`: the counter plays the TSC. `rdtsc` keeps its x86 name because 29 files call
/// it by that name; the wall-clock servo and the calibration are not needed yet.
pub mod tsc {
    #[inline(always)] pub fn rdtsc() -> u64 { super::super::cpu::counter() }
    pub fn hz() -> u64 { super::super::cpu::counter_hz() }
    pub fn calibrate_cached() -> u64 { super::super::cpu::counter_hz() }
    /// The clock's parameters for the vDSO: the counter as it is, no wall-clock offset
    /// (the servo of `tsc.rs` is what will discipline them).
    pub fn publish_vdso_now() {
        let c = super::super::cpu::counter();
        crate::vdso::publish(c, 0, super::super::cpu::counter_hz(), 0);
        VDSO_BASE.store(c, core::sync::atomic::Ordering::Relaxed);
    }
    /// The counter value the vDSO's zero corresponds to (for the probe's arithmetic).
    pub static VDSO_BASE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
}

/// `trace.rs`: the early trace on disk — there is no disk yet.
pub mod trace {
    pub fn flush() {}
}

/// `sched.rs`: the halt accounting of `paging::halt_enter/exit` — there is no scheduler yet.
pub mod sched {
    pub fn halt_acct_enter(_cpu: usize) {}
    pub fn halt_acct_exit(_cpu: usize) {}
}

/// `affinity.rs`: one NUMA node until the PPTT/SRAT are read.
pub mod affinity {
    pub fn my_node() -> u8 { 0 }
}
