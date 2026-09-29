//! Polled 16550 UART on COM1 (0x3F8). Debug/telemetry channel only —
//! never on the datapath.

use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

static LOCK: AtomicBool = AtomicBool::new(false);

const COM1: u16 = 0x3F8;

pub use crate::arch::cpu::{inb, outb};

pub fn init() {
    unsafe {
        outb(COM1 + 1, 0x00); // no interrupts
        outb(COM1 + 3, 0x80); // DLAB
        outb(COM1 + 0, 0x01); // 115200
        outb(COM1 + 1, 0x00);
        outb(COM1 + 3, 0x03); // 8N1
        outb(COM1 + 2, 0xC7); // FIFO
        outb(COM1 + 4, 0x03); // DTR|RTS
    }
}

/// The wait for "the transmit register is empty" is BOUNDED; a port that keeps silent in a row
/// is switched to a short wait, but is not switched off for good (A9).
///
/// Why. Under QEMU and on Nitro guests the UART is always ready, so an unconditional
/// `while inb(LSR) & 0x20 == 0 {}` bothered nobody for three years. On bare metal
/// the port 0x3F8 is held before us by the firmware, and it is enough for it to leave it with
/// automatic flow control (MCR.AFE) without CTS: the FIFO fills up, the THRE bit
/// NEVER rises again — and the kernel hangs on THE VERY FIRST line. From the outside
/// this looks exactly like the c6i.metal picture of session 8: control has been handed to the kernel,
/// there is neither COM1, nor a screen, nor a trace on the disk, and there is no reboot.
///
/// The threshold: 200 000 polls of the port — on real hardware an `in` from a legacy port costs
/// about a microsecond, that is ~0.2 s, while the worst honest wait
/// (a 16-byte FIFO at 115200 baud) is 1.4 ms. After 8 timeouts in a row the wait
/// falls to 2 000 polls: otherwise every line of the log would cost seconds.
const TX_SPINS: u32 = 200_000;
/// The polling threshold after the port has already failed to answer once.
const TX_SPINS_SLOW: u32 = 2_000;
const TX_DEAD_AFTER: u32 = 8;
static TX_TIMEOUTS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static TX_DEAD: AtomicBool = AtomicBool::new(false);

/// Does it look as if COM1 does not answer (diagnostics: a line in the early trace).
pub fn tx_dead() -> bool { TX_DEAD.load(Ordering::Relaxed) }

fn write_byte(b: u8) {
    unsafe {
        // A "dead" port is not switched off FOREVER but only goes over to a short
        // wait: silence at the beginning does not mean silence further on (the firmware
        // could have been keeping the FIFO busy), while the console on bare metal is the only
        // channel that sometimes works. The price of a mistake differs in the two directions:
        // an eternal wait hangs the kernel, while an extra 2000 polls per byte are only
        // milliseconds over the whole log.
        // SERVICE THE TLB MAILBOX — ONCE EVERY 64 BYTES.
        //
        // The step's size is a measurement, not a taste, and both extremes have been checked.
        // Once per LINE is too little: the diagnostics block is printed from the tick in one go,
        // and fifty lines hold IF=0 for those very ten milliseconds —
        // `did not acknowledge` stayed at 3-4. Once per BYTE is expensive: with four
        // cores continuously removing mappings there is almost always a request, and
        // every byte cost a full TLB flush — on a ClickHouse run with
        // snapshots once every two seconds the workload managed 45 requests instead of 176.
        // Sixty-four bytes is a window of about a quarter of a millisecond (forty
        // times less than the wait's ceiling) at a price 64 times lower.
        //
        // BUT IT IS TIME THAT HAS TO BE COUNTED, NOT BYTES. A step in bytes is a window in time
        // only at a known price per byte, and that one is not constant: when FOUR cores
        // print at once (the shootdown gate: all the memrace threads catch a SIGSEGV in
        // one and the same microsecond, and every core prints its own
        // `[fault]` line from the #PF handler with IF=0), every byte is a VMEXIT into the queue
        // for QEMU's common lock, and 64 bytes stretch into milliseconds.
        // The gate gave `did not acknowledge 1-2` once every two or three runs, and the core that kept silent
        // stood in this printing every time. So the window is measured by the TSC:
        // once every 100 us — per core, not per machine (the byte counter was shared,
        // and the servicing of one core was "credited" to the others).
        let n = TX_BYTES.fetch_add(1, Ordering::Relaxed);
        let hz = crate::tsc::hz();
        if hz == 0 { if n & 63 == 0 { crate::paging::tlb_service(); } }
        else {
            let cpu = crate::percpu::cpu_id().min(63);
            let now = crate::tsc::rdtsc();
            if now.wrapping_sub(TX_SERVED_TSC[cpu].load(Ordering::Relaxed)) > hz / 10_000 {
                TX_SERVED_TSC[cpu].store(now, Ordering::Relaxed);
                crate::paging::tlb_service();
            }
        }
        let budget = if TX_DEAD.load(Ordering::Relaxed) { TX_SPINS_SLOW } else { TX_SPINS };
        let mut n = 0u32;
        while inb(COM1 + 5) & 0x20 == 0 {
            n += 1;
            if n >= budget {
                if TX_TIMEOUTS.fetch_add(1, Ordering::Relaxed) + 1 >= TX_DEAD_AFTER {
                    TX_DEAD.store(true, Ordering::Relaxed);
                }
                return; // the port keeps silent — we lose this byte, the kernel lives on
            }
            core::hint::spin_loop();
        }
        TX_TIMEOUTS.store(0, Ordering::Relaxed);
        TX_DEAD.store(false, Ordering::Relaxed);
        outb(COM1, b);
    }
}

/// Output one byte (for the write syscall; without blocking — it is called with IF=0).
/// If the screen is on (PLAN-V1.6 §2: on bare metal COM1 cannot be heard) — the same
/// byte goes there too.
pub fn putb(b: u8) {
    crate::trace::push(b);   // A9: a copy into the early trace's buffer (on a metal — the only channel)
    if b == b'\n' { write_byte(b'\r'); }
    write_byte(b);
    crate::vga::putb(b);
}

/// Output a buffer under the LOCK (write() from different cores does not get mixed up). The spin
/// is bounded: a same-CPU deadlock with an interrupt holding the LOCK is impossible forever.
/// The bytes output into the console (the step of servicing the TLB mailbox).
static TX_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// When THIS core last serviced the TLB mailbox from printing (the TSC).
static TX_SERVED_TSC: [core::sync::atomic::AtomicU64; 64] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 64];

pub fn puts_locked(s: &[u8]) {
    // PRINTING IS BOUND TO SERVICE THE TLB MAILBOX — BUT ONCE PER LINE, NOT ONCE
    // PER BYTE.
    //
    // Why at all. Under a hypervisor every access to a UART port is a
    // VMEXIT, about a microsecond. The diagnostics block `stat:` is printed FROM THE TICK,
    // that is with IF=0, and its fifty lines are ten thousand VMEXITs
    // in a row: all that time the core does not take interrupts, including the TLB
    // flush IPI. A neighbour that has removed a mapping does not get the acknowledgement within
    // the allotted 10 ms and GIVES UP, leaving us a live TLB entry for a frame
    // that the allocator has already given to another. Further on it looks like a "random"
    // corruption of memory: in ClickHouse — `Checksum doesn't match` and different sums
    // for one and the same `SELECT`.
    //
    // Why not on every byte. The first version did exactly that — and MEASUREMENT
    // rejected it: on a run with snapshots once every two seconds the workload managed 45
    // requests instead of 176. The servicing is cheap exactly as long as there is no request;
    // with four cores continuously removing mappings there is almost always a request,
    // and every byte cost a full TLB flush.
    //
    // Once per line is the right step size: the window with IF=0 is bounded by ONE line
    // (a hundred VMEXITs, tenths of a millisecond) instead of a whole block, that is
    // a hundred times less than the wait's ceiling of 10 ms, while the price is one servicing per
    // line instead of eighty.
    //
    // AND THE MAIN THING — IN THE SPIN FOR THE LOCK. Ten milliseconds with IF=0 accumulate not at
    // the one who prints but at the one who WAITS for his turn: while one core
    // outputs fifty lines from the tick, the second stands here on a CAS with the interrupts
    // disabled for that whole block. Servicing once per line at the one who prints
    // does not close this hole — the measurement showed `did not acknowledge 4`
    // just the same. The rule is the same as for all the kernel's other spins: if you spin —
    // service the mailbox.
    crate::paging::tlb_service();
    let mut spins = 0u32;
    let got = loop {
        if LOCK.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() { break true; }
        core::hint::spin_loop();
        spins += 1;
        if spins & 0xFF == 0 { crate::paging::tlb_service(); }
        if spins > 50_000_000 { break false; } // we do not hang — we print without the lock (a mixed-up output is possible)
    };
    for &b in s { putb(b); }
    if got { LOCK.store(false, Ordering::Release); }
}

/// Read a byte from the UART, if there is one (LSR bit0 = data ready). None — there is no data.
/// Read a byte from COM1, if it has arrived.
///
/// A12. An absent port reads as `0xFF` in ALL the registers — including
/// the LSR, where bit 0 means "a byte is ready". The former check `LSR & 1 != 0`
/// took that for an eternal stream of data: `console::poll_tick` spun
/// `while let Some(..) = getc()` without a bound, and on a `c6i.metal` (there is no COM1 there)
/// THE VERY FIRST tick of the scheduler went into an infinite loop INSIDE the interrupt
/// handler. From the outside that looked like "the workload is silent": ring 3 is alive, the syscalls
/// go, the timer ticks — but there is no pre-emption, because the kernel never returned
/// from the first tick. Under QEMU the port is real, the LSR gives out 0, and the class of error is invisible.
///
/// So: `LSR == 0xFF` is not "data", it is an absent port.
pub fn getc() -> Option<u8> {
    unsafe { if port_has_data(COM1) { Some(inb(COM1)) } else { None } }
}

/// Whether there is a real byte in the port's INPUT register.
///
/// The decision is put into a separate function so that it can be checked on a
/// knowingly ABSENT port (`talaria.test=serial`): the only way
/// at home to reproduce a machine without COM1, which most servers are.
/// `LSR == 0xFF` is a floating bus, not data.
pub fn port_has_data(port: u16) -> bool {
    unsafe {
        let lsr = inb(port + 5);
        lsr != 0xFF && lsr & 0x01 != 0
    }
}

// ── THE CAPTURE OF THE OUTPUT INTO A BUFFER (for large diagnostics blocks) ──
//
// The `talaria.stat=` block is 25 KB over a polled UART: every byte waits for
// the THRE and costs a VMEXIT, the whole block is 400-500 ms. It was printed from the tick, with IF=0, and
// the instrument spoiled exactly what it measured: at mongodb insert (gp3, SMP=4, 5000/s)
// the maximum was 417-510 ms against 16.7 ms without snapshots.
//
// It is cured not by "printing less often" but by SEPARATION: assemble the block in memory (that is
// formatting, without a single VMEXIT), and pour it into the UART in portions bounded BY
// TIME. Then the price of a snapshot per tick is comparable to the price of the tick itself, and the whole
// block arrives within a second or two — for a snapshot once every 12 s that is unnoticeable.
const CAP_N: usize = 48 * 1024;
static mut CAP_BUF: [u8; CAP_N] = [0; CAP_N];
static CAP_LEN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static CAP_OUT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// The core whose output currently goes into the buffer (-1 — there is no capture).
static CAP_CPU: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(-1);
/// How many bytes did not fit into the buffer (normally 0).
pub static CAP_LOST: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The price of a pour: bytes and cycles — from it one sees what the instrument itself costs.
pub static CAP_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static CAP_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The worst SINGLE pour: this many cycles and bytes went in one go (the budget is bound to hold it).
pub static CAP_WORST_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static CAP_WORST_N: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Begin capturing THIS core's output into the buffer. It returns false if the previous
/// block has not been poured out yet or a capture is already going on — then the new snapshot is skipped.
pub fn capture_begin() -> bool {
    if CAP_OUT.load(Ordering::Relaxed) < CAP_LEN.load(Ordering::Relaxed) { return false; }
    let me = crate::percpu::cpu_id() as i32;
    if CAP_CPU.compare_exchange(-1, me, Ordering::SeqCst, Ordering::SeqCst).is_err() { return false; }
    CAP_LEN.store(0, Ordering::SeqCst);
    CAP_OUT.store(0, Ordering::SeqCst);
    true
}
pub fn capture_end() { CAP_CPU.store(-1, Ordering::SeqCst); }
/// Whether there is anything to pour.
pub fn capture_pending() -> bool { CAP_OUT.load(Ordering::Relaxed) < CAP_LEN.load(Ordering::Relaxed) }
/// Pour out the rest of the block AS A WHOLE, waiting for the transmitter. Only there where
/// there is nobody to shepherd the portions: a workload without a scheduler (an echo server in the kernel)
/// has no tick, and a deferred block would otherwise never have gone out.
pub fn capture_drain_blocking() {
    let len = CAP_LEN.load(Ordering::Relaxed);
    let mut i = CAP_OUT.load(Ordering::Relaxed);
    while i < len { let b = unsafe { CAP_BUF[i] }; putb_raw(b); i += 1; }
    CAP_OUT.store(i, Ordering::Relaxed);
}
/// A byte onto the line without inserting a CR (in a snapshot's buffer the CR is already there).
fn putb_raw(b: u8) { crate::trace::push(b); write_byte(b); crate::vga::putb(b); }
/// How many bytes of the block have not gone onto the line yet.
pub fn capture_left() -> usize { CAP_LEN.load(Ordering::Relaxed).saturating_sub(CAP_OUT.load(Ordering::Relaxed)) }
/// Pour out of the buffer as much as the transmitter is ready to take RIGHT NOW.
///
/// NOT A SINGLE WAIT. A port is not memory: QEMU (like a real 16550)
/// gives out the THRE at the line's speed, and one byte costs tens of microseconds —
/// a measurement on the stand gave 40-60 us/byte, and the worst SINGLE line of 51 bytes
/// cost 97 ms, because `write_byte` waited for it. A budget in time
/// does not bound such a thing: it is checked between the bytes, while the wait is
/// INSIDE. So here: the transmitter is ready — we give the byte away, it is not — we leave until
/// the next tick. A block of 7 KB goes onto the line within a second or two and costs
/// the workload almost nothing.
pub fn capture_flush(budget_ns: u64, max_bytes: usize) -> usize {
    let len = CAP_LEN.load(Ordering::Relaxed);
    if CAP_OUT.load(Ordering::Relaxed) >= len { return 0; }
    let hz = crate::tsc::hz();
    // the budget's cycles: we count in u128, so that hz * budget does not overflow u64
    let limit = if hz == 0 { u64::MAX } else { ((hz as u128 * budget_ns as u128) / 1_000_000_000u128) as u64 };
    let t0 = crate::tsc::rdtsc();
    // the output lock — so that a portion does not cut a foreign line in two; we do NOT wait for it.
    if LOCK.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() { return 0; }
    // READ THE POSITION UNDER THE LOCK. Session 32: the portions are poured by two cores (the network
    // spinners), the position was taken before the lock — both took one byte, and on the line
    // every tenth character went twice ("reead->writte").
    let mut i = CAP_OUT.load(Ordering::Relaxed);
    if i >= len { LOCK.store(false, Ordering::Release); return 0; }
    let start = i;
    while i < len && i - start < max_bytes {
        if unsafe { inb(COM1 + 5) } & 0x20 == 0 { break; }   // the transmitter is busy — until the next time
        let b = unsafe { CAP_BUF[i] };
        unsafe { outb(COM1, b); }
        crate::trace::push(b);
        crate::vga::putb(b);
        i += 1;
        if hz != 0 && crate::tsc::rdtsc().wrapping_sub(t0) >= limit { break; }
    }
    LOCK.store(false, Ordering::Release);
    CAP_OUT.store(i, Ordering::Relaxed);
    let d = crate::tsc::rdtsc().wrapping_sub(t0);
    CAP_BYTES.fetch_add((i - start) as u64, Ordering::Relaxed);
    CAP_TSC.fetch_add(d, Ordering::Relaxed);
    if d > CAP_WORST_TSC.load(Ordering::Relaxed) {
        CAP_WORST_TSC.store(d, Ordering::Relaxed);
        CAP_WORST_N.store((i - start) as u64, Ordering::Relaxed);
    }
    i - start
}

/// The ONLY way out for the bytes: either into the snapshot's buffer (if there is a capture on
/// this core), or onto the line under the lock.
fn emit(s: &[u8]) {
    if CAP_CPU.load(Ordering::Relaxed) == crate::percpu::cpu_id() as i32 {
        // the capture: into memory, without a single access to the port
        let mut n = CAP_LEN.load(Ordering::Relaxed);
        for &b in s {
            if n + 2 > CAP_N { CAP_LOST.fetch_add(1, Ordering::Relaxed); continue; }
            if b == b'\n' { unsafe { CAP_BUF[n] = b'\r'; } n += 1; }   // we put the CR here, the pour is a pure transfer
            unsafe { CAP_BUF[n] = b; }
            n += 1;
        }
        CAP_LEN.store(n, Ordering::Relaxed);
        return;
    }
    puts_locked(s);
    crate::trace::maybe_flush();   // A9: the trace on the disk — outside the output lock
}

/// A LINE AS A WHOLE, NOT IN PIECES.
///
/// `println!("... {} ...", a, b)` is SEVERAL calls of `write_str`
/// (a literal, an argument, a literal...), and the output lock was taken on every piece.
/// A neighbouring core inserted its own line in the middle of a foreign one: in every run of
/// mongodb four splices of the form `cow=0) frame_allocs=...` were found in the middle of
/// a line. The analysis of a log is an instrument, and a splice makes it a liar (one looked for
/// a "disappeared" line, and it had been cut in two).
///
/// So the line is assembled on the stack and given away in ONE `emit`. The buffer is finite:
/// what did not fit into it goes out with the previous portion — a long line may be
/// cut at the buffer's boundary, but not at a neighbour's whim.
pub struct Line { buf: [u8; 320], n: usize }
impl Line {
    pub fn new() -> Self { Line { buf: [0; 320], n: 0 } }
    fn flush(&mut self) { if self.n > 0 { emit(&self.buf[..self.n]); self.n = 0; } }
}
impl Default for Line { fn default() -> Self { Self::new() } }
impl Drop for Line { fn drop(&mut self) { self.flush(); } }
impl fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if self.n == self.buf.len() { self.flush(); }
            self.buf[self.n] = b; self.n += 1;
        }
        Ok(())
    }
}



/// The same as `println!`, but without a newline: the line is assembled out of pieces
/// (the list of placement rules, a core's line in `cores`).
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut _l = $crate::serial::Line::new();
        let _ = write!(_l, $($arg)*);
    }};
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut _l = $crate::serial::Line::new();
        let _ = writeln!(_l, $($arg)*);
    }};
}
