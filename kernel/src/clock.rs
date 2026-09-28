//! Clock arithmetic: TSC cycles to nanoseconds, the wall-clock servo and the
//! monotonic base that ring 3 reads through the vDSO.
//!
//! Pure integer arithmetic with no hardware access — `tsc.rs` feeds it real
//! TSC readings and external reference time, the unit tests (`kernel-unit`)
//! and the in-kernel self-test (`talaria.test=clock`) feed it synthetic ones.
//! Otherwise the servo could only be checked against days of real drift, that
//! is, not at all.
//!
//! The servo disciplines FREQUENCY, not value. Stepping the value would be
//! simpler, but time must stay monotonic: a step back in the middle of a
//! measurement is a negative latency, a step forward is an outlier in the tail,
//! and both are indistinguishable from real jitter. A step is taken only when
//! there is nothing left to absorb (the error exceeds `STEP_NS` — that is a
//! different epoch, not crystal drift), and it is counted.

/// Largest frequency correction per interval: how many nanoseconds of error the
/// clock may absorb per second. 500 ppm = 500 µs/s: the observed lag of a guest
/// behind its host (~36 µs) is absorbed within one interval, and a garbage
/// reference sample cannot noticeably speed our time up.
pub const SLEW_PPM_MAX: i64 = 500;
/// Beyond this, absorbing makes no sense: it is another epoch, not drift.
pub const STEP_NS: i64 = 100_000_000;
/// A frequency estimate over a shorter interval is measurement noise.
pub const MIN_EST_NS: u64 = 100_000_000;

/// TSC cycles to nanoseconds. The ORDER of operations matters: the naive
/// `c / (hz / 1e9)` rounds the frequency to whole gigahertz (3.602 GHz becomes
/// 3) and overstates every result by twenty percent; multiplying first,
/// `c * 1e9`, overflows u64 after half an hour of uptime. So the integer part
/// and the remainder are converted separately.
#[inline]
pub fn cycles_to_ns(c: u64, hz: u64) -> u64 {
    if hz == 0 { return 0; }
    (c / hz) * 1_000_000_000 + (c % hz) * 1_000_000_000 / hz
}

/// Monotonic nanoseconds at TSC value `t` counted from TSC zero (the formula of
/// `tsc::mono_ns`).
pub fn mono_at(t: u64, hz: u64) -> u64 {
    cycles_to_ns(t, hz)
}

/// Monotonic nanoseconds at `base_tsc` under the NEW frequency `hz`, counted
/// from the PREVIOUS base point at the PREVIOUS rate.
///
/// Recomputing the whole uptime at the new rate (`mono_at(base_tsc, hz)`) moved
/// CLOCK_MONOTONIC in ring 3 by `uptime * Δppm` on every servo adjustment, in
/// both directions: on a 15-second run the clock went back by up to 520 µs in
/// 3 runs of 5. The rate may change; the value at the change point may not.
pub fn mono_step(prev_tsc: u64, prev_ns: u64, prev_hz: u64, base_tsc: u64, hz: u64) -> u64 {
    if prev_tsc == 0 || prev_hz == 0 || base_tsc < prev_tsc {
        mono_at(base_tsc, hz) // first publication: nothing to count from
    } else {
        prev_ns + cycles_to_ns(base_tsc - prev_tsc, prev_hz)
    }
}

/// State of the wall-clock discipline.
#[derive(Clone, Copy)]
pub struct Servo {
    /// Base point: a TSC reading ...
    pub base_tsc: u64,
    /// ... and the unix nanoseconds at it.
    pub base_ns: u64,
    /// Frequency time is currently counted at (including the correction).
    pub hz: u64,
    /// Estimate of the true TSC frequency against the external clock.
    pub hz_true: u64,
    /// Calibrated frequency: the frame the estimate must stay within.
    pub nominal: u64,
    last_tsc: u64,
    last_ns: u64,
    pub have_last: bool,
    /// Times the value had to be stepped.
    pub steps: u32,
    /// Times the frequency was adjusted.
    pub adjusts: u32,
    /// Times time went BACKWARDS (must stay 0).
    pub back: u32,
    /// Last error against the reference, ns.
    pub last_err_ns: i64,
    /// Current correction relative to the calibrated frequency, ppm.
    pub ppm: i32,
}

impl Servo {
    pub const fn new(hz: u64) -> Servo {
        Servo { base_tsc: 0, base_ns: 0, hz, hz_true: hz, nominal: hz,
                last_tsc: 0, last_ns: 0, have_last: false,
                steps: 0, adjusts: 0, back: 0, last_err_ns: 0, ppm: 0 }
    }

    /// Time at TSC value `t` from the current base point.
    pub fn ns_at(&self, t: u64) -> u64 {
        let hz = if self.hz == 0 { 1 } else { self.hz };
        let d = t.wrapping_sub(self.base_tsc);
        self.base_ns + (d / hz) * 1_000_000_000 + (d % hz) * 1_000_000_000 / hz
    }

    /// Initial base point (no discipline yet).
    pub fn reset(&mut self, t: u64, ns: u64) {
        self.base_tsc = t; self.base_ns = ns; self.have_last = false;
    }

    /// A reference sample `ref_ns` taken at TSC value `t`; `iv_ns` is the
    /// expected interval to the next sample (the error is absorbed over it).
    pub fn feed(&mut self, ref_ns: u64, t: u64, iv_ns: u64) {
        let our = self.ns_at(t);
        let err = ref_ns as i64 - our as i64;
        self.last_err_ns = err;
        // 1) Estimate the TRUE frequency from raw TSC against the reference.
        //    Our own correction is deliberately left out: estimating the rate
        //    from our already corrected time closes the loop on itself and
        //    makes an oscillator out of a regulator.
        if self.have_last && ref_ns > self.last_ns {
            let d_tsc = t.wrapping_sub(self.last_tsc);
            let d_ref = ref_ns - self.last_ns;
            if d_ref >= MIN_EST_NS && d_tsc > 0 {
                let obs = (d_tsc as u128 * 1_000_000_000u128 / d_ref as u128) as u64;
                // ±5 %: a crystal does not drift by percents, but a missed
                // interval or a jump of the foreign clock does happen.
                // Believing it would run our time after garbage.
                if obs >= self.nominal / 100 * 95 && obs <= self.nominal / 100 * 105 {
                    self.hz_true = (self.hz_true * 7 + obs) / 8; // smoothing
                }
            }
        }
        self.last_tsc = t; self.last_ns = ref_ns; self.have_last = true;
        if self.hz_true == 0 { self.hz_true = self.nominal; }
        if !(-STEP_NS..=STEP_NS).contains(&err) {
            self.base_tsc = t; self.base_ns = ref_ns; self.hz = self.hz_true;
            self.steps += 1;
            self.ppm = 0;
        } else {
            // 2) The rest is absorbed by FREQUENCY: over the next interval the
            //    clock must gain `err` nanoseconds, not get them at once.
            let iv = if iv_ns < 1_000_000 { 1_000_000i64 } else { iv_ns as i64 };
            let cap = iv * SLEW_PPM_MAX / 1_000_000;
            let e = err.clamp(-cap, cap);
            // hz_used = hz_true * iv / (iv + e): "we are behind" (err > 0)
            // makes the divisor smaller, i.e. our nanoseconds longer.
            let den = if iv + e < 1 { 1 } else { iv + e };
            let hz_used = (self.hz_true as u128 * iv as u128 / den as u128) as u64;
            self.base_tsc = t; self.base_ns = our; // continuity: the value does NOT jump
            self.hz = if hz_used == 0 { self.hz_true } else { hz_used };
            self.adjusts += 1;
            self.ppm = ((self.hz as i64 - self.nominal as i64) * 1_000_000
                / (self.nominal.max(1) as i64)) as i32;
        }
        // The one check that truly matters here: after an adjustment, time
        // may not be earlier than it was before it.
        if self.ns_at(t) < our { self.back += 1; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HZ: u64 = 3_602_393_693; // a real calibration (c6i / KVM)

    #[test]
    fn cycles_to_ns_keeps_fractional_gigahertz() {
        assert_eq!(cycles_to_ns(HZ, HZ), 1_000_000_000);
        assert_eq!(cycles_to_ns(HZ / 2, HZ), 499_999_999);
        // 3.6 GHz, not 3: the naive division would overstate by 20 %
        assert_eq!(cycles_to_ns(3_602_393_693_000, HZ), 1_000_000_000_000);
        assert_eq!(cycles_to_ns(123, 0), 0);
    }

    #[test]
    fn cycles_to_ns_does_not_overflow_after_years() {
        let ten_years = HZ * 3600 * 24 * 365 * 10;
        let ns = cycles_to_ns(ten_years, HZ);
        assert_eq!(ns / 1_000_000_000, 3600 * 24 * 365 * 10);
    }

    #[test]
    fn mono_step_continues_from_previous_base_at_previous_rate() {
        let t1 = HZ * 1000; // 1000 s of uptime
        let n1 = mono_step(0, 0, 0, t1, HZ);
        assert_eq!(n1, 1_000_000_000_000);
        // rate changes by +100 ppm one second later: the value at the change
        // point is counted at the OLD rate from the old base, not recomputed
        let hz2 = HZ + HZ / 10_000;
        let t2 = t1 + HZ;
        let n2 = mono_step(t1, n1, HZ, t2, hz2);
        assert_eq!(n2, n1 + 1_000_000_000);
        // what the old code computed: the whole uptime at the new rate — a jump back
        assert!(mono_at(t2, hz2) < n2);
    }

    #[test]
    fn mono_step_first_publication_and_tsc_going_back() {
        assert_eq!(mono_step(0, 0, 0, HZ, HZ), 1_000_000_000);
        assert_eq!(mono_step(HZ * 2, 5, HZ, HZ, HZ), mono_at(HZ, HZ));
    }

    /// Drive the servo with a reference clock that runs `ppm` fast relative to
    /// the nominal TSC rate, one sample per second, for `secs` seconds.
    fn run(ppm: i64, secs: u64) -> Servo {
        let mut s = Servo::new(HZ);
        s.reset(0, 0);
        let true_hz = (HZ as i128 * 1_000_000 / (1_000_000 + ppm as i128)) as u64;
        for k in 1..=secs {
            let t = true_hz * k;       // the TSC after k true seconds
            s.feed(k * 1_000_000_000, t, 1_000_000_000);
        }
        s
    }

    #[test]
    fn servo_absorbs_crystal_drift_without_steps_or_going_back() {
        for ppm in [-95i64, -20, 0, 20, 95] {
            let s = run(ppm, 120);
            assert_eq!((s.steps, s.back), (0, 0), "ppm {ppm}");
            assert!(s.last_err_ns.abs() < 10_000, "ppm {ppm}: residual {} ns", s.last_err_ns);
            // the reference runs `ppm` fast, so our nanoseconds must get longer:
            // the counting rate goes down by the same amount
            assert!((s.ppm as i64 + ppm).abs() <= 2, "ppm {ppm}: correction {}", s.ppm);
        }
    }

    #[test]
    fn servo_steps_on_a_different_epoch_and_counts_it() {
        let mut s = Servo::new(HZ);
        s.reset(0, 0);
        s.feed(2_000_000_000 + 1_000_000_000, HZ, 1_000_000_000); // reference is 2 s ahead
        assert_eq!(s.steps, 1);
        assert_eq!(s.ns_at(HZ), 3_000_000_000);
        let mut s = Servo::new(HZ);
        s.reset(0, 10_000_000_000);
        s.feed(8_000_000_000 + 1_000_000_000, HZ, 1_000_000_000);  // 2 s behind
        assert_eq!((s.steps, s.back), (1, 1), "a step back is counted, not hidden");
    }

    #[test]
    fn servo_slews_at_most_500_ppm_per_interval() {
        let mut s = Servo::new(HZ);
        s.reset(0, 0);
        // reference jumps 2 ms ahead: below STEP_NS, so it must be absorbed by rate
        s.feed(1_000_000_000 + 2_000_000, HZ, 1_000_000_000);
        assert_eq!(s.steps, 0);
        let gained = s.ns_at(2 * HZ) as i64 - 2_000_000_000;
        assert!(gained > 0 && gained <= 500_000 + 1_000, "gained {gained} ns in 1 s");
    }

    #[test]
    fn servo_ignores_absurd_frequency_estimates() {
        let mut s = Servo::new(HZ);
        s.reset(0, 0);
        s.feed(1_000_000_000, HZ, 1_000_000_000);
        // next sample claims the TSC ran 20 % slow: outside ±5 %, hz_true unchanged
        s.feed(2_000_000_000, HZ + HZ * 8 / 10, 1_000_000_000);
        assert_eq!(s.hz_true, HZ);
    }

    #[test]
    fn value_is_continuous_across_an_adjustment() {
        let mut s = Servo::new(HZ);
        s.reset(0, 0);
        let t = HZ;
        let before = s.ns_at(t);
        s.feed(before + 50_000, t, 1_000_000_000);
        assert_eq!(s.ns_at(t), before, "adjusting the rate must not move the value");
    }
}
