//! Reading cell telemetry from shared memory (seqlock read).
use std::os::unix::io::AsRawFd;
include!("../shared/stats.rs");

pub struct ShmReader { p: *const Stats, _f: std::fs::File }

impl ShmReader {
    pub fn open(path: &str) -> std::io::Result<Self> {
        let f = std::fs::File::open(path)?;
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ, libc::MAP_SHARED, f.as_raw_fd(), 0)
        };
        if ptr == libc::MAP_FAILED { return Err(std::io::Error::last_os_error()); }
        Ok(ShmReader { p: ptr as *const Stats, _f: f })
    }

    /// Direct read of the live pkts counter (for the load signal, without the seqlock).
    pub fn pid(&self) -> u32 { unsafe { std::ptr::read_volatile(&(*self.p).pid) } }
    pub fn pkts(&self) -> u64 { unsafe { std::ptr::read_volatile(&(*self.p).pkts) } }

    /// seqlock read: repeat until seq is even and stable.
    pub fn read(&self) -> Option<Stats> {
        for _ in 0..100 {
            let s1 = unsafe { std::ptr::read_volatile(&(*self.p).seq) };
            if s1 & 1 != 0 { continue; }
            std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
            let snap = unsafe { std::ptr::read_volatile(self.p) };
            std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
            let s2 = unsafe { std::ptr::read_volatile(&(*self.p).seq) };
            if s1 == s2 && snap.magic == STATS_MAGIC { return Some(snap); }
        }
        None
    }
}
