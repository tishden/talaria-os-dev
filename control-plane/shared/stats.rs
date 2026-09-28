// The shared layout of cell telemetry in shared memory. include!() into both crates
// (the cell as writer and the control plane as reader) guarantees one ABI.
pub const STATS_MAGIC: u64 = 0x7461_6c61_7269_6100; // "talaria\0"

#[repr(C)]
pub struct Stats {
    pub magic: u64,
    pub seq: u64,            // seqlock: odd while being written
    pub link: u32,
    pub mac: [u8; 6],
    pub _pad: [u8; 2],
    pub pid: u32,
    pub pkts: u64,           // a live counter (updated on every packet)
    pub rx_tx_avg_ns: u64,
    pub rx_tx_min_ns: u64,
    pub rx_tx_max_ns: u64,
    pub under_100ns_pct: u64,
    pub syscalls_hotpath: u64,
    pub gaps_gt_10us: u64,
    pub llc_mask: u64,       // 0 = no LLC partitioning applied
    pub uptime_s: u64,
    pub dtlb_mpk: u64,  // dTLB-load-misses per 1000 packets
    pub llc_mpk: u64,   // LLC-load-misses per 1000 packets
}
