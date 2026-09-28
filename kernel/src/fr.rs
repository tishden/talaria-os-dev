//! The flight recorder: a ring of the kernel's latest events in memory. It is written on the hot
//! and control paths (boot, entering ring 3, mprotect, a fault, a restart); it is dumped
//! into the crash dump (idt.rs) and by the console command `log`. A post-mortem without an external
//! debugger: it shows what led to the failure.
use core::sync::atomic::{AtomicU64, Ordering};

const N: usize = 64; // a power of two
static POS: AtomicU64 = AtomicU64::new(0);
static mut TS: [u64; N] = [0; N];
static mut KIND: [u8; N] = [0; N];
static mut A: [u64; N] = [0; N];

pub const BOOT: u8 = 0;
pub const PAGING: u8 = 1;
pub const MODE: u8 = 2;
pub const USER: u8 = 3;
pub const SYSINIT: u8 = 4;
pub const FAULT: u8 = 5;
pub const RESTART: u8 = 6;
pub const MPROTECT: u8 = 7;
pub const NOTE: u8 = 8;
pub const SYSDENY: u8 = 9;

fn name(k: u8) -> &'static str {
    match k {
        BOOT => "boot", PAGING => "paging", MODE => "mode", USER => "user-enter",
        SYSINIT => "syscall-init", FAULT => "FAULT", RESTART => "restart",
        MPROTECT => "mprotect", NOTE => "note", SYSDENY => "syscall-deny", _ => "?",
    }
}

/// Record an event (kind, an arbitrary value a). Lock-free; exact on one core,
/// on several a rare overlap is acceptable (diagnostics, not critical).
pub fn record(kind: u8, a: u64) {
    let i = POS.fetch_add(1, Ordering::Relaxed) as usize & (N - 1);
    let t = crate::tsc::rdtsc();
    unsafe {
        core::ptr::write_volatile(core::ptr::addr_of_mut!(TS[i]), t);
        core::ptr::write_volatile(core::ptr::addr_of_mut!(KIND[i]), kind);
        core::ptr::write_volatile(core::ptr::addr_of_mut!(A[i]), a);
    }
}

/// Dump the ring to serial (from the oldest to the newest). Prints Δtsc relative to the first.
pub fn dump() {
    let total = POS.load(Ordering::Relaxed);
    if total == 0 { crate::println!("flight-recorder: empty"); return; }
    let shown = if total as usize > N { N } else { total as usize };
    let start = total - shown as u64;
    crate::println!("╠═══ FLIGHT RECORDER: {} events (the last {}) ═══", total, shown);
    // the time base is the oldest shown element
    let base = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(TS[(start as usize) & (N - 1)])) };
    for j in 0..shown {
        let idx = (start as usize + j) & (N - 1);
        let (t, k, a) = unsafe {
            (core::ptr::read_volatile(core::ptr::addr_of!(TS[idx])),
             core::ptr::read_volatile(core::ptr::addr_of!(KIND[idx])),
             core::ptr::read_volatile(core::ptr::addr_of!(A[idx])))
        };
        crate::println!("║ [{:>3}] +{:>10} tsc  {:<12} a={:#x}", start as usize + j, t.wrapping_sub(base), name(k), a);
    }
}
