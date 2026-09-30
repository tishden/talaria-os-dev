//! PSCI (the Power State Coordination Interface): how a secondary core is started
//! and how the machine is turned off on arm64 — a call into the firmware or the
//! hypervisor through `hvc` (under a hypervisor: QEMU, KVM) or `smc` (bare metal,
//! where EL3 firmware answers). The conduit comes from the device tree's `psci`
//! node or the FADT's ARM boot flags. This replaces the whole INIT-SIPI-SIPI
//! trampoline of x86 (docs/ARM64-PLAN.md §4.10).

use core::sync::atomic::{AtomicU8, Ordering};

const CPU_ON: u64 = 0xC400_0003;
const SYSTEM_OFF: u64 = 0x8400_0008;
const SYSTEM_RESET: u64 = 0x8400_0009;
const VERSION: u64 = 0x8400_0000;

pub const SUCCESS: i64 = 0;
pub const ALREADY_ON: i64 = -4;
pub const INVALID_PARAMS: i64 = -2;

static CONDUIT: AtomicU8 = AtomicU8::new(0); // 0 = none, 1 = hvc, 2 = smc

pub fn set_conduit(hvc: bool) { CONDUIT.store(if hvc { 1 } else { 2 }, Ordering::Relaxed); }
pub fn present() -> bool { CONDUIT.load(Ordering::Relaxed) != 0 }

/// One SMCCC call. `x0` is the function id; the result is what the callee put in `x0`.
fn call(fid: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let r: u64;
    unsafe {
        match CONDUIT.load(Ordering::Relaxed) {
            1 => core::arch::asm!("hvc #0", inout("x0") fid => r, in("x1") a1, in("x2") a2, in("x3") a3, options(nostack)),
            2 => core::arch::asm!("smc #0", inout("x0") fid => r, in("x1") a1, in("x2") a2, in("x3") a3, options(nostack)),
            _ => return -1, // NOT_SUPPORTED
        }
    }
    r as i64
}

/// The PSCI version the firmware implements: (major, minor); (0, 0) without a conduit.
pub fn version() -> (u32, u32) {
    if !present() { return (0, 0); }
    let v = call(VERSION, 0, 0, 0) as u64;
    (((v >> 16) & 0xFFFF) as u32, (v & 0xFFFF) as u32)
}

/// Start the core with `mpidr` at `entry` (a physical address, entered with the MMU
/// off, `x0` = `context`). `SUCCESS`, `ALREADY_ON`, or a negative PSCI error.
pub fn cpu_on(mpidr: u64, entry: u64, context: u64) -> i64 {
    call(CPU_ON, mpidr, entry, context)
}

/// Power the machine off; returns only if the firmware refused.
pub fn system_off() { call(SYSTEM_OFF, 0, 0, 0); }

/// Reset the machine; returns only if the firmware refused.
pub fn system_reset() { call(SYSTEM_RESET, 0, 0, 0); }
