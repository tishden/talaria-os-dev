//! The control plane executes the cell's syscall-proxy requests: drains the log
//! ring into an audit file and serves synchronous calls (READFILE).
use std::io::Write;
use std::os::unix::io::AsRawFd;
include!("../shared/proxy.rs");
const MAP: usize = 65536;

pub struct ProxyDrainer { p: *mut ProxyShm, last_req: u64 }

impl ProxyDrainer {
    /// Pre-create the file (as the user) so that both sides can write.
    pub fn create(path: &str) -> std::io::Result<Self> {
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        f.set_len(MAP as u64)?;
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), MAP, libc::PROT_READ|libc::PROT_WRITE,
            libc::MAP_SHARED, f.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED { return Err(std::io::Error::last_os_error()); }
        unsafe { std::ptr::write_bytes(ptr as *mut u8, 0, MAP); }
        Ok(ProxyDrainer { p: ptr as *mut ProxyShm, last_req: 0 })
    }

    /// Drain the accumulated logs into the audit file; execute a sync request if any.
    /// Returns the number of drained log lines.
    pub fn poll(&mut self, audit: &mut std::fs::File) -> usize {
        let mut n = 0;
        unsafe {
            let s = &mut *self.p;
            // The cell stamps the region when it attaches; until then there is
            // nothing to drain (the file was created zeroed by `create`).
            if std::ptr::read_volatile(&s.magic) != PROXY_MAGIC { return 0; }
            // async log
            loop {
                let head = std::ptr::read_volatile(&s.log_head);
                let tail = std::ptr::read_volatile(&s.log_tail);
                if tail >= head { break; }
                std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
                let slot = (tail as usize) % LOG_SLOTS;
                let len = s.log[slot][0] as usize;
                let line = &s.log[slot][1..1 + len.min(LOG_LINE - 1)];
                let _ = audit.write_all(line);
                let _ = audit.write_all(b"\n");
                std::ptr::write_volatile(&mut s.log_tail, tail + 1);
                n += 1;
            }
            // sync mailbox
            let req = std::ptr::read_volatile(&s.req_seq);
            if req != self.last_req {
                self.last_req = req;
                if std::ptr::read_volatile(&s.req_op) == OP_READFILE {
                    let l = std::ptr::read_volatile(&s.req_len) as usize;
                    let path = String::from_utf8_lossy(&s.req_data[..l.min(256)]).into_owned();
                    let data = std::fs::read(path.trim()).unwrap_or_default();
                    let m = data.len().min(1024);
                    s.rep_data[..m].copy_from_slice(&data[..m]);
                    s.rep_len = m as u32;
                    std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
                    std::ptr::write_volatile(&mut s.rep_seq, req);
                }
            }
        }
        let _ = audit.flush();
        n
    }
}

/// voluntary_ctxt_switches of a process (how many times it made a blocking syscall).
pub fn vcs(pid: u32) -> u64 {
    if let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        for l in s.lines() {
            if let Some(v) = l.strip_prefix("voluntary_ctxt_switches:") {
                return v.trim().parse().unwrap_or(0);
            }
        }
    }
    0
}
