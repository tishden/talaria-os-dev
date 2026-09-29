//! The I/O APIC: the delivery of interrupts from DEVICES (rather than from cores).
//!
//! Why it was needed. There was no network interrupt in this build at all: the stack
//! was driven ONLY from the syscalls, that is, a frame was seen only by the thread that
//! entered the stack itself. The measurement of session 21 showed what that is paid with: in mongod
//! the stage "the data is in the socket -> ring 3 has taken it" has a p50 of 1.4 us at a p99 of
//! 182 us — the median is excellent, while in a hundredth of the cases a frame lies and waits until
//! the socket's owner gets to the processor. Neither the scheduler nor the targeted
//! handover of the processor cures that (it was checked and rejected by measurement):
//! what cures it is that the frame ITSELF wakes the machine.
//!
//! We have had a LAPIC for a long time (the tick, IPIs, MSI-X at the ENA), but the line from virtio-mmio
//! under a microvm goes through the I/O APIC, which was not in the kernel. Here is exactly what
//! is needed for one line: find the controller in the MADT and lay a record of the
//! redirection table "a GSI -> a vector on the BSP".
//!
//! There are two registers: the IOREGSEL at the base and the IOWIN at base+0x10. A record of the redirection
//! table occupies TWO 32-bit words (the indices 0x10+2n and 0x10+2n+1),
//! and the order of the writes matters: first the upper word (where to deliver), then
//! the lower one (with what to deliver and clear the mask) — otherwise the window between them may
//! deliver an interrupt to the wrong addressee yet.
use core::sync::atomic::{AtomicU64, AtomicU32, Ordering};

/// There may be SEVERAL controllers, and that is not exotic: a microvm has two.
/// The first covers GSIs 0..23, the second starts at 24 — while the virtio-mmio windows there
/// occupy the slots up to the 23rd, that is, the network card's line (GSI 5+23 = 28) lives
/// EXACTLY ON THE SECOND. While only the first was remembered, the laying refused with
/// "lines 24" and looked like an absence of support.
const MAXIOAPIC: usize = 4;
static BASE: [AtomicU64; MAXIOAPIC] = [const { AtomicU64::new(0) }; MAXIOAPIC];
static GSI_BASE: [AtomicU32; MAXIOAPIC] = [const { AtomicU32::new(0) }; MAXIOAPIC];
static NIOAPIC: AtomicU32 = AtomicU32::new(0);

/// Remember a controller (it is called by the parsing of the MADT).
pub fn note(addr: u64, gsi_base: u32) {
    let n = NIOAPIC.load(Ordering::Relaxed) as usize;
    if n >= MAXIOAPIC { return; }
    BASE[n].store(addr, Ordering::SeqCst);
    GSI_BASE[n].store(gsi_base, Ordering::SeqCst);
    NIOAPIC.store(n as u32 + 1, Ordering::SeqCst);
}

pub fn available() -> bool { NIOAPIC.load(Ordering::Relaxed) != 0 }
pub fn count() -> usize { NIOAPIC.load(Ordering::Relaxed) as usize }

unsafe fn read(b: u64, reg: u32) -> u32 {
    core::ptr::write_volatile(b as *mut u32, reg);
    core::ptr::read_volatile((b + 0x10) as *const u32)
}
unsafe fn write(b: u64, reg: u32, val: u32) {
    core::ptr::write_volatile(b as *mut u32, reg);
    core::ptr::write_volatile((b + 0x10) as *mut u32, val);
}

/// How many lines the controller `i` has (from the version register).
pub fn lines_of(i: usize) -> u32 {
    if i >= count() { return 0; }
    unsafe { ((read(BASE[i].load(Ordering::Relaxed), 0x01) >> 16) & 0xFF) + 1 }
}

/// Which controller serves this GSI.
fn owner(gsi: u32) -> Option<(u64, u32)> {
    for i in 0..count() {
        let g = GSI_BASE[i].load(Ordering::Relaxed);
        if gsi >= g && gsi - g < lines_of(i) { return Some((BASE[i].load(Ordering::Relaxed), gsi - g)); }
    }
    None
}

/// Lay the line `gsi` onto the `vector` of the core with the APIC id `apic`.
/// An edge, active high, a physical addressee, the fixed mode, the mask cleared.
///
/// The order of the writes matters: first the UPPER word (where to deliver), then the lower one
/// (with what to deliver and clear the mask) — otherwise the window between them would deliver an interrupt
/// to the wrong addressee yet.
pub fn route(gsi: u32, vector: u8, apic: u32) -> bool {
    let Some((b, n)) = owner(gsi) else { return false };
    unsafe {
        write(b, 0x11 + 2 * n, apic << 24);
        write(b, 0x10 + 2 * n, vector as u32);
    }
    true
}

/// The highest served GSI (it is needed in order to derive the virtio-mmio lines:
/// under a microvm they lie at the top of the space).
pub fn top_gsi() -> u32 {
    let mut top = 0;
    for i in 0..count() {
        let g = GSI_BASE[i].load(Ordering::Relaxed) + lines_of(i);
        if g > top { top = g; }
    }
    top.saturating_sub(1)
}

pub fn dump() {
    if !available() { crate::println!("ioapic: it was not found in the MADT"); return; }
    for i in 0..count() {
        crate::println!("ioapic[{}]: the base {:#x}, lines {}, the GSIs from {}",
            i, BASE[i].load(Ordering::Relaxed), lines_of(i), GSI_BASE[i].load(Ordering::Relaxed));
    }
}
