//! The GDB Remote Serial Protocol without the machine: the packet framing, the hex, the stop
//! reply and the packets whose answer does not depend on the registers — the memory (`m`/`M`),
//! the threads (`H`/`T`/`qfThreadInfo`/`qC`), `qSupported`, `qAttached`. The stubs are the
//! machine's (`arch::gdb`: x86 over COM2 with DR0-DR7 and the trap flag, arm64 over the
//! console's UART with the breakpoint/watchpoint registers and the software step); they own
//! the entry, the registers, the breakpoints, the step and the all-stop parking, and hand
//! every other packet to `common`.
//!
//! A thread's number in gdb is the workload's tid (the task + 1), as in `ps` and in the core
//! dump.

/// The line the conversation goes over: the machine's raw byte in and out, no translation.
pub struct Port {
    pub putb: fn(u8),
    pub getc: fn() -> Option<u8>,
}

/// The largest packet either side sends, in bytes (`qSupported`'s PacketSize, hex). arm64's
/// `g` alone is 536 hex digits; 0x400 leaves room for a `G` and an `M` of 400 bytes.
pub const PACKET: usize = 0x400;

impl Port {
    fn getc_block(&self) -> u8 { loop { if let Some(c) = (self.getc)() { return c; } } }
    /// Receive a packet `$...#xx` into `buf`, acknowledge it (`+`, or `-` and wait for the
    /// resend), return its length. Bytes outside a packet are skipped — on a shared line
    /// (arm64's console) that is whatever was printed before the debugger attached.
    pub fn recv(&self, buf: &mut [u8]) -> usize {
        loop {
            loop { if self.getc_block() == b'$' { break; } }
            let mut n = 0; let mut sum: u8 = 0;
            loop {
                let c = self.getc_block();
                if c == b'#' { break; }
                if n < buf.len() { buf[n] = c; n += 1; }
                sum = sum.wrapping_add(c);
            }
            let c1 = self.getc_block(); let c2 = self.getc_block();
            if (unhex(c1) << 4) | unhex(c2) == sum { (self.putb)(b'+'); return n; }
            (self.putb)(b'-');
        }
    }
    /// Send `data` as one packet; resend until gdb acknowledges it.
    pub fn send(&self, data: &[u8]) {
        loop {
            (self.putb)(b'$'); let mut sum: u8 = 0;
            for &b in data { (self.putb)(b); sum = sum.wrapping_add(b); }
            (self.putb)(b'#'); (self.putb)(hexd(sum >> 4)); (self.putb)(hexd(sum & 0xf));
            if self.getc_block() == b'+' { return; }
        }
    }
}

pub fn hexd(n: u8) -> u8 { if n < 10 { b'0' + n } else { b'a' + n - 10 } }
pub fn unhex(c: u8) -> u8 { match c { b'0'..=b'9' => c - b'0', b'a'..=b'f' => c - b'a' + 10, b'A'..=b'F' => c - b'A' + 10, _ => 0 } }
/// A hex number up to the first `,`, `:` or `#`.
pub fn parse_hex(s: &[u8]) -> u64 {
    let mut v = 0u64;
    for &c in s { if c == b',' || c == b':' || c == b'#' { break; } v = (v << 4) | unhex(c) as u64; }
    v
}
/// A little-endian register of `bytes` bytes out of gdb's hex buffer, from the offset `start`.
pub fn read_le_hex(buf: &[u8], start: usize, bytes: usize) -> u64 {
    let mut v = 0u64;
    for i in 0..bytes {
        let j = start + i * 2;
        if j + 1 < buf.len() { v |= ((unhex(buf[j]) << 4 | unhex(buf[j + 1])) as u64) << (i * 8); }
    }
    v
}
/// Append `v`'s low `bytes` bytes, little-endian, as hex.
pub fn push_le(v: u64, bytes: usize, buf: &mut [u8], n: &mut usize) {
    for i in 0..bytes {
        let b = (v >> (i * 8)) as u8;
        buf[*n] = hexd(b >> 4); buf[*n + 1] = hexd(b & 0xf); *n += 2;
    }
}
/// A number as minimal hex into `out`; the number of characters.
pub fn write_tid_hex(out: &mut [u8], id: u64) -> usize {
    if id == 0 { out[0] = b'0'; return 1; }
    let mut tmp = [0u8; 16]; let mut k = 0; let mut v = id;
    while v > 0 { tmp[k] = hexd((v & 0xf) as u8); v >>= 4; k += 1; }
    for i in 0..k { out[i] = tmp[k - 1 - i]; }
    k
}

/// The stop reply: a SIGTRAP, the thread that stopped and `core:` — on which core.
pub fn stop_reply(out: &mut [u8; 40], tid: u64, core: u64) -> usize {
    let mut n = 0;
    for &b in b"T05thread:" { out[n] = b; n += 1; }
    n += write_tid_hex(&mut out[n..], tid); out[n] = b';'; n += 1;
    for &b in b"core:" { out[n] = b; n += 1; }
    n += write_tid_hex(&mut out[n..], core); out[n] = b';'; n += 1;
    n
}

/// One byte of memory, or `None` where nothing is mapped: through the profiler's guarded
/// load (`obs::unwind::try_read`) — gdb asks for whatever a stale frame pointer or a typo
/// names, and before this a read of an unmapped address took the kernel down from inside
/// the stub.
fn peek(addr: u64) -> Option<u8> {
    let w = crate::obs::unwind::try_read(addr & !7, false)?;
    Some((w >> ((addr & 7) * 8)) as u8)
}

/// The packets that do not touch the stopped thread's registers. `me` is the thread that
/// stopped, `sel` the one `Hg` selected. `false` — not one of them.
pub fn common(p: &Port, buf: &[u8], n: usize, me: u64, sel: &mut u64) -> bool {
    match buf[0] {
        b'm' => { // m addr,len
            let addr = parse_hex(&buf[1..n]);
            let mut i = 1; while i < n && buf[i] != b',' { i += 1; }
            let len = parse_hex(&buf[(i + 1).min(n)..n]) as usize;
            let mut out = [0u8; PACKET]; let mut o = 0;
            for k in 0..len.min(PACKET / 2) {
                match peek(addr + k as u64) {
                    Some(b) => { out[o] = hexd(b >> 4); out[o + 1] = hexd(b & 0xf); o += 2; }
                    None => break,
                }
            }
            // Nothing readable at all: an error, as gdb expects ("Cannot access memory").
            if o == 0 && len > 0 { p.send(b"E14"); } else { p.send(&out[..o]); }
        }
        b'M' => { // M addr,len:data
            let addr = parse_hex(&buf[1..n]);
            let mut i = 1; while i < n && buf[i] != b':' { i += 1; }
            let (mut k, mut j) = (0u64, i + 1);
            let mut ok = true;
            while j + 1 < n {
                let b = (unhex(buf[j]) << 4) | unhex(buf[j + 1]);
                let (present, writable, _, _) = crate::paging::query(addr + k);
                if !present || !writable { ok = false; break; }
                unsafe { core::ptr::write_volatile(crate::mem::vptr::<u8>(addr + k), b); }
                k += 1; j += 2;
            }
            p.send(if ok { b"OK" } else { b"E14" });
        }
        b'H' => { // Hg<tid> — whose registers `g` reads; Hc — every thread is continued anyway
            if n >= 2 && buf[1] == b'g' {
                let neg = n >= 3 && buf[2] == b'-';
                let t = if neg { 0 } else { parse_hex(&buf[2..n]) };
                if t == 0 { *sel = me; p.send(b"OK"); }
                else if t == me || crate::sched::saved_ctx(t as usize - 1).is_some() { *sel = t; p.send(b"OK"); }
                else { p.send(b"E01"); }
            } else { p.send(b"OK"); }
        }
        b'T' => { // is the thread alive
            let t = parse_hex(&buf[1..n]);
            if t == me || (t >= 1 && crate::sched::task_alive(t as usize - 1)) { p.send(b"OK"); } else { p.send(b"E01"); }
        }
        b'q' => {
            if n >= 10 && &buf[0..10] == b"qSupported" { p.send(b"PacketSize=400;multiprocess-"); }
            else if n >= 12 && &buf[0..12] == b"qfThreadInfo" {
                // The threads: under the scheduler — the tasks, otherwise one ("1").
                let cnt = if crate::syscall::THREADED.load(core::sync::atomic::Ordering::SeqCst) {
                    crate::sched::task_count().max(1) } else { 1 };
                let mut out = [0u8; 256]; let mut o = 0; out[o] = b'm'; o += 1;
                for t in 0..cnt {
                    if o + 18 > out.len() { break; }
                    if t > 0 { out[o] = b','; o += 1; }
                    o += write_tid_hex(&mut out[o..], (t + 1) as u64);
                }
                p.send(&out[..o]);
            }
            else if n >= 12 && &buf[0..12] == b"qsThreadInfo" { p.send(b"l"); } // the end of the list
            else if n >= 2 && &buf[0..2] == b"qC" {
                let mut out = [0u8; 20]; out[0] = b'Q'; out[1] = b'C';
                let k = write_tid_hex(&mut out[2..], me); p.send(&out[..2 + k]);
            }
            else if n >= 9 && &buf[0..9] == b"qAttached" { p.send(b"1"); }
            else { p.send(b""); }
        }
        _ => return false,
    }
    true
}
