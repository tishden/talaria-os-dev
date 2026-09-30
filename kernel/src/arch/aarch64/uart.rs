//! The PL011 UART, polled: the console of QEMU `virt` (at `0x0900_0000`) and of every
//! arm64 board whose ACPI SPCR names a PL011. Phase 1 hard-codes the `virt` base;
//! the DTB (`/chosen/stdout-path`) and the SPCR take over when they are parsed
//! (docs/ARM64-PLAN.md §4.2). Debug channel only — never on the datapath, as COM1
//! on x86.
//!
//! The registers used: DR (data), FR (flags: TXFF bit 5 = the FIFO is full), IBRD/FBRD
//! (the baud divisor), LCR_H (the line control), CR (enable), ICR (clear interrupts).

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// One line at a time from ANY core. The wait is bounded (as every wait on this
/// console): a core that faults while holding the lock must still get its diagnosis
/// out, so after the budget the taker proceeds and the lines may interleave.
static LOCK: AtomicBool = AtomicBool::new(false);
const LOCK_SPINS: u32 = 2_000_000;

pub struct Guard;
pub fn lock() -> Guard {
    let mut n = 0u32;
    while LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        n += 1;
        if n >= LOCK_SPINS { break; }
        core::hint::spin_loop();
    }
    Guard
}
impl Drop for Guard { fn drop(&mut self) { LOCK.store(false, Ordering::Release); } }

static BASE: AtomicU64 = AtomicU64::new(0x0900_0000);

const DR: u64 = 0x00;
const FR: u64 = 0x18;
const IBRD: u64 = 0x24;
const FBRD: u64 = 0x28;
const LCR_H: u64 = 0x2C;
const CR: u64 = 0x30;
const ICR: u64 = 0x44;
const FR_TXFF: u32 = 1 << 5;

#[inline] unsafe fn w(off: u64, v: u32) { core::ptr::write_volatile((BASE.load(Ordering::Relaxed) + off) as *mut u32, v) }
#[inline] unsafe fn r(off: u64) -> u32 { core::ptr::read_volatile((BASE.load(Ordering::Relaxed) + off) as *const u32) }

/// Where the UART is (a DTB/SPCR parse calls this before the first line, when it can).
pub fn set_base(b: u64) { BASE.store(b, Ordering::Relaxed); }

/// 8N1, the FIFO on, 115200 against the 24 MHz reference clock QEMU models (QEMU
/// ignores the divisor; a board does not).
pub fn init() {
    unsafe {
        w(CR, 0);
        w(ICR, 0x7FF);
        w(IBRD, 13);
        w(FBRD, 1);
        w(LCR_H, 0x70);   // WLEN = 8 bits | FEN
        w(CR, 0x301);     // UARTEN | TXE | RXE
    }
}

/// The wait for room in the FIFO is BOUNDED, for the same reason as on x86
/// (`serial.rs`): a console that never drains must not hang the kernel on its first line.
const TX_SPINS: u32 = 200_000;

pub fn write_byte(b: u8) {
    unsafe {
        let mut n = 0u32;
        while r(FR) & FR_TXFF != 0 {
            n += 1;
            if n >= TX_SPINS { return; }
            core::hint::spin_loop();
        }
        w(DR, b as u32);
    }
}

/// A formatted line; `\n` becomes `\r\n` for a terminal.
pub struct Line;
impl Line { pub fn new() -> Self { Line } }
impl Default for Line { fn default() -> Self { Self::new() } }
impl fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if b == b'\n' { write_byte(b'\r'); }
            write_byte(b);
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut _l = $crate::arch::uart::Line::new();
        let _ = write!(_l, $($arg)*);
    }};
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _g = $crate::arch::uart::lock();
        let mut _l = $crate::arch::uart::Line::new();
        let _ = writeln!(_l, $($arg)*);
    }};
}

// ── the x86 `serial` surface the shared modules call (`arch::serial` is this module) ──
use core::sync::atomic::AtomicU64 as _CapWord;
pub fn tx_dead() -> bool { false }
pub fn putb(b: u8) { if b == b'\n' { write_byte(b'\r'); } write_byte(b); }
/// A whole buffer under the line lock (two cores' `write()` do not mix).
pub fn puts_locked(s: &[u8]) { let _g = lock(); for &b in s { putb(b); } }
const FR_RXFE: u32 = 1 << 4;
/// A byte from the receive FIFO, if there is one.
pub fn getc() -> Option<u8> {
    unsafe { if r(FR) & FR_RXFE != 0 { None } else { Some((r(DR) & 0xFF) as u8) } }
}
pub fn port_has_data(_port: u16) -> bool { unsafe { r(FR) & FR_RXFE == 0 } }
/// # Safety
/// There is no port I/O on arm64; the x86 self-test that pokes COM ports reads zero here.
pub unsafe fn inb(_port: u16) -> u8 { 0 }
/// # Safety
/// As `inb`.
pub unsafe fn outb(_port: u16, _v: u8) {}
/// The console capture (`talaria.stat=` blocks flushed by a time budget) is not ported yet:
/// a capture never begins, so `obs` prints directly, as it does when the buffer is busy.
pub static CAP_LOST: _CapWord = _CapWord::new(0);
pub static CAP_BYTES: _CapWord = _CapWord::new(0);
pub static CAP_TSC: _CapWord = _CapWord::new(0);
pub static CAP_WORST_TSC: _CapWord = _CapWord::new(0);
pub static CAP_WORST_N: _CapWord = _CapWord::new(0);
pub fn capture_begin() -> bool { false }
pub fn capture_end() {}
pub fn capture_pending() -> bool { false }
pub fn capture_drain_blocking() {}
pub fn capture_left() -> usize { 0 }
pub fn capture_flush(_budget_ns: u64, _max_bytes: usize) -> usize { 0 }
