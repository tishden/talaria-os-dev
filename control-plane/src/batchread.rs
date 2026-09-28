//! Reading the batch cell's telemetry from shared memory.
use std::os::unix::io::AsRawFd;
include!("../shared/batch_stats.rs");

pub struct BatchReader { p: *const BatchStats, _f: std::fs::File }
impl BatchReader {
    pub fn open(path: &str) -> std::io::Result<Self> {
        let f = std::fs::File::open(path)?;
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ, libc::MAP_SHARED, f.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED { return Err(std::io::Error::last_os_error()); }
        let p = ptr as *const BatchStats;
        // The batch cell stamps the region when it starts; anything else is not ours.
        if unsafe { std::ptr::read_volatile(&(*p).magic) } != BATCH_MAGIC {
            unsafe { libc::munmap(ptr, 4096); }
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "not a batch-cell stats region (magic)"));
        }
        Ok(BatchReader { p, _f: f })
    }
    pub fn work_units(&self) -> u64 { unsafe { std::ptr::read_volatile(&(*self.p).work_units) } }
}
