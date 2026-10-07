//! The gdb stub on arm64: the GDB Remote Serial Protocol (`gdbproto`) over THE CONSOLE'S
//! UART. x86 talks over COM2; an arm64 machine has one line — QEMU `virt` models a single
//! PL011, a Graviton guest's only serial port is the console's 16550 — so the stub shares it
//! the way Linux's kgdboc does: while the machine is stopped every core but the stub's is
//! parked and the line carries packets alone; while it runs, the console prints and gdb
//! skips whatever is outside a `$...#xx`. A ^C from gdb (0x03) reaches the console's poll,
//! which stops the machine in the debugger (`on_console`).
//!
//! What is supported: `?` `g` `G` `m` `M` `c` `s` `D` `H` `T` `q*` (the shared part),
//! `Z0/z0` (a `brk` written into the kernel's text), `Z1/z1` (the breakpoint registers),
//! `Z2/Z3/Z4` (a watchpoint register: write / read / access). The registers: x0..x30, sp,
//! pc, cpsr — gdb's `aarch64-core`; the FP/SIMD set is reported unavailable.
//!
//! ## The debug exceptions, and what arm64 does not give for free
//!
//! * **No resume flag.** x86 continues over a hardware breakpoint with `RFLAGS.RF`; arm64
//!   re-takes a breakpoint (or a watchpoint) at the same instruction. Every resume from such
//!   a stop, and every step, is a software step with THIS core's breakpoint and watchpoint
//!   registers switched off; the step's exception puts them back and — unless gdb asked for
//!   the step — returns without a word on the line (`INNER`).
//! * **`PSTATE.D`.** The kernel runs with debug exceptions masked (an exception entry sets
//!   D, `daifclr #3` clears only I and F), so the breakpoint and watchpoint registers catch
//!   the WORKLOAD (EL0: never masked by D when the debug target is EL1). In the kernel the
//!   tools are the `brk` (not maskable) and the step, which clears D in the stepped frame.
//! * **The OS lock.** Out of reset `OSLSR_EL1.OSLK` may be set, and with it no breakpoint
//!   fires; every core that applies the registers releases it first.
//!
//! The registers are the machine's state, as on x86: the stub writes the DESIRED values and a
//! generation; every core applies them at its entry into the scheduler (`apply_if_changed`)
//! and the others are kicked there at once.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use crate::gdbproto::{self, Port, PACKET, push_le, read_le_hex, parse_hex};
use super::trap::Frame;

fn putb(b: u8) { super::uart::write_byte(b) }
fn getc() -> Option<u8> { super::uart::getc() }
const PORT: Port = Port { putb, getc };

const NCPU: usize = crate::percpu::MAXCPU;
static ENABLED: AtomicBool = AtomicBool::new(false);

/// The SGI that parks the other cores while one talks to gdb.
pub const PARK_SGI: u32 = 6;

/// `brk #0x401` — the kernel's compiled-in stop (`breakpoint()`); `brk #0x400` — a `Z0`
/// written into the text. Other immediates are someone else's (the bring-up's probes, a
/// workload's `__builtin_trap`) and go their usual way.
const BRK_COMPILED: u64 = 0x401;
const BRK_INSERTED: u64 = 0x400;
const BRK_INSERTED_INSN: u32 = 0xD420_0000 | (BRK_INSERTED as u32) << 5;

// ── the registers, by number ─────────────────────────────────────────────────────────
macro_rules! by_index {
    ($n:expr, $op:ident) => {
        match $n {
            0 => $op!("0"), 1 => $op!("1"), 2 => $op!("2"), 3 => $op!("3"),
            4 => $op!("4"), 5 => $op!("5"), _ => {}
        }
    };
}
unsafe fn dbg_w(kind: u8, n: usize, v: u64) {
    macro_rules! bvr { ($i:literal) => { core::arch::asm!(concat!("msr dbgbvr", $i, "_el1, {}"), in(reg) v, options(nomem, nostack)) }; }
    macro_rules! bcr { ($i:literal) => { core::arch::asm!(concat!("msr dbgbcr", $i, "_el1, {}"), in(reg) v, options(nomem, nostack)) }; }
    macro_rules! wvr { ($i:literal) => { core::arch::asm!(concat!("msr dbgwvr", $i, "_el1, {}"), in(reg) v, options(nomem, nostack)) }; }
    macro_rules! wcr { ($i:literal) => { core::arch::asm!(concat!("msr dbgwcr", $i, "_el1, {}"), in(reg) v, options(nomem, nostack)) }; }
    match kind {
        0 => by_index!(n, bvr),
        1 => by_index!(n, bcr),
        2 => by_index!(n, wvr),
        _ => by_index!(n, wcr),
    }
}
fn mdscr_r() -> u64 { let v: u64; unsafe { core::arch::asm!("mrs {}, mdscr_el1", out(reg) v, options(nomem, nostack)); } v }
unsafe fn mdscr_w(v: u64) { core::arch::asm!("msr mdscr_el1, {}", "isb", in(reg) v, options(nomem, nostack)); }
const MDSCR_SS: u64 = 1 << 0;
const MDSCR_KDE: u64 = 1 << 13;
const MDSCR_MDE: u64 = 1 << 15;
/// SPSR: the software step's bit, D, I.
const SPSR_SS: u64 = 1 << 21;
const SPSR_D: u64 = 1 << 9;
const SPSR_I: u64 = 1 << 7;

/// How many breakpoint / watchpoint registers the core has (`ID_AA64DFR0_EL1`), at most six
/// each are used.
fn nbrp() -> usize {
    let v: u64; unsafe { core::arch::asm!("mrs {}, id_aa64dfr0_el1", out(reg) v, options(nomem, nostack)); }
    (((v >> 12) & 0xF) as usize + 1).min(6)
}
fn nwrp() -> usize {
    let v: u64; unsafe { core::arch::asm!("mrs {}, id_aa64dfr0_el1", out(reg) v, options(nomem, nostack)); }
    (((v >> 20) & 0xF) as usize + 1).min(6)
}

// ── the desired state, published to every core ───────────────────────────────────────
static BP_ADDR: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];     // 0 — the slot is free
static WP_ADDR: AtomicU64 = AtomicU64::new(0);
/// The watchpoint's control word (0 — none).
static WP_CTRL: AtomicU64 = AtomicU64::new(0);
static GEN: AtomicU64 = AtomicU64::new(0);
static APPLIED: [AtomicU64; NCPU] = [const { AtomicU64::new(0) }; NCPU];
/// This core is in a software step with its registers off (`on_exception` puts them back).
static STEPPING: [AtomicBool; NCPU] = [const { AtomicBool::new(false) }; NCPU];

/// Breakpoint control: enabled, all four bytes of the instruction (BAS), EL1 and EL0 (PMC).
const BCR_EXEC: u64 = 1 | (0b11 << 1) | (0xF << 5);

/// Apply the desired registers on this core, if they have changed. While the debugger has
/// set nothing — one load and an exit.
#[inline]
pub fn apply_if_changed(cpu: usize) {
    let g = GEN.load(Ordering::Relaxed);
    if g == 0 || cpu >= NCPU || APPLIED[cpu].load(Ordering::Relaxed) == g { return; }
    if STEPPING[cpu].load(Ordering::Relaxed) { return; } // the step's end applies them
    unsafe { apply_local(true); }
    APPLIED[cpu].store(g, Ordering::Relaxed);
}
/// Write this core's registers: the desired state (`on`) or all off (a step is under way).
unsafe fn apply_local(on: bool) {
    core::arch::asm!("msr oslar_el1, xzr", "msr osdlr_el1, xzr", "isb", options(nomem, nostack));
    for i in 0..nbrp() {
        let a = BP_ADDR[i].load(Ordering::SeqCst);
        dbg_w(1, i, 0);
        if on && a != 0 { dbg_w(0, i, a & !3); dbg_w(1, i, BCR_EXEC); }
    }
    let (wa, wc) = (WP_ADDR.load(Ordering::SeqCst), WP_CTRL.load(Ordering::SeqCst));
    for i in 0..nwrp() { dbg_w(3, i, 0); }
    if on && wc != 0 { dbg_w(2, 0, wa & !7); dbg_w(3, 0, wc); }
    let m = mdscr_r() | MDSCR_MDE | MDSCR_KDE;
    mdscr_w(m);
}
/// Publish the new state: apply it here and send the other cores into their schedulers.
fn publish() {
    let g = GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let me = crate::percpu::cpu_id();
    unsafe { apply_local(true); }
    if me < NCPU { APPLIED[me].store(g, Ordering::SeqCst); }
    let online = crate::percpu::ONLINE.load(Ordering::SeqCst);
    for c in 0..NCPU {
        if c != me && online & (1 << c) != 0 {
            crate::smp::send_ipi(crate::smp::apic_of(c), crate::idt::TIMER_VECTOR as u8);
        }
    }
}
fn set_hw_exec(addr: u64) -> bool {
    let n = nbrp();
    for i in 0..n { if BP_ADDR[i].load(Ordering::SeqCst) == addr { return true; } }
    for i in 0..n {
        if BP_ADDR[i].compare_exchange(0, addr, Ordering::SeqCst, Ordering::SeqCst).is_ok() { publish(); return true; }
    }
    false
}
fn clear_hw_exec(addr: u64) {
    let mut any = false;
    for s in BP_ADDR.iter() { if s.load(Ordering::SeqCst) == addr { s.store(0, Ordering::SeqCst); any = true; } }
    if any { publish(); }
}
/// A watchpoint of `len` bytes (at most 8, inside one doubleword); `lsc`: 1 load, 2 store, 3 both.
fn set_watchpoint(addr: u64, len: u64, lsc: u64) -> bool {
    let off = addr & 7;
    if len == 0 || off + len > 8 { return false; }
    let bas = ((1u64 << len) - 1) << off;
    WP_ADDR.store(addr, Ordering::SeqCst);
    WP_CTRL.store(1 | (0b11 << 1) | (lsc << 3) | (bas << 5), Ordering::SeqCst);
    publish();
    true
}
fn clear_watchpoint() { WP_CTRL.store(0, Ordering::SeqCst); publish(); }

// ── the software breakpoints in the kernel's text ────────────────────────────────────
const BPMAX: usize = 32;
static SW_ADDR: [AtomicU64; BPMAX] = [const { AtomicU64::new(0) }; BPMAX];
static SW_ORIG: [AtomicU64; BPMAX] = [const { AtomicU64::new(0) }; BPMAX];
/// A breakpoint can go at `addr`: it is mapped, and either the image's code (written through
/// the direct map — the alias itself is read-only, W^X) or a writable page.
fn text_writable(addr: u64) -> bool {
    let (present, writable, _, _) = crate::paging::query(addr);
    addr & 3 == 0 && present && (writable || crate::mem::kimg_write_va(addr) != addr)
}
fn poke_insn(addr: u64, insn: u32) {
    let w = crate::mem::kimg_write_va(addr);
    unsafe { core::ptr::write_volatile(crate::mem::vptr::<u32>(w), insn); }
    // The data side by the address written through, the instruction side by the one fetched
    // (`sync` cleans by VA: both name the same lines on a PIPT cache).
    super::icache::sync(w, 4);
    super::icache::sync(addr, 4);
}
fn sw_set(addr: u64) -> bool {
    if !text_writable(addr) { return false; }
    for i in 0..BPMAX { if SW_ADDR[i].load(Ordering::SeqCst) == addr { return true; } }
    for i in 0..BPMAX {
        if SW_ADDR[i].load(Ordering::SeqCst) == 0 {
            SW_ORIG[i].store(unsafe { core::ptr::read_volatile(crate::mem::vptr::<u32>(addr)) } as u64, Ordering::SeqCst);
            SW_ADDR[i].store(addr, Ordering::SeqCst);
            poke_insn(addr, BRK_INSERTED_INSN);
            return true;
        }
    }
    false
}
/// Take the breakpoint at `addr` out (its instruction back); whether there was one.
fn sw_clear(addr: u64) -> bool {
    for i in 0..BPMAX {
        if SW_ADDR[i].load(Ordering::SeqCst) == addr {
            poke_insn(addr, SW_ORIG[i].load(Ordering::SeqCst) as u32);
            SW_ADDR[i].store(0, Ordering::SeqCst);
            return true;
        }
    }
    false
}
fn sw_clear_all() {
    for i in 0..BPMAX {
        let a = SW_ADDR[i].load(Ordering::SeqCst);
        if a != 0 { sw_clear(a); }
    }
}

// ── the entry ────────────────────────────────────────────────────────────────────────

/// Whether the stub is on and talks over the console's line (the console's poll turns a ^C
/// into a stop then). x86's stub has COM2 to itself and says no.
pub fn on_console() -> bool { ENABLED.load(Ordering::Relaxed) }

/// A synchronous exception the stub may own: a `brk` of its own, a breakpoint, a watchpoint
/// or a step — from EL0 or EL1. `true` — handled (the frame is what to return to).
pub fn on_exception(f: &mut Frame, ec: u64) -> bool {
    if !ENABLED.load(Ordering::Relaxed) { return false; }
    let me = crate::percpu::cpu_id();
    match ec {
        0x3C => {
            let imm = f.esr & 0xFFFF;
            if imm == BRK_COMPILED { f.pc += 4; }        // resume after it, as x86's int3 does
            else if imm == BRK_INSERTED { sw_clear(f.pc); } // consumed: gdb inserts again before it resumes
            else { return false; }
        }
        0x30..=0x35 => {
            if ec == 0x32 || ec == 0x33 {
                // The end of a step: this core's registers back on, SS off, the frame's D and I
                // as they were. A step nobody asked for is not ours.
                if me >= NCPU || !STEPPING[me].swap(false, Ordering::SeqCst) { return false; }
                end_step(f, me);
                if INNER[me].swap(false, Ordering::SeqCst) { return true; } // a step-over: silent
            }
            // A breakpoint or a watchpoint: only the stub writes those registers.
        }
        _ => return false,
    }
    stop(f, ec);
    true
}

/// The D and I bits of the stepped frame before the step, per core (restored at its end).
static STEP_SPSR: [AtomicU64; NCPU] = [const { AtomicU64::new(0) }; NCPU];
/// The step was the stub's own (over a hardware breakpoint or a watchpoint on `c`).
static INNER: [AtomicBool; NCPU] = [const { AtomicBool::new(false) }; NCPU];

fn begin_step(f: &mut Frame, me: usize, inner: bool) {
    if me >= NCPU { return; }
    STEP_SPSR[me].store(f.flags & (SPSR_D | SPSR_I), Ordering::SeqCst);
    // Interrupts masked for the stepped instruction (kgdb's rule, x86's TF+IF): a pending
    // tick would be taken first and the step would land in its vector. D cleared: at EL1
    // the step's exception is masked by it.
    f.flags = (f.flags | SPSR_SS | SPSR_I) & !SPSR_D;
    STEPPING[me].store(true, Ordering::SeqCst);
    INNER[me].store(inner, Ordering::SeqCst);
    unsafe {
        apply_local(false); // the breakpoint at this pc would be taken before the step
        mdscr_w(mdscr_r() | MDSCR_SS | MDSCR_KDE | MDSCR_MDE);
    }
}
fn end_step(f: &mut Frame, me: usize) {
    let keep = STEP_SPSR[me].load(Ordering::SeqCst);
    f.flags = (f.flags & !(SPSR_SS | SPSR_D | SPSR_I)) | keep;
    unsafe {
        mdscr_w(mdscr_r() & !MDSCR_SS);
        apply_local(true);
    }
    APPLIED[me].store(GEN.load(Ordering::SeqCst), Ordering::SeqCst);
}

/// Into the stub one core at a time; the others park (all-stop), as on x86.
static IN_STUB: AtomicBool = AtomicBool::new(false);
static STOP_ALL: AtomicBool = AtomicBool::new(false);
static STEP_OWNER: AtomicUsize = AtomicUsize::new(usize::MAX);
/// The frame of the interrupted thread of a parked core (0 — not parked).
static PARK_FRAME: [AtomicU64; NCPU] = [const { AtomicU64::new(0) }; NCPU];

const ACT_CONT: u8 = 0;
const ACT_STEP: u8 = 1;
const ACT_DETACH: u8 = 2;

fn stop(f: &mut Frame, ec: u64) {
    let me = crate::percpu::cpu_id();
    let ours = |o: usize| o == usize::MAX || o == me;
    loop {
        if ours(STEP_OWNER.load(Ordering::Acquire)) && !IN_STUB.swap(true, Ordering::Acquire) {
            if ours(STEP_OWNER.load(Ordering::Acquire)) { break; }
            IN_STUB.store(false, Ordering::Release);
        }
        core::hint::spin_loop();
    }
    STEP_OWNER.store(usize::MAX, Ordering::Release);
    if !STOP_ALL.swap(true, Ordering::SeqCst) {
        let online = crate::percpu::ONLINE.load(Ordering::SeqCst);
        for c in 0..NCPU {
            if c != me && online & (1 << c) != 0 { crate::smp::send_ipi(crate::smp::apic_of(c), PARK_SGI as u8); }
        }
    }
    let act = converse(f);
    // A resume from a hardware breakpoint or a watchpoint steps over the instruction first
    // (no resume flag on arm64); a step is a step.
    if act == ACT_STEP { begin_step(f, me, false); STEP_OWNER.store(me, Ordering::Release); }
    else {
        if act == ACT_CONT && matches!(ec, 0x30 | 0x31 | 0x34 | 0x35) { begin_step(f, me, true); }
        STOP_ALL.store(false, Ordering::SeqCst);
    }
    IN_STUB.store(false, Ordering::Release);
}

/// The parking SGI: stand until gdb releases the machine, then take the fresh registers.
pub fn park(f: &mut Frame) {
    let me = crate::percpu::cpu_id();
    if me < NCPU { PARK_FRAME[me].store(crate::mem::vaddr(f as *mut Frame), Ordering::SeqCst); }
    while STOP_ALL.load(Ordering::Acquire) && STEP_OWNER.load(Ordering::Acquire) != me { core::hint::spin_loop(); }
    if me < NCPU { PARK_FRAME[me].store(0, Ordering::SeqCst); }
    apply_if_changed(me);
}

/// gdb's `aarch64-core` order: x0..x30, sp, pc (64 bits each), cpsr (32 bits).
fn send_regs(f: &Frame) {
    let mut buf = [0u8; 600]; let mut n = 0;
    for i in 0..31 { push_le(f.x(i), 8, &mut buf, &mut n); }
    push_le(f.usp, 8, &mut buf, &mut n);
    push_le(f.pc, 8, &mut buf, &mut n);
    push_le(f.flags, 4, &mut buf, &mut n);
    PORT.send(&buf[..n]);
}

/// gdb is attached: it resumed the machine and waits for the stop reply. Before the first
/// packet (a `brk` the kernel reached on its own, `talaria.gdb=break`) and after a detach the
/// stub says nothing until gdb asks (`?`): an unsolicited reply that crossed gdb's opening
/// `qSupported` on the wire was taken for its answer, and the attach failed.
static ATTACHED: AtomicBool = AtomicBool::new(false);
/// The stop is gdb's own ^C (`interrupt`): it waits for the reply.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

fn converse(f: &mut Frame) -> u8 {
    let mut buf = [0u8; PACKET];
    let me = crate::sched::current_id() as u64 + 1;
    let mut sel = me;
    let mut stop = [0u8; 40];
    let sn = gdbproto::stop_reply(&mut stop, me, crate::percpu::cpu_id() as u64);
    if ATTACHED.load(Ordering::SeqCst) | INTERRUPTED.swap(false, Ordering::SeqCst) { PORT.send(&stop[..sn]); }
    loop {
        let n = PORT.recv(&mut buf);
        ATTACHED.store(true, Ordering::SeqCst);
        if n == 0 { PORT.send(b""); continue; }
        if gdbproto::common(&PORT, &buf, n, me, &mut sel) { continue; }
        match buf[0] {
            b'?' => PORT.send(&stop[..sn]),
            b'g' if sel == me => send_regs(f),
            b'g' => {
                // A thread on a parked core: its registers are the parking frame; else the
                // context the scheduler saved (the same 36-word frame); else zeroes (an E01
                // made gdb repeat the request forever).
                let t = sel as usize - 1;
                let mut parked = 0u64;
                for c in 0..NCPU {
                    let pf = PARK_FRAME[c].load(Ordering::SeqCst);
                    if pf != 0 && crate::percpu::cur_task_of(c) == t as u64 { parked = pf; break; }
                }
                if parked != 0 { send_regs(unsafe { &*crate::mem::vptr::<Frame>(parked) }); continue; }
                let mut tf: Frame = unsafe { core::mem::zeroed() };
                if let Some(c) = crate::sched::saved_ctx(t) {
                    // SAFETY: the saved context IS a trap frame, CTX_WORDS words of it.
                    unsafe { core::ptr::copy_nonoverlapping(c.as_ptr(), &mut tf as *mut Frame as *mut u64, super::switch::CTX_WORDS.min(36)); }
                }
                send_regs(&tf);
            }
            b'G' if sel != me => PORT.send(b"E01"),
            b'G' => {
                let reg = |i: usize| read_le_hex(&buf, 1 + i * 16, 8);
                for i in 0..31 { f.set_x(i, reg(i)); }
                // sp: an EL0 frame's SP_EL0 is restored from the slot; an EL1 frame's sp is
                // where the frame ends and is not the stub's to move.
                if super::switch::mode_is_user(f.flags) { f.usp = reg(31); }
                f.pc = reg(32);
                f.flags = (f.flags & !0xFFFF_FFFF) | read_le_hex(&buf, 1 + 33 * 16, 4);
                PORT.send(b"OK");
            }
            b'Z' | b'z' if n >= 3 => {
                let set = buf[0] == b'Z';
                let addr = parse_hex(&buf[3..n]);
                let mut i = 3; while i < n && buf[i] != b',' { i += 1; }
                let len = parse_hex(&buf[(i + 1).min(n)..n]);
                let ok = match (buf[1], set) {
                    (b'0', true) => sw_set(addr),
                    (b'0', false) => { sw_clear(addr); true }
                    (b'1', true) => set_hw_exec(addr),
                    (b'1', false) => { clear_hw_exec(addr); true }
                    (b'2', true) => set_watchpoint(addr, len, 2),
                    (b'3', true) => set_watchpoint(addr, len, 1),
                    (b'4', true) => set_watchpoint(addr, len, 3),
                    (b'2' | b'3' | b'4', false) => { clear_watchpoint(); true }
                    _ => { PORT.send(b""); continue; }
                };
                PORT.send(if ok { b"OK" } else { b"E0e" });
            }
            b'c' => return ACT_CONT,
            b's' => return ACT_STEP,
            // Detach: every breakpoint out, the machine released (without the answer gdb
            // printed "Can't detach" and kept it stopped).
            b'D' => {
                sw_clear_all();
                for s in BP_ADDR.iter() { s.store(0, Ordering::SeqCst); }
                WP_CTRL.store(0, Ordering::SeqCst);
                publish();
                PORT.send(b"OK");
                ATTACHED.store(false, Ordering::SeqCst);
                return ACT_DETACH;
            }
            _ => PORT.send(b""),
        }
    }
}

/// `talaria.gdb=break`: enter the debugger before the workload's first instruction.
pub static BREAK_AT_START: AtomicBool = AtomicBool::new(false);
/// Called by the scheduler before the entry into the first task (once).
pub fn maybe_break_at_start() {
    if BREAK_AT_START.swap(false, Ordering::SeqCst) {
        enable();
        crate::println!("gdb: talaria.gdb=break — we stand before the workload's first instruction; gdb on the console's line");
        breakpoint();
    }
}

/// Switch the stub on: from now on its `brk`s, the breakpoint and watchpoint registers and the
/// steps stop the machine in it.
pub fn enable() {
    if !ENABLED.swap(true, Ordering::SeqCst) {
        crate::println!("gdb: the stub speaks the RSP on the console's line (arm64 has one) — `target remote` to it; ^C stops the machine");
    }
}

/// gdb's ^C reached the console's poll: stop, and answer it.
pub fn interrupt() { INTERRUPTED.store(true, Ordering::SeqCst); breakpoint(); }

/// The kernel's compiled-in stop: `brk #0x401`. Without the stub it is a fatal trap.
#[inline(always)]
pub fn breakpoint() { unsafe { core::arch::asm!("brk #0x401", options(nomem, nostack)); } }
