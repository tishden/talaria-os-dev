//! The syscall-proxy client in the cell: cold operations (logging, reading a file) are put
//! into a shared-memory ring; the control plane executes them on the cell's behalf.
//! Not used on the hot path — only the cold path (telemetry once per tick, startup).
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{fence, Ordering};
include!("../../../control-plane/shared/proxy.rs");

const MAP: usize = 65536;

pub struct ProxyWriter { p: *mut ProxyShm }

impl ProxyWriter {
    pub fn create(path: &str) -> std::io::Result<Self> {
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        f.set_len(MAP as u64)?;
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), MAP, libc::PROT_READ|libc::PROT_WRITE,
            libc::MAP_SHARED, f.as_raw_fd(), 0) };
        assert_ne!(ptr, libc::MAP_FAILED, "mmap proxy");
        let p = ptr as *mut ProxyShm;
        unsafe { std::ptr::write_bytes(p as *mut u8, 0, MAP); (*p).magic = PROXY_MAGIC; }
        Ok(ProxyWriter { p })
    }

    /// Asynchronous log: put a line into the ring (fire-and-forget, dropped when full).
    pub fn log(&self, msg: &str) {
        unsafe {
            let s = &mut *self.p;
            let head = std::ptr::read_volatile(&s.log_head);
            let tail = std::ptr::read_volatile(&s.log_tail);
            if head - tail >= LOG_SLOTS as u64 { return; } // full — drop
            let slot = (head as usize) % LOG_SLOTS;
            let bytes = msg.as_bytes();
            let n = bytes.len().min(LOG_LINE - 1);
            s.log[slot][0] = n as u8;
            s.log[slot][1..1 + n].copy_from_slice(&bytes[..n]);
            fence(Ordering::Release);
            std::ptr::write_volatile(&mut s.log_head, head + 1);
        }
    }

    /// Synchronous call: ask the CP to read a file, wait for the answer (spin).
    /// Cold path only (startup, for example).
    pub fn readfile(&self, path: &str) -> Vec<u8> {
        unsafe {
            let s = &mut *self.p;
            let b = path.as_bytes();
            let n = b.len().min(256);
            s.req_data[..n].copy_from_slice(&b[..n]);
            s.req_len = n as u32;
            s.req_op = OP_READFILE;
            let want = std::ptr::read_volatile(&s.req_seq) + 1;
            fence(Ordering::Release);
            std::ptr::write_volatile(&mut s.req_seq, want);
            // wait for the answer
            for _ in 0..100_000_000u64 {
                if std::ptr::read_volatile(&s.rep_seq) == want {
                    fence(Ordering::Acquire);
                    let len = std::ptr::read_volatile(&s.rep_len) as usize;
                    return s.rep_data[..len.min(1024)].to_vec();
                }
                std::hint::spin_loop();
            }
            Vec::new()
        }
    }
}
