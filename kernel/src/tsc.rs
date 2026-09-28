//! TSC as the primary clock. Frequency via KVM CPUID leaf 0x4000_0010
//! (TSC kHz), fallback: calibration against the i8254 PIT.

use crate::serial::{inb, outb};

#[inline(always)]
pub fn rdtsc() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

fn cpuid(leaf: u32) -> (u32, u32, u32, u32) {
    let r = core::arch::x86_64::__cpuid(leaf);
    (r.eax, r.ebx, r.ecx, r.edx)
}

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

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
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

// ── The wall clock (CLOCK_REALTIME): the reference point + the DISCIPLINE by an external clock ──
// The epoch's base: (1) the cmdline talaria.epoch=SECS (the host passes `date +%s`), otherwise
// (2) kvmclock: MSR_KVM_WALL_CLOCK_NEW/SYSTEM_TIME_NEW (the wall time of the VM's start +
// the system_time since the moment of the start).
//
// Further on the time is counted by the TSC — but NOT "once and for all". One calibration point
// is enough exactly for the length of a demonstration: a quartz drifts by 10-50 ppm, that is by
// 1-4 SECONDS a day. That is not cosmetics of numbers — on the wall clock stand the timestamps
// that the workload gives outwards and by which the trading day is then analysed:
// the time of an order, a file's mtime on ext2 (`ext2::now`), an echo's timestamp for one-way
// delays (`netsys` §4.9). So the reference point is periodically pulled towards
// the external clock by a servo.
//
// The servo corrects the RATE, not the value. Stepping by value is simpler, but the time
// is bound to stay MONOTONIC: a step backwards in the middle of a measurement gives a negative
// delay, a step forwards — an outlier in the tail, and both are indistinguishable from real
// jitter — that is, the instrument starts lying exactly where it exists for.
// A step is left only for the case when there is nothing to absorb (an error greater than
// STEP_NS is no longer a quartz drift but another epoch), and it is spoken about out loud.

use crate::clock::{Servo, mono_step, mono_at, SLEW_PPM_MAX};
pub use crate::clock::cycles_to_ns;

static mut SERVO: Servo = Servo::new(0);
/// Only one may write the state: the adjustment goes both from the timer's tick
/// (an interrupt handler) and from a thread (the PHC reference). Skipping an adjustment is not
/// frightening — tearing the reference point is.
static WRITING: AtomicBool = AtomicBool::new(false);
// A published copy of the reference point for the READERS (a seqlock): `wall_ns()` is called
// from the hot path (an echo's timestamp, ext2), a lock must not be taken there.
static SEQ: AtomicU64 = AtomicU64::new(0);
static PUB_TSC: AtomicU64 = AtomicU64::new(0);
static PUB_NS: AtomicU64 = AtomicU64::new(0);
static PUB_HZ: AtomicU64 = AtomicU64::new(0);

pub const SRC_NONE: u8 = 0;
pub const SRC_KVM: u8 = 1;
pub const SRC_PHC: u8 = 2;
static SRC: AtomicU8 = AtomicU8::new(SRC_NONE);
static SYNC_IV_NS: AtomicU64 = AtomicU64::new(1_000_000_000);
static LAST_SYNC_TSC: AtomicU64 = AtomicU64::new(0);
static FEEDS: AtomicU32 = AtomicU32::new(0);

/// The discipline's interval in milliseconds; 0 — switch it off.
pub fn set_sync_ms(ms: u64) { SYNC_IV_NS.store(ms.saturating_mul(1_000_000), Ordering::Relaxed); }
/// Whether the machine has a cheap reference (kvmclock). On hardware there is none — see the TODO.
pub fn have_ref() -> bool { KVM_OK.load(Ordering::Relaxed) }
pub fn source() -> u8 { SRC.load(Ordering::Relaxed) }
pub fn source_name() -> &'static str {
    match source() { SRC_KVM => "kvmclock", SRC_PHC => "phc(ena)", _ => "there is no reference" }
}
/// A snapshot of the discipline: (the correction in ppm, the error in ns, adjustments, steps, steps backwards, samples).
pub fn wall_stats() -> (i32, i64, u32, u32, u32, u32) {
    let s = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(SERVO)) };
    (s.ppm, s.last_err_ns, s.adjusts, s.steps, s.back, FEEDS.load(Ordering::Relaxed))
}

fn publish(base_tsc: u64, base_ns: u64, hz: u64) {
    SEQ.fetch_add(1, Ordering::AcqRel); // odd — a write is in progress
    PUB_TSC.store(base_tsc, Ordering::Relaxed);
    PUB_NS.store(base_ns, Ordering::Relaxed);
    PUB_HZ.store(hz, Ordering::Relaxed);
    core::sync::atomic::fence(Ordering::Release);
    SEQ.fetch_add(1, Ordering::AcqRel);
    // The same parameters go into the vDSO page, so that user space counts the time itself and
    // in exactly the same way as the kernel (otherwise the clocks "in a process" and "in the kernel" would diverge).
    let mono_at_base = mono_advance(base_tsc, hz);
    crate::vdso::publish(base_tsc, mono_at_base, hz, base_ns.wrapping_sub(mono_at_base));
}

// The reference point of the MONOTONIC clock of ring 3. Separate from the wall one — because
// an adjustment changes the RATE, while it is bound to leave the value in place.
static MONO_TSC: AtomicU64 = AtomicU64::new(0);
static MONO_NS: AtomicU64 = AtomicU64::new(0);
static MONO_HZ: AtomicU64 = AtomicU64::new(0);

/// The monotonic nanoseconds at the point `base_tsc` at the NEW frequency `hz`, computed
/// from the FORMER reference point at the FORMER rate.
///
/// There used to stand `mono_at(base_tsc, hz)` here — a recomputation of the whole TSC since the boot
/// at the new frequency. The wall clock survived that (the servo separately keeps its
/// continuity), while CLOCK_MONOTONIC in ring 3 jumped by `the uptime * Δppm` at
/// EVERY adjustment — and it jumped in both directions. A measurement sees that as
/// a negative delay: on a 15-second run at 20 thousand messages/s
/// (`examples/mdpipe`) the clock went backwards by up to 520 us in 3 runs out of 5, while with
/// `talaria.clocksync=0` — not once. The kernel did not see the defect at all: `mono_ns()`
/// counts by the calibrated frequency and has no reference point.
fn mono_advance(base_tsc: u64, hz: u64) -> u64 {
    let ns = mono_step(MONO_TSC.load(Ordering::Relaxed), MONO_NS.load(Ordering::Relaxed),
                       MONO_HZ.load(Ordering::Relaxed), base_tsc, hz);
    MONO_TSC.store(base_tsc, Ordering::Relaxed);
    MONO_NS.store(ns, Ordering::Relaxed);
    MONO_HZ.store(hz, Ordering::Relaxed);
    ns
}

/// Publish the clock's current parameters into the vDSO (after it has come up: the discipline
/// may not have worked even once yet, while a process needs the time from the very first start).
pub fn publish_vdso_now() {
    let hz = calibrate_cached();
    let t = rdtsc();
    let mono = mono_advance(t, hz);
    let wall = wall_ns();
    crate::vdso::publish(t, mono, hz, wall.wrapping_sub(mono));
}

/// A sample of the external clock. Source-agnostic: kvmclock comes from the tick,
/// the ENA's PHC — from a path where a blocking read of the device is admissible.
pub fn feed_ref(ref_ns: u64, at_tsc: u64, src: u8) {
    if WRITING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_err() { return; }
    let iv = SYNC_IV_NS.load(Ordering::Relaxed);
    unsafe {
        let p = core::ptr::addr_of_mut!(SERVO);
        (*p).feed(ref_ns, at_tsc, iv);
        publish((*p).base_tsc, (*p).base_ns, (*p).hz);
    }
    SRC.store(src, Ordering::Relaxed);
    FEEDS.fetch_add(1, Ordering::Relaxed);
    WRITING.store(false, Ordering::Release);
}

/// The discipline from the timer's tick. THE CHEAP path: the check of the interval is an rdtsc and
/// a comparison, the adjustment itself is a read of the pvclock page and arithmetic. Nothing with
/// an unbounded wait can be here (pitfall A12: the body of an interrupt handler is
/// not entitled to spin), so the PHC reference does not get here — it costs
/// a doorbell into the device and a poll of the answer.
pub fn wall_tick() {
    let iv = SYNC_IV_NS.load(Ordering::Relaxed);
    if iv == 0 || !KVM_OK.load(Ordering::Relaxed) { return; }
    let hz = PUB_HZ.load(Ordering::Relaxed);
    if hz == 0 { return; }
    let t = rdtsc();
    let need = (hz / 1_000_000).saturating_mul(iv / 1000);
    if t.wrapping_sub(LAST_SYNC_TSC.load(Ordering::Relaxed)) < need { return; }
    LAST_SYNC_TSC.store(t, Ordering::Relaxed);
    if let Some((ns, tk)) = kvmclock_read() { feed_ref(ns, tk, SRC_KVM); }
}

#[repr(C)]
struct PvWall { version: u32, sec: u32, nsec: u32 }
#[repr(C)]
struct PvTime { version: u32, pad0: u32, tsc_timestamp: u64, system_time: u64, tsc_to_system_mul: u32, tsc_shift: i8, flags: u8, pad: [u8; 2] }
static mut PV_WALL: PvWall = PvWall { version: 0, sec: 0, nsec: 0 };
#[repr(align(64))]
struct Aligned(PvTime);
static mut PV_TIME: Aligned = Aligned(PvTime { version: 0, pad0: 0, tsc_timestamp: 0, system_time: 0, tsc_to_system_mul: 0, tsc_shift: 0, flags: 0, pad: [0; 2] });

unsafe fn wrmsr(msr: u32, v: u64) {
    core::arch::asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nostack, preserves_flags));
}

static KVM_OK: AtomicBool = AtomicBool::new(false);
static KVM_WALL_NS: AtomicU64 = AtomicU64::new(0); // the wall time of the VM's start (constant)

/// Switch kvmclock on (once): the pvclock pages are registered with the host.
fn kvmclock_enable() -> bool {
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

/// kvmclock "now": (the unix ns, the TSC count it was taken at). Only a READ
/// of the page, without a wrmsr — it is called from the timer's tick. The host corrects the structure on the fly,
/// so we read it BY VERSION (an odd one = a write is in progress), as the pvclock protocol itself
/// prescribes: without that sooner or later one catches half-updated
/// mul/shift and takes the clock to where the servo would have to get out of by a step.
fn kvmclock_read() -> Option<(u64, u64)> {
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

/// Initialise the wall clock (once at the boot, after the TSC's calibration).
pub fn wall_init(cmdline: &str) {
    let hz = calibrate_cached();
    let mut base: Option<u64> = None;
    for tok in cmdline.split_whitespace() {
        if let Some(v) = tok.strip_prefix("talaria.epoch=") {
            if let Ok(s) = v.parse::<u64>() { base = Some(s * 1_000_000_000); }
        } else if let Some(v) = tok.strip_prefix("talaria.clocksync=") {
            // 0 — switch the discipline off (a comparison "as it was" in measurements).
            let ms = v.parse::<u64>().unwrap_or(1000);
            SYNC_IV_NS.store(ms.saturating_mul(1_000_000), Ordering::Relaxed);
        }
    }
    let from_cmdline = base.is_some();
    let kvm = kvmclock_enable();
    let mut t = rdtsc();
    if base.is_none() {
        if let Some((ns, tk)) = kvmclock_read() { base = Some(ns); t = tk; }
    }
    let known = base.is_some();
    let b = base.unwrap_or(1_700_000_000 * 1_000_000_000); // there is no source — a fixed epoch
    unsafe {
        let p = core::ptr::addr_of_mut!(SERVO);
        *p = Servo::new(hz);
        (*p).reset(t, b);
        publish((*p).base_tsc, (*p).base_ns, (*p).hz);
    }
    SRC.store(if kvm { SRC_KVM } else { SRC_NONE }, Ordering::Relaxed);
    LAST_SYNC_TSC.store(t, Ordering::Relaxed);
    crate::println!("talaria: the wall clock's base is {} unix={}s; the discipline is {}",
        if from_cmdline { "the cmdline" } else if known { "kvmclock" } else { "a fallback (there is no source)" },
        b / 1_000_000_000,
        if kvm && SYNC_IV_NS.load(Ordering::Relaxed) != 0 { "kvmclock" }
        else if kvm { "SWITCHED OFF (talaria.clocksync=0)" }
        else { "THERE IS NO REFERENCE — the clock will drift by the quartz's drift (10-50 ppm = 1-4 s/day)" });
}
/// The unix time in nanoseconds (CLOCK_REALTIME). A seqlock reader: the adjustment goes
/// from the tick, that is it may fall exactly in the middle of this read.
pub fn wall_ns() -> u64 {
    loop {
        let s1 = SEQ.load(Ordering::Acquire);
        if s1 & 1 != 0 { core::hint::spin_loop(); continue; }
        let bt = PUB_TSC.load(Ordering::Relaxed);
        let bn = PUB_NS.load(Ordering::Relaxed);
        let hz = PUB_HZ.load(Ordering::Relaxed);
        core::sync::atomic::fence(Ordering::Acquire);
        if SEQ.load(Ordering::Relaxed) != s1 { continue; }
        let hz = if hz == 0 { calibrate_cached() } else { hz };
        let d = rdtsc().wrapping_sub(bt);
        return bn + (d / hz) * 1_000_000_000 + (d % hz) * 1_000_000_000 / hz;
    }
}
/// The monotonic time since the boot in nanoseconds (CLOCK_MONOTONIC).
pub fn mono_ns() -> u64 {
    let hz = calibrate_cached();
    let t = rdtsc();
    (t / hz) * 1_000_000_000 + (t % hz) * 1_000_000_000 / hz
}
/// The servo's self-test (`talaria.test=clock`). The reference is SYNTHETIC: our own TSC
/// rate and a "real" clock diverging by the given ppm. What is checked is exactly what
/// the discipline exists for — the time does not go backwards, the error converges,
/// the correction is bounded — and it is checked in milliseconds rather than in a day of drift.
pub fn servo_selftest() -> bool {
    const HZ: u64 = 2_500_000_000;
    const IV: u64 = 1_000_000_000;
    const EPOCH: u64 = 1_700_000_000 * 1_000_000_000;
    let mut ok = true;

    // Phases 1-2: the quartz has drifted by +40 / -40 ppm. The servo is bound to find that and
    // bring the error together without moving the time backwards even once.
    let drifts = [40i64, -40i64].iter();
    for &ppm in drifts {
        let hz_true = (HZ as i64 + HZ as i64 * ppm / 1_000_000) as u64;
        let mut s = Servo::new(HZ);
        let mut t = 1_000_000u64;
        let mut r = EPOCH;
        s.reset(t, r);
        let mut prev = s.ns_at(t);
        let mut mono = true;
        for _ in 0..40 {
            t += hz_true; // exactly a second of REAL time has passed
            r += IV;
            let before = s.ns_at(t);
            if before < prev { mono = false; }
            s.feed(r, t, IV);
            let after = s.ns_at(t);
            if after < before { mono = false; }
            prev = after;
        }
        let err = if s.last_err_ns < 0 { -s.last_err_ns } else { s.last_err_ns };
        let good = mono && s.steps == 0 && s.back == 0 && err <= 10_000;
        crate::println!("clock-selftest: a drift of {} ppm -> a residual error of {} ns, the correction {} ppm, steps {}, backwards {}, monotonic {}",
            ppm, s.last_err_ns, s.ppm, s.steps, s.back, mono);
        if !good { ok = false; }
    }

    // Phase 3: the reference has gone 2 s forwards — there is nothing to absorb, that is another epoch.
    // We expect EXACTLY one step forwards (and not a single one backwards).
    let mut s = Servo::new(HZ);
    let t0 = 1_000_000u64;
    s.reset(t0, EPOCH);
    let t = t0 + HZ;
    let before = s.ns_at(t);
    s.feed(EPOCH + IV + 2_000_000_000, t, IV);
    let good3 = s.steps == 1 && s.back == 0 && s.ns_at(t) > before;
    crate::println!("clock-selftest: a foreign clock +2 s -> steps {}, backwards {}, the time forwards {}",
        s.steps, s.back, s.ns_at(t) > before);
    if !good3 { ok = false; }

    // Phase 3b: the reference has gone 2 s BACKWARDS. A step backwards is possible only here, and it
    // is bound to be counted — a silent jump backwards is the very defect because of
    // which a measurement gives a negative delay.
    let mut s = Servo::new(HZ);
    s.reset(t0, EPOCH + 4_000_000_000);
    let t = t0 + HZ;
    s.feed(EPOCH, t, IV);
    let good3b = s.steps == 1 && s.back == 1;
    crate::println!("clock-selftest: a foreign clock -2 s -> steps {}, backwards {} (counted)", s.steps, s.back);
    if !good3b { ok = false; }

    // Phase 4: the clamp. The reference has gone 2 ms over the interval — the clock is bound to pick that up
    // in portions of SLEW_PPM_MAX rather than jump. We check it by behaviour: how much
    // accumulates over the next second of real time.
    let mut s = Servo::new(HZ);
    s.reset(t0, EPOCH);
    let t = t0 + HZ;
    s.feed(EPOCH + IV + 2_000_000, t, IV);
    let a = s.ns_at(t);
    let gained = s.ns_at(t + HZ) - a;
    let good4 = s.steps == 0 && (IV..=IV + 700_000).contains(&gained);
    crate::println!("clock-selftest: a jerk of the reference +2 ms -> over a second {} ns was picked up (the ceiling {} ns)",
        gained - IV, IV / 1_000_000 * SLEW_PPM_MAX as u64);
    if !good4 { ok = false; }

    // Phase 5: the reference point of the MONOTONIC clock of ring 3. The servo changes the rate
    // once per interval, and the publication is bound to leave the VALUE in place: a reader
    // in ring 3 calls the clock every few microseconds, and what interests him is not
    // the monotonicity "from publication to publication" but that the time read
    // right AFTER a publication is not less than the one read right BEFORE it.
    //
    // The former construction recomputed the base as "the whole TSC since the boot at the new
    // frequency", that is, it moved the scale retroactively by "the uptime * Δppm". The kernel
    // does not see that (`mono_ns()` counts by the calibrated frequency), while the workload
    // sees it as a NEGATIVE delay.
    let mut pt = 0u64; let mut pn = 0u64; let mut ph = 0u64;   // as it is now
    let mut ot = 0u64; let mut on = 0u64; let mut oh = 0u64;   // as it was
    let mut worst = 0u64; let mut worst_old = 0u64;
    for k in 1..=60u64 {
        let t = k * HZ;                                        // a publication once a second
        let hz = HZ + if k % 2 == 0 { 80_000 } else { 0 };     // the rate wanders by ~32 ppm
        // A read at the point t BEFORE the publication — with the former parameters.
        let before = if pt == 0 { 0 } else { pn + cycles_to_ns(t - pt, ph) };
        let after = mono_step(pt, pn, ph, t, hz);
        if pt != 0 && after < before && before - after > worst { worst = before - after; }
        pt = t; pn = after; ph = hz;

        let before_old = if ot == 0 { 0 } else { on + cycles_to_ns(t - ot, oh) };
        let after_old = mono_at(t, hz);                         // as it was before the fix
        if ot != 0 && after_old < before_old && before_old - after_old > worst_old {
            worst_old = before_old - after_old;
        }
        ot = t; on = after_old; oh = hz;
    }
    let good5 = worst == 0 && worst_old > 100_000;
    crate::println!("clock-selftest: CLOCK_MONOTONIC at a change of the rate -> backwards {} ns (with the former construction {} ns)",
        worst, worst_old);
    if !good5 { ok = false; }

    crate::println!("clock-selftest: {}", if ok { "ALL OK" } else { "FAILURE" });
    ok
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
