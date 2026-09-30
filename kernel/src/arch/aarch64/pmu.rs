//! The performance counters: PMUv3 is phase 8 (docs/ARM64-PLAN.md §5). Until then this is
//! the x86 module's surface with `available() == false`, so `perf`, the tick profile and
//! `kstat` say "no PMU" instead of not compiling.

#[derive(Clone, Copy, Default)]
pub struct Sample {
    pub cycles: u64, pub instrs: u64, pub ref_cycles: u64, pub llc_miss: u64, pub llc_ref: u64,
    pub br_miss: u64, pub br_ret: u64, pub tsc: u64,
}
pub fn init_cpu() -> bool { false }
pub fn init() -> bool { false }
pub fn available() -> bool { false }
pub fn armed_mask() -> u64 { 0 }
pub fn ngp() -> u32 { 0 }
pub fn nfixed() -> u32 { 0 }
pub fn read() -> (u64, u64) { (0, 0) }
pub fn read_cache() -> (u64, u64) { (0, 0) }
pub fn read_branch() -> (u64, u64) { (0, 0) }
pub fn sample() -> Sample { Sample { tsc: super::cpu::counter(), ..Default::default() } }
pub fn tick_enabled() -> bool { false }
pub fn configure_from_cmdline(_cmdline: &str) {}
pub fn note_here() {}
pub fn note_tick() {}
pub fn ipi_check() {}
pub fn refresh_all() {}
pub fn last(_c: usize) -> (Sample, u64) { (Sample::default(), 0) }
pub fn poke(_c: usize) -> bool { false }
