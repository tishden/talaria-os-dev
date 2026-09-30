//! One snapshot of a core's performance counters and the arithmetic over it — the IPC, the
//! miss rates per thousand instructions — shared by `obs::perf`, `top` and `kstat`. The
//! counters themselves are the machine's (`arch::pmu`: the x86 PMU through MSRs and `rdpmc`,
//! PMUv3 on arm64 when phase 8 writes it); the numbers a person reads are not.

/// A full snapshot of the CURRENT core's counters.
#[derive(Clone, Copy, Default)]
pub struct Sample {
    pub cycles: u64,
    pub instrs: u64,
    pub ref_cycles: u64,
    pub llc_miss: u64,
    pub llc_ref: u64,
    pub br_miss: u64,
    pub br_ret: u64,
    pub tsc: u64,
}
impl Sample {
    /// A zero snapshot — the initial value of the accumulators (`Default` in the `const` context
    /// of an array will not do).
    pub const ZERO: Sample = Sample { cycles: 0, instrs: 0, ref_cycles: 0, llc_miss: 0, llc_ref: 0, br_miss: 0, br_ret: 0, tsc: 0 };

    /// The difference of two snapshots of one and the same core.
    pub fn delta(&self, prev: &Sample) -> Sample {
        Sample {
            cycles: self.cycles.wrapping_sub(prev.cycles),
            instrs: self.instrs.wrapping_sub(prev.instrs),
            ref_cycles: self.ref_cycles.wrapping_sub(prev.ref_cycles),
            llc_miss: self.llc_miss.wrapping_sub(prev.llc_miss),
            llc_ref: self.llc_ref.wrapping_sub(prev.llc_ref),
            br_miss: self.br_miss.wrapping_sub(prev.br_miss),
            br_ret: self.br_ret.wrapping_sub(prev.br_ret),
            tsc: self.tsc.wrapping_sub(prev.tsc),
        }
    }
    /// The IPC in hundredths (231 = 2.31). 0 — there is nothing to compute it from.
    pub fn ipc100(&self) -> u64 { if self.cycles > 0 { self.instrs * 100 / self.cycles } else { 0 } }
    /// The share of LLC misses in hundredths of a percent.
    pub fn llc_miss_pct100(&self) -> u64 { if self.llc_ref > 0 { self.llc_miss * 10_000 / self.llc_ref } else { 0 } }
    /// The share of the predictor's misses in hundredths of a percent.
    pub fn br_miss_pct100(&self) -> u64 { if self.br_ret > 0 { self.br_miss * 10_000 / self.br_ret } else { 0 } }
    /// Whether there are denominators for the shares (LLC references / branches in total).
    pub fn has_llc_ratio(&self) -> bool { self.llc_ref > 0 }
    pub fn has_br_ratio(&self) -> bool { self.br_ret > 0 }
    /// LLC misses per THOUSAND INSTRUCTIONS, in hundredths (1234 = 12.34 MPKI).
    /// The denominator is the instructions: any machine counts them, and the quantity is comparable
    /// between runs, whereas "the share of misses" requires a counter of references,
    /// for which there may be no slot.
    pub fn llc_mpki100(&self) -> u64 { if self.instrs > 0 { self.llc_miss * 100_000 / self.instrs } else { 0 } }
    /// The predictor's misses per thousand instructions, in hundredths.
    pub fn br_mpki100(&self) -> u64 { if self.instrs > 0 { self.br_miss * 100_000 / self.instrs } else { 0 } }
}

