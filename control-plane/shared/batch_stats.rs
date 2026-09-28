// The shared telemetry ABI of the batch cell (include! into batch and into the control plane).
pub const BATCH_MAGIC: u64 = 0x6261_7463_685f_7401; // "batch_t\1"

#[repr(C)]
pub struct BatchStats {
    pub magic: u64,
    pub seq: u64,
    pub work_units: u64,   // units of work done in total
    pub rate_kps: u64,     // thousands of units/s over the last window
    pub running: u32,      // 1 = ran in the last window (was not parked)
    pub _pad: u32,
    pub uptime_s: u64,
}
