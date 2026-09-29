//! The VGA text screen (0xB8000, 80x25) — the kernel's SECOND output channel.
//!
//! Why. On `*.metal` `get-console-output` returns the console of the
//! FIRMWARE rather than the COM1 port: a probe showed that code which certainly
//! did execute in sector 0 printed to 0x3F8 and was not visible by a single line,
//! while GRUB and the Linux kernel are visible in the same console. So on bare metal
//! our kernel is mute in principle — regardless of whether it booted or not.
//! The firmware redirects what a guest writes through `int 10h` and into the text
//! VGA buffer; in long mode the second path is available to us — a direct write into
//! 0xB8000.
//!
//! The channel is for debugging: writing to it is expensive (a device write + `clflush`),
//! and it never reaches the datapath — just like COM1.
//!
//! Synchronisation: the cursor position is an ordinary atomic, the lock is taken one level up
//! (`serial::puts_locked` / `Serial::write_str`). Interleaved output from different
//! cores is no worse here than in COM1.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const VGA: u64 = 0xB8000; // identity-mapped (the first 2 MB) — the kernel is already there
const COLS: usize = 80;
const ROWS: usize = 25;
const ATTR: u16 = 0x0700; // grey on black

static ENABLED: AtomicBool = AtomicBool::new(false);
static POS: AtomicUsize = AtomicUsize::new(0); // the cell number in [0, COLS*ROWS)

#[inline]
unsafe fn cell(i: usize) -> *mut u16 { (VGA as *mut u16).add(i) }

/// The cache line holding the cell goes back to memory: the screen buffer on bare metal lives
/// in a device, and a write deferred in the cache is invisible to the firmware.
#[inline]
unsafe fn flush(i: usize) {
    core::arch::asm!("clflush [{}]", in(reg) cell(i), options(nostack, preserves_flags));
}

fn put_cell(i: usize, ch: u8) {
    unsafe {
        core::ptr::write_volatile(cell(i), ATTR | ch as u16);
        flush(i);
    }
}

/// The hardware cursor (0x3D4/0x3D5): some firmware redirects the screen
/// by tracking exactly it.
fn set_cursor(pos: usize) {
    unsafe {
        crate::serial::outb(0x3D4, 0x0F);
        crate::serial::outb(0x3D5, (pos & 0xFF) as u8);
        crate::serial::outb(0x3D4, 0x0E);
        crate::serial::outb(0x3D5, ((pos >> 8) & 0xFF) as u8);
    }
}

pub fn clear() {
    for i in 0..COLS * ROWS { put_cell(i, b' '); }
    POS.store(0, Ordering::Relaxed);
    set_cursor(0);
}

/// Enable the screen as the second output channel (and clear it).
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
    clear();
}

pub fn disable() { ENABLED.store(false, Ordering::Relaxed); }

#[inline]
pub fn enabled() -> bool { ENABLED.load(Ordering::Relaxed) }

fn scroll() {
    unsafe {
        for i in 0..COLS * (ROWS - 1) {
            let v = core::ptr::read_volatile(cell(i + COLS));
            core::ptr::write_volatile(cell(i), v);
            flush(i);
        }
    }
    for i in COLS * (ROWS - 1)..COLS * ROWS { put_cell(i, b' '); }
}

/// Print a byte. `\n` moves to a new line, `\r` is ignored (the COM1 path sends it).
pub fn putb(b: u8) {
    if !enabled() { return; }
    let mut p = POS.load(Ordering::Relaxed);
    match b {
        b'\r' => return,
        b'\n' => p = (p / COLS + 1) * COLS,
        0x08 => { if p > 0 { p -= 1; put_cell(p, b' '); } }
        _ => { put_cell(p, b); p += 1; }
    }
    if p >= COLS * ROWS {
        scroll();
        p = COLS * (ROWS - 1);
    }
    POS.store(p, Ordering::Relaxed);
    set_cursor(p);
}

/// Read a cell's character — for the self-check (`talaria.test=vga`).
pub fn peek(i: usize) -> u8 {
    unsafe { (core::ptr::read_volatile(cell(i)) & 0xFF) as u8 }
}

/// Whether the string `pat` is in screen row number `row`.
pub fn row_contains(row: usize, pat: &[u8]) -> bool {
    if row >= ROWS || pat.is_empty() || pat.len() > COLS { return false; }
    let base = row * COLS;
    for off in 0..=(COLS - pat.len()) {
        let mut ok = true;
        for (k, &c) in pat.iter().enumerate() {
            if peek(base + off + k) != c { ok = false; break; }
        }
        if ok { return true; }
    }
    false
}

/// Whether the string `pat` is anywhere on the screen.
pub fn contains(pat: &[u8]) -> bool { (0..ROWS).any(|r| row_contains(r, pat)) }
