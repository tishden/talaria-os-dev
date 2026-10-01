//! TSC as the primary clock. Frequency via KVM CPUID leaf 0x4000_0010
//! (TSC kHz), fallback: calibration against the i8254 PIT.

use crate::serial::{inb, outb};

pub use crate::arch::cpu::rdtsc;
use crate::arch::cpu::cpuid;
/// The wall clock, the monotonic clock and the servo live in `wallclock.rs` (arithmetic over
/// the counter, shared with arm64); the names stay here for the callers.
pub use crate::wallclock::{SRC_PHC, set_sync_ms, have_ref, source_name, wall_stats,
    publish_vdso_now, feed_ref, wall_tick, wall_init, wall_ns, mono_ns, servo_selftest, cycles_to_ns};

/// Returns TSC frequency in Hz.
pub fn calibrate() -> u64 {
    // Hypervisor leaves present? ebx == "KVMK" for the KVM signature.
    let (max_leaf, b, _, _) = cpuid(0x4000_0000);
    if b == u32::from_le_bytes(*b"KVMK") && max_leaf >= 0x4000_0010 {
        let (khz, _, _, _) = cpuid(0x4000_0010);
        if khz > 100_000 {
            return khz as u64 * 1000;
        }
    }
    // Without the hypervisor's leaf (EC2 Nitro/hardware, §4.2): CPUID 0x15 (the ratio
    // TSC/crystal + the crystal's frequency), then 0x16 (the base frequency in MHz).
    let (max_std, _, _, _) = cpuid(0);
    if max_std >= 0x15 {
        let (denom, numer, crystal, _) = cpuid(0x15);
        if denom != 0 && numer != 0 && crystal != 0 {
            return crystal as u64 * numer as u64 / denom as u64;
        }
        if max_std >= 0x16 {
            let (mhz, _, _, _) = cpuid(0x16);
            if mhz > 100 { return mhz as u64 * 1_000_000; }
        }
    }
    pit_calibrate()
}

/// PIT channel 0 calibration over ~50 ms using latched counter reads
/// (mode 2, count 0xFFFF; ports 0x61/0x42 are absent on QEMU microvm).
fn pit_calibrate() -> u64 {
    const PIT_HZ: u64 = 1_193_182;
    const WANT_TICKS: u64 = PIT_HZ / 20; // 50 ms

    unsafe fn read_ch0() -> u16 {
        outb(0x43, 0x00); // latch channel 0
        let lo = inb(0x40) as u16;
        let hi = inb(0x40) as u16;
        (hi << 8) | lo
    }

    unsafe {
        outb(0x43, 0x34); // ch0, lo/hi, mode 2 (rate generator)
        outb(0x40, 0xFF);
        outb(0x40, 0xFF);
        let mut prev = read_ch0();
        let t0 = rdtsc();
        let mut elapsed: u64 = 0;
        // A9: the loop is BOUNDED by the number of polls. If there is no i8254 in the machine (or it
        // does not count — on bare metal a legacy timer is entitled to be absent), the counter
        // always returns one and the same thing, elapsed does not grow, and an unconditional `while`
        // hangs the kernel FOREVER at the very start of the bring-up — with exactly the picture
        // that the c6i.metal gave: control has been received, there is no output, there is no reboot.
        let mut polls: u64 = 0;
        while elapsed < WANT_TICKS {
            let cur = read_ch0();
            // mode 2 counts down and wraps 0 -> 0xFFFF
            let d = prev.wrapping_sub(cur) as u64 & 0xFFFF;
            elapsed += d;
            prev = cur;
            polls += 1;
            if polls > 50_000_000 { break; }   // ~seconds of polling the port: the PIT is dead
        }
        let t1 = rdtsc();
        if elapsed < PIT_HZ / 1000 {           // less than a millisecond has "ticked up" — we do not believe it
            crate::println!("talaria: the i8254 does not tick — the TSC's frequency defaults to 2.5 GHz");
            return 2_500_000_000;
        }
        (t1 - t0) * PIT_HZ / elapsed
    }
}

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// The calibration with caching (the first call measures, then it comes from the cache).
pub fn set_hz(hz: u64) { TSC_HZ.store(hz, Ordering::Relaxed); }
/// The frequency WITHOUT a calibration (0 — it has not been measured yet). For the paths where one must not wait.
pub fn hz() -> u64 { TSC_HZ.load(Ordering::Relaxed) }
pub fn calibrate_cached() -> u64 {
    let v = TSC_HZ.load(Ordering::Relaxed);
    if v != 0 { return v; }
    let hz = calibrate();
    TSC_HZ.store(hz, Ordering::Relaxed);
    hz
}

#[repr(C)]
struct PvWall { version: u32, sec: u32, nsec: u32 }
#[repr(C)]
struct PvTime { version: u32, pad0: u32, tsc_timestamp: u64, system_time: u64, tsc_to_system_mul: u32, tsc_shift: i8, flags: u8, pad: [u8; 2] }
static mut PV_WALL: PvWall = PvWall { version: 0, sec: 0, nsec: 0 };
#[repr(align(64))]
struct Aligned(PvTime);
static mut PV_TIME: Aligned = Aligned(PvTime { version: 0, pad0: 0, tsc_timestamp: 0, system_time: 0, tsc_to_system_mul: 0, tsc_shift: 0, flags: 0, pad: [0;
    2] });

unsafe fn wrmsr(msr: u32, v: u64) {
    core::arch::asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nostack, preserves_flags));
}

static KVM_OK: AtomicBool = AtomicBool::new(false);
static KVM_WALL_NS: AtomicU64 = AtomicU64::new(0); // the wall time of the VM's start (constant)

/// The machine's cheap external clock: kvmclock. Switch it on (once): the pvclock pages are
/// registered with the host. `wallclock::wall_init` calls it; `have_ref` is its answer.
pub fn ref_enable() -> bool {
    let (_, b, _, _) = cpuid(0x4000_0000);
    if b != u32::from_le_bytes(*b"KVMK") { return false; }
    let (feat, _, _, _) = cpuid(0x4000_0001);
    if feat & (1 << 3) == 0 { return false; } // KVM_FEATURE_CLOCKSOURCE2
    unsafe {
        let wp = core::ptr::addr_of_mut!(PV_WALL) as u64;
        let tp = core::ptr::addr_of_mut!(PV_TIME) as u64;
        wrmsr(0x4b56_4d01, tp | 1); // MSR_KVM_SYSTEM_TIME_NEW (enable)
        wrmsr(0x4b56_4d00, wp);     // MSR_KVM_WALL_CLOCK_NEW
        let w = core::ptr::read_volatile(core::ptr::addr_of!(PV_WALL));
        if w.sec == 0 { return false; }
        KVM_WALL_NS.store(w.sec as u64 * 1_000_000_000 + w.nsec as u64, Ordering::Relaxed);
    }
    KVM_OK.store(true, Ordering::Relaxed);
    true
}

/// The reference's "now": (the unix ns, the TSC count it was taken at). Only a READ
/// of the page, without a wrmsr — it is called from the timer's tick. The host corrects the structure on the fly,
/// so we read it BY VERSION (an odd one = a write is in progress), as the pvclock protocol itself
/// prescribes: without that sooner or later one catches half-updated
/// mul/shift and takes the clock to where the servo would have to get out of by a step.
pub fn ref_read() -> Option<(u64, u64)> {
    if !KVM_OK.load(Ordering::Relaxed) { return None; }
    unsafe {
        for _ in 0..8 {
            let v1 = core::ptr::read_volatile(core::ptr::addr_of!(PV_TIME.0.version));
            if v1 & 1 != 0 { continue; }
            let t = core::ptr::read_volatile(core::ptr::addr_of!(PV_TIME.0));
            let now = rdtsc();
            let v2 = core::ptr::read_volatile(core::ptr::addr_of!(PV_TIME.0.version));
            if v1 != v2 { continue; }
            // the system_time now = the system_time + scale(rdtsc - tsc_timestamp)
            let mut delta = now.wrapping_sub(t.tsc_timestamp);
            if t.tsc_shift >= 0 { delta <<= t.tsc_shift as u32; } else { delta >>= (-(t.tsc_shift as i32)) as u32; }
            let scaled = ((delta as u128 * t.tsc_to_system_mul as u128) >> 32) as u64;
            let sys_ns = t.system_time.wrapping_add(scaled);
            return Some((KVM_WALL_NS.load(Ordering::Relaxed) + sys_ns, now));
        }
        None
    }
}


/// RDRAND (CPUID.1:ECX[30]); None if it is unavailable/failed.
pub fn rdrand64() -> Option<u64> {
    let (_, _, c, _) = cpuid(1);
    if c & (1 << 30) == 0 { return None; }
    for _ in 0..16 {
        let v: u64; let ok: u8;
        unsafe { core::arch::asm!("rdrand {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok, options(nomem, nostack)); }
        if ok != 0 { return Some(v); }
    }
    None
}

/// The wall clock's base from a real-time clock: x86 takes it from kvmclock (`ref_read`);
/// the CMOS clock is not read.
pub fn rtc_read() -> Option<(u64, u64)> { None }
pub const RTC_NAME: &str = "the RTC";
