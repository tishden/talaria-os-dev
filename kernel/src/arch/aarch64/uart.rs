//! The PL011 UART, polled: the console of QEMU `virt` (at `0x0900_0000`) and of every
//! arm64 board whose ACPI SPCR names a PL011. Phase 1 hard-codes the `virt` base;
//! the DTB (`/chosen/stdout-path`) and the SPCR take over when they are parsed
//! (docs/ARM64-PLAN.md §4.2). Debug channel only — never on the datapath, as COM1
//! on x86.
//!
//! The registers used: DR (data), FR (flags: TXFF bit 5 = the FIFO is full), IBRD/FBRD
//! (the baud divisor), LCR_H (the line control), CR (enable), ICR (clear interrupts).

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

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
        let mut _l = $crate::arch::uart::Line::new();
        let _ = writeln!(_l, $($arg)*);
    }};
}
