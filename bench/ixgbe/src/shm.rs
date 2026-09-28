//! Shared memory for the cell's telemetry: a page-sized mmap of a file; the cell
//! writes a Stats structure under a seqlock, the control plane reads it.
use std::os::unix::io::AsRawFd;

include!("../../../control-plane/shared/stats.rs");

pub struct ShmWriter { pub p: *mut Stats }

impl ShmWriter {
    pub fn create(path: &str) -> std::io::Result<Self> {
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        f.set_len(4096)?;
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED, f.as_raw_fd(), 0)
        };
        assert_ne!(ptr, libc::MAP_FAILED, "mmap shm");
        let p = ptr as *mut Stats;
        unsafe {
            std::ptr::write_bytes(p as *mut u8, 0, std::mem::size_of::<Stats>());
            (*p).magic = STATS_MAGIC;
        }
        Ok(ShmWriter { p })
    }

    #[inline]
    pub fn bump_pkts(&self) {
        unsafe { (*self.p).pkts = (*self.p).pkts.wrapping_add(1); }
    }

    /// Update the aggregates under the seqlock (called off the hot path, once per tick).
    #[allow(clippy::too_many_arguments)]
    pub fn publish(&self, link: bool, mac: [u8;6], avg: u64, min: u64, max: u64,
                   under100: u64, syscalls: u64, gaps: u64, llc: u64, uptime: u64,
                   dtlb_mpk: u64, llc_mpk: u64) {
        unsafe {
            let s = &mut *self.p;
            std::ptr::write_volatile(&mut s.seq, s.seq + 1); // odd
            std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
            s.link = link as u32; s.mac = mac;
            s.rx_tx_avg_ns = avg; s.rx_tx_min_ns = min; s.rx_tx_max_ns = max;
            s.under_100ns_pct = under100; s.syscalls_hotpath = syscalls;
            s.gaps_gt_10us = gaps; s.llc_mask = llc; s.uptime_s = uptime;
            s.dtlb_mpk = dtlb_mpk; s.llc_mpk = llc_mpk;
            std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
            std::ptr::write_volatile(&mut s.seq, s.seq + 1); // even
        }
    }
}
