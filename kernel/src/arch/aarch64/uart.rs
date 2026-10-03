//! The console UART, polled — one of two: the PL011 of QEMU `virt` (at `0x0900_0000`) and
//! of every arm64 board whose ACPI SPCR names one, or the 16550 a Nitro guest's SPCR names
//! (memory-mapped, its byte registers a word apart — `mmio32`). Phase 1 hard-codes the
//! `virt` base; the DTB (`/chosen/stdout-path`) and the SPCR take over when they are parsed
//! (docs/ARM64-PLAN.md §4.2). Debug channel only — never on the datapath, as COM1 on x86.
//!
//! The PL011 registers used: DR (data), FR (flags: TXFF bit 5 = the FIFO is full), IBRD/FBRD
//! (the baud divisor), LCR_H (the line control), CR (enable), ICR (clear interrupts). The
//! 16550's: THR/RBR (0), IER (1), LSR (5: DR bit 0, THRE bit 5) — the line itself (the
//! divisor, the format) is left as the firmware set it: the reference clock behind a
//! platform's 16550 is nobody's to guess, and the SPCR describes a console that works.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

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
/// 0 — a PL011; 1 — a 16550 whose registers are `1 << SHIFT` bytes apart and `WIDTH` bytes wide.
static KIND: AtomicU8 = AtomicU8::new(0);
static SHIFT: AtomicU8 = AtomicU8::new(0);
static WIDTH: AtomicU8 = AtomicU8::new(1);

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

// The 16550's registers, by index; the stride and the width come from the SPCR.
const NS_THR: u64 = 0; const NS_IER: u64 = 1; const NS_LSR: u64 = 5;
const NS_LSR_DR: u32 = 1; const NS_LSR_THRE: u32 = 1 << 5;
#[inline] unsafe fn nsw(idx: u64, v: u32) {
    let a = BASE.load(Ordering::Relaxed) + (idx << SHIFT.load(Ordering::Relaxed));
    match WIDTH.load(Ordering::Relaxed) { 4 => core::ptr::write_volatile(a as *mut u32, v), 2 => core::ptr::write_volatile(a as *mut u16, v as u16), _ => core::ptr::write_volatile(a as *mut u8, v as u8) }
}
#[inline] unsafe fn nsr(idx: u64) -> u32 {
    let a = BASE.load(Ordering::Relaxed) + (idx << SHIFT.load(Ordering::Relaxed));
    match WIDTH.load(Ordering::Relaxed) { 4 => core::ptr::read_volatile(a as *const u32), 2 => core::ptr::read_volatile(a as *const u16) as u32, _ => core::ptr::read_volatile(a as *const u8) as u32 }
}
#[inline] fn is_16550() -> bool { KIND.load(Ordering::Relaxed) == 1 }

// The four questions every path below asks, answered for whichever UART this is.
#[inline] fn tx_full() -> bool { unsafe { if is_16550() { nsr(NS_LSR) & NS_LSR_THRE == 0 } else { r(FR) & FR_TXFF != 0 } } }
#[inline] fn tx_put(b: u8) { unsafe { if is_16550() { nsw(NS_THR, b as u32) } else { w(DR, b as u32) } } }
#[inline] fn rx_empty() -> bool { unsafe { if is_16550() { nsr(NS_LSR) & NS_LSR_DR == 0 } else { r(FR) & FR_RXFE != 0 } } }
#[inline] fn rx_get() -> u8 { unsafe { (if is_16550() { nsr(NS_THR) } else { r(DR) } & 0xFF) as u8 } }

/// Where the UART is (a DTB/SPCR parse calls this before the first line, when it can).
pub fn set_base(b: u64) { BASE.store(b, Ordering::Relaxed); KIND.store(0, Ordering::Relaxed); }
/// A 16550 at `b`: registers `1 << shift` bytes apart, `width` (1, 2 or 4) bytes each.
pub fn set_16550(b: u64, shift: u8, width: u8) {
    BASE.store(b, Ordering::Relaxed);
    SHIFT.store(shift, Ordering::Relaxed);
    WIDTH.store(width, Ordering::Relaxed);
    KIND.store(1, Ordering::Relaxed);
}

/// 8N1, the FIFO on, 115200 against the 24 MHz reference clock QEMU models (QEMU
/// ignores the divisor; a board does not).
pub fn init() {
    if BASE.load(Ordering::Relaxed) == 0 { return; }
    // A 16550: polled, so its interrupts off; the line stays as the firmware set it.
    if is_16550() { unsafe { nsw(NS_IER, 0); } return; }
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
    // No console (a machine described by ACPI whose SPCR names none we can drive).
    if BASE.load(Ordering::Relaxed) == 0 { return; }
    let mut n = 0u32;
    while tx_full() {
        n += 1;
        if n >= TX_SPINS { return; }
        core::hint::spin_loop();
    }
    tx_put(b);
}

/// A formatted line; `\n` becomes `\r\n` for a terminal.
pub struct Line;
impl Line { pub fn new() -> Self { Line } }
impl Default for Line { fn default() -> Self { Self::new() } }
impl fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if capture_here() { capture_put(s.as_bytes()); return Ok(()); }
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
    if BASE.load(Ordering::Relaxed) == 0 || rx_empty() { return None; }
    Some(rx_get())
}
pub fn port_has_data(_port: u16) -> bool { BASE.load(Ordering::Relaxed) != 0 && !rx_empty() }
/// # Safety
/// There is no port I/O on arm64; the x86 self-test that pokes COM ports reads zero here.
pub unsafe fn inb(_port: u16) -> u8 { 0 }
/// # Safety
/// As `inb`.
pub unsafe fn outb(_port: u16, _v: u8) {}
// ── THE CAPTURE OF THE OUTPUT INTO A BUFFER (for large diagnostics blocks) ──
//
// The same contract as x86's `serial::capture_*` (the reasons are written there): a block of
// `talaria.stat=` or a `netprof` summary is ASSEMBLED in memory by the core that takes it and
// goes into the UART in portions bounded by time. While this was a stub that answered "no",
// every such block was SKIPPED on arm64 — `talaria.stat=` printed nothing at all, and the
// gates that judge by its counters had nothing to read.
const CAP_N: usize = 48 * 1024;
static mut CAP_BUF: [u8; CAP_N] = [0; CAP_N];
static CAP_LEN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static CAP_OUT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// The core whose output currently goes into the buffer (-1 — there is no capture).
static CAP_CPU: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(-1);
pub static CAP_LOST: _CapWord = _CapWord::new(0);
pub static CAP_BYTES: _CapWord = _CapWord::new(0);
pub static CAP_TSC: _CapWord = _CapWord::new(0);
pub static CAP_WORST_TSC: _CapWord = _CapWord::new(0);
pub static CAP_WORST_N: _CapWord = _CapWord::new(0);

/// Does THIS core's output go into the buffer now? The core is named by MPIDR
/// (`cpu::cpu_index`), not through the per-CPU block: a line printed from a trap with a
/// broken `TPIDR_EL1` must still find its way out.
#[inline]
fn capture_here() -> bool {
    let c = CAP_CPU.load(Ordering::Relaxed);
    c >= 0 && c == super::cpu::cpu_index() as i32
}
fn capture_put(s: &[u8]) {
    let mut n = CAP_LEN.load(Ordering::Relaxed);
    for &b in s {
        if n + 2 > CAP_N { CAP_LOST.fetch_add(1, Ordering::Relaxed); continue; }
        // SAFETY: only the capturing core writes the buffer, below CAP_N; the pourers read below CAP_LEN.
        unsafe {
            if b == b'\n' { CAP_BUF[n] = b'\r'; n += 1; }
            CAP_BUF[n] = b;
        }
        n += 1;
    }
    CAP_LEN.store(n, Ordering::Relaxed);
}
/// Begin capturing THIS core's output. False: the previous block has not been poured out
/// yet, or a capture is already going on — the new block is skipped by the caller.
pub fn capture_begin() -> bool {
    if CAP_OUT.load(Ordering::Relaxed) < CAP_LEN.load(Ordering::Relaxed) { return false; }
    let me = super::cpu::cpu_index() as i32;
    if CAP_CPU.compare_exchange(-1, me, Ordering::SeqCst, Ordering::SeqCst).is_err() { return false; }
    CAP_LEN.store(0, Ordering::SeqCst);
    CAP_OUT.store(0, Ordering::SeqCst);
    true
}
pub fn capture_end() { CAP_CPU.store(-1, Ordering::SeqCst); }
pub fn capture_pending() -> bool { CAP_OUT.load(Ordering::Relaxed) < CAP_LEN.load(Ordering::Relaxed) }
pub fn capture_left() -> usize { CAP_LEN.load(Ordering::Relaxed).saturating_sub(CAP_OUT.load(Ordering::Relaxed)) }
/// Pour out the rest of the block as a whole, waiting for the transmitter: only where there
/// is nobody to shepherd the portions (a workload without a tick).
pub fn capture_drain_blocking() {
    let _g = lock();
    let len = CAP_LEN.load(Ordering::Relaxed);
    let mut i = CAP_OUT.load(Ordering::Relaxed);
    while i < len { write_byte(unsafe { CAP_BUF[i] }); i += 1; }
    CAP_OUT.store(i, Ordering::Relaxed);
}
/// Pour out as much as the FIFO takes RIGHT NOW, within the budget; not a single wait.
pub fn capture_flush(budget_ns: u64, max_bytes: usize) -> usize {
    let len = CAP_LEN.load(Ordering::Relaxed);
    if CAP_OUT.load(Ordering::Relaxed) >= len { return 0; }
    // No console: the block is dropped, not kept waiting for a transmitter that does not exist.
    if BASE.load(Ordering::Relaxed) == 0 { CAP_OUT.store(len, Ordering::Relaxed); return 0; }
    let hz = crate::tsc::hz();
    let limit = if hz == 0 { u64::MAX } else { ((hz as u128 * budget_ns as u128) / 1_000_000_000u128) as u64 };
    let t0 = crate::tsc::rdtsc();
    if LOCK.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() { return 0; }
    // The position is read UNDER the lock: two cores pour, and a position taken before it
    // sent every tenth character twice on x86 (session 32).
    let mut i = CAP_OUT.load(Ordering::Relaxed);
    if i >= len { LOCK.store(false, Ordering::Release); return 0; }
    let start = i;
    while i < len && i - start < max_bytes {
        if tx_full() { break; }                              // the FIFO is full — until the next time
        tx_put(unsafe { CAP_BUF[i] });
        i += 1;
        if hz != 0 && crate::tsc::rdtsc().wrapping_sub(t0) >= limit { break; }
    }
    CAP_OUT.store(i, Ordering::Relaxed);
    LOCK.store(false, Ordering::Release);
    let d = crate::tsc::rdtsc().wrapping_sub(t0);
    CAP_BYTES.fetch_add((i - start) as u64, Ordering::Relaxed);
    CAP_TSC.fetch_add(d, Ordering::Relaxed);
    if d > CAP_WORST_TSC.load(Ordering::Relaxed) {
        CAP_WORST_TSC.store(d, Ordering::Relaxed);
        CAP_WORST_N.store((i - start) as u64, Ordering::Relaxed);
    }
    i - start
}
