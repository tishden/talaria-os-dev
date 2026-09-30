//! The gdb stub over the second UART is phase 8 (docs/ARM64-PLAN.md §5): the x86 module's
//! surface, and `enable` says so once instead of pretending.
use core::sync::atomic::{AtomicBool, Ordering};
static SAID: AtomicBool = AtomicBool::new(false);
pub fn enable() {
    if !SAID.swap(true, Ordering::Relaxed) { crate::println!("gdb: the stub is not ported to arm64 yet (phase 8)"); }
}
pub fn apply_if_changed(_cpu: usize) {}
pub fn maybe_break_at_start() {}
/// `brk #0` — the debugger's breakpoint instruction; without a stub it is a fatal trap.
pub fn breakpoint() { unsafe { core::arch::asm!("brk #0", options(nomem, nostack)); } }
