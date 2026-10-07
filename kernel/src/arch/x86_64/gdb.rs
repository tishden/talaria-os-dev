//! A gdb stub: the GDB Remote Serial Protocol over COM2. Remote debugging of the kernel and of ring 3 from a
//! dev host: `target remote :1234` (QEMU -serial tcp::1234,server). What is supported:
//! ? g G(a write of the registers) m M c s H Z0 z0 (sw-bp) Z2/Z3/Z4 z2/z3/z4 (a hw watchpoint
//! through DR0/DR7) qSupported qfThreadInfo qsThreadInfo qC qAttached. The entry is by
//! a #BP(3)/#DB(1) with a full ExcFrame.
use crate::serial::{inb, outb};
use crate::gdbproto::{self, Port, PACKET, push_le, read_le_hex, parse_hex};

const COM2: u16 = 0x2F8;
pub fn init_com2() { unsafe {
    outb(COM2+1,0x00); outb(COM2+3,0x80); outb(COM2+0,0x01); outb(COM2+1,0x00);
    outb(COM2+3,0x03); outb(COM2+2,0xC7); outb(COM2+4,0x03);
}}
fn putb(b: u8) { unsafe { while inb(COM2+5)&0x20==0 {} outb(COM2,b); } }
fn getc() -> Option<u8> { unsafe { if inb(COM2+5)&0x01!=0 { Some(inb(COM2)) } else { None } } }
const PORT: Port = Port { putb, getc };

// The frame as in idt.rs (the same layout).
#[repr(C)]
pub struct Frame {
    pub r15: u64, pub r14: u64, pub r13: u64, pub r12: u64, pub r11: u64, pub r10: u64, pub r9: u64, pub r8: u64,
    pub rbp: u64, pub rdi: u64, pub rsi: u64, pub rdx: u64, pub rcx: u64, pub rbx: u64, pub rax: u64,
    pub vec: u64, pub err: u64, pub rip: u64, pub cs: u64, pub rflags: u64, pub rsp: u64, pub ss: u64,
}

const BPMAX: usize = 32;
static mut BP_ADDR: [u64; BPMAX] = [0; BPMAX];
static mut BP_ORIG: [u8; BPMAX] = [0; BPMAX];
static mut BP_USED: [bool; BPMAX] = [false; BPMAX];

/// The registers in gdb's x86-64 `g` order: rax rbx rcx rdx rsi rdi rbp rsp r8..r15 rip,
/// then eflags cs ss ds es fs gs (32 bits each).
fn send_regs_packet(f: &Frame) {
    let mut buf = [0u8; 512]; let mut n = 0;
    let regs64 = [f.rax,f.rbx,f.rcx,f.rdx,f.rsi,f.rdi,f.rbp,f.rsp,f.r8,f.r9,f.r10,f.r11,f.r12,f.r13,f.r14,f.r15,f.rip];
    for &r in regs64.iter() { push_le(r, 8, &mut buf, &mut n); }
    push_le(f.rflags, 4, &mut buf, &mut n); push_le(f.cs, 4, &mut buf, &mut n); push_le(f.ss, 4, &mut buf, &mut n);
    for _ in 0..4 { push_le(0, 4, &mut buf, &mut n); }
    PORT.send(&buf[..n]);
}

// THE DEBUG REGISTERS ARE THE MACHINE'S STATE, NOT THE KERNEL'S. DR0-DR7 are each core's
// own, and the stub executes on the one where it stopped: before, a breakpoint set
// from gdb was armed only on it, and a thread on a neighbouring core went straight
// through it — live debugging on SMP did not work. The stub corrects the DESIRED
// state and the generation's number; every core applies it at its own entry into the
// scheduler and in the pickup loop (`apply_if_changed`), while the other cores are immediately
// sent an entry into the scheduler (a dedicated core has no tick at all).
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
const NCPU: usize = crate::percpu::MAXCPU;
static DR_ADDR: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static DR7_WANT: AtomicU64 = AtomicU64::new(0);
static DR_GEN: AtomicU64 = AtomicU64::new(0);
static DR_APPLIED: [AtomicU64; NCPU] = [const { AtomicU64::new(0) }; NCPU];

/// Apply the desired debug registers on this core, if they have changed.
/// While the debugger has never yet set any breakpoints — one load and an exit.
#[inline]
pub fn apply_if_changed(cpu: usize) {
    let g = DR_GEN.load(Ordering::Relaxed);
    if g == 0 || cpu >= NCPU || DR_APPLIED[cpu].load(Ordering::Relaxed) == g { return; }
    unsafe { apply_local(); }
    DR_APPLIED[cpu].store(g, Ordering::Relaxed);
}
unsafe fn apply_local() {
    let a = [DR_ADDR[0].load(Ordering::SeqCst), DR_ADDR[1].load(Ordering::SeqCst),
             DR_ADDR[2].load(Ordering::SeqCst), DR_ADDR[3].load(Ordering::SeqCst)];
    core::arch::asm!("mov dr7, {}", in(reg) 0u64, options(nomem, nostack)); // first switch it off
    core::arch::asm!("mov dr0, {}", in(reg) a[0], options(nomem, nostack));
    core::arch::asm!("mov dr1, {}", in(reg) a[1], options(nomem, nostack));
    core::arch::asm!("mov dr2, {}", in(reg) a[2], options(nomem, nostack));
    core::arch::asm!("mov dr3, {}", in(reg) a[3], options(nomem, nostack));
    core::arch::asm!("mov dr7, {}", in(reg) DR7_WANT.load(Ordering::SeqCst), options(nomem, nostack));
}
/// Publish the new state: apply it here and call the other cores.
fn publish() {
    let g = DR_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let me = crate::percpu::cpu_id();
    unsafe { apply_local(); }
    if me < NCPU { DR_APPLIED[me].store(g, Ordering::SeqCst); }
    let online = crate::percpu::ONLINE.load(Ordering::SeqCst);
    for c in 0..NCPU {
        if c != me && online & (1 << c) != 0 {
            crate::smp::send_ipi(crate::smp::apic_of(c), crate::idt::TIMER_VECTOR as u8);
        }
    }
}
fn dr7_update(clear: u64, set: u64) {
    let v = (DR7_WANT.load(Ordering::SeqCst) & !clear) | set;
    DR7_WANT.store(v, Ordering::SeqCst);
}

/// Set a hardware watchpoint (DR0, 4 bytes). rw: 0b01=write, 0b11=access.
unsafe fn set_watchpoint(addr: u64, rw: u64) {
    DR_ADDR[0].store(addr, Ordering::SeqCst);
    dr7_update(0x000F_0003, 0x1 | (rw << 16) | (0b11 << 18)); // L0=1, R/W0=rw, LEN0=4 bytes
    publish();
}
/// Hardware breakpoints on EXECUTION — DR1..DR3 (R/W=00, LEN=00; DR0
/// is occupied by the watchpoint of Z2-Z4). They are needed for the workload's code: its text is mapped R-X
/// (W^X), and a software breakpoint (0xCC) cannot be written there from the kernel — CR0.WP
/// is on. In gdb that is `hbreak`; at once — up to three.
static mut HW_EXEC: [u64; 4] = [0; 4]; // the address by the slot DR1..DR3 (0 — free)
fn slot_bits(k: usize) -> (u64, u64) { (1u64 << (k * 2), 0xFu64 << (16 + k * 4)) } // (Lk, R/Wk+LENk)
/// false — there are no free slots (gdb will say so itself by the E answer).
unsafe fn set_hw_exec(addr: u64) -> bool {
    for k in 1..4 { if HW_EXEC[k] == addr { return true; } }
    for k in 1..4 {
        if HW_EXEC[k] == 0 {
            HW_EXEC[k] = addr;
            DR_ADDR[k].store(addr, Ordering::SeqCst);
            let (l, rwlen) = slot_bits(k);
            dr7_update(rwlen, l); // R/W=00 (execution), LEN=00
            publish();
            return true;
        }
    }
    false
}
unsafe fn clear_hw_exec(addr: u64) {
    let mut any = false;
    for k in 1..4 { if HW_EXEC[k] == addr { HW_EXEC[k] = 0; dr7_update(slot_bits(k).0, 0); any = true; } }
    if any { publish(); }
}
unsafe fn clear_hw_exec_all() {
    for k in 1..4 { if HW_EXEC[k] != 0 { HW_EXEC[k] = 0; dr7_update(slot_bits(k).0, 0); } }
    publish();
}
/// Remove a watchpoint (clear L0 in DR7).
unsafe fn clear_watchpoint() {
    dr7_update(0x1, 0);
    publish();
}

/// `talaria.gdb=break`: enter the debugger before the workload's first instruction —
/// to set breakpoints on its start while nothing has been executed.
pub static BREAK_AT_START: AtomicBool = AtomicBool::new(false);
/// It is called by the scheduler before the entry into the first task (once).
pub fn maybe_break_at_start() {
    if BREAK_AT_START.swap(false, Ordering::SeqCst) {
        enable();
        crate::println!("gdb: talaria.gdb=break — we stand before the workload's first instruction, `target remote :1234`");
        breakpoint();
    }
}

/// Into the stub one at a time. On SMP a breakpoint may be touched by two cores at once, while COM2 and
/// the conversation with gdb are one: the second waits until the first is released (`c`/`s`/`D`).
static IN_STUB: AtomicBool = AtomicBool::new(false);

/// The main loop of the gdb stub. It returns control at a 'c'/'s' (the iretq will resume).
#[unsafe(no_mangle)]
pub extern "C" fn rust_gdb(f: *mut Frame) {
    // ALL-STOP. While one core talks to gdb, the others stand (an IPI onto
    // `PARK_VECTOR`). Otherwise gdb, stepping over a breakpoint, got a stop from
    // ANOTHER thread — a neighbouring core touched a breakpoint not yet removed at its place — and
    // crashed itself (`finish_step_over: Assertion ... trap_expected`). A step (`s`)
    // releases only the stepping core: the others stay parked, while those that
    // managed to touch a breakpoint earlier wait until the step is over.
    let me = crate::percpu::cpu_id();
    let ours = |o: usize| o == usize::MAX || o == me;
    loop {
        if ours(STEP_OWNER.load(Ordering::Acquire)) && !IN_STUB.swap(true, Ordering::Acquire) {
            // The step's owner could have been appointed between the check and the capture.
            if ours(STEP_OWNER.load(Ordering::Acquire)) { break; }
            IN_STUB.store(false, Ordering::Release);
        }
        core::hint::spin_loop();
    }
    STEP_OWNER.store(usize::MAX, Ordering::Release);
    // The step is over: return to the thread the interrupt flag removed for the time of the step.
    let f_ = unsafe { &mut *f };
    if me < NCPU && STEP_IF[me].swap(false, Ordering::SeqCst) { f_.rflags |= 0x200; }
    if !STOP_ALL.swap(true, Ordering::SeqCst) {
        let online = crate::percpu::ONLINE.load(Ordering::SeqCst);
        for c in 0..NCPU {
            if c != me && online & (1 << c) != 0 {
                crate::smp::send_ipi(crate::smp::apic_of(c), PARK_VECTOR as u8);
            }
        }
    }
    let act = rust_gdb_inner(f);
    if act == ACT_STEP {
        // A STEP GOES WITH THE INTERRUPTS CLOSED (as in kgdb). Otherwise an interrupt
        // that was waiting for this core would be delivered earlier than the stepped instruction:
        // the tick would take the core away to another task, while a stale parking IPI
        // would park the stepping core itself — the step would never return.
        let f_ = unsafe { &mut *f };
        if f_.rflags & 0x200 != 0 && me < NCPU { f_.rflags &= !0x200; STEP_IF[me].store(true, Ordering::SeqCst); }
        STEP_OWNER.store(me, Ordering::Release);
    }
    else { STOP_ALL.store(false, Ordering::SeqCst); }
    IN_STUB.store(false, Ordering::Release);
}
const ACT_CONT: u8 = 0;
const ACT_STEP: u8 = 1;
const ACT_DETACH: u8 = 2;
/// The vector of the parking of the other cores for the time of the conversation with gdb. The priority
/// class is 6 — above the tick's: the parking does not wait for a core to work its quantum out.
pub const PARK_VECTOR: usize = 0x6E;
static STOP_ALL: AtomicBool = AtomicBool::new(false);
static STEP_OWNER: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(usize::MAX);
/// The thread's interrupt flag has been removed for the time of the step (return it at the next entry).
static STEP_IF: [AtomicBool; NCPU] = [const { AtomicBool::new(false) }; NCPU];
/// The parking handler: stand until gdb releases the machine; on leaving — take
/// the fresh debug registers (they could have been changed while the core stood).
/// The frame of the interrupted thread of a parked core (0 — the core is not parked).
static PARK_FRAME: [AtomicU64; NCPU] = [const { AtomicU64::new(0) }; NCPU];
#[unsafe(no_mangle)]
pub extern "C" fn rust_gdb_park(fr: *mut Frame) {
    let me = crate::percpu::cpu_id();
    if me < NCPU { PARK_FRAME[me].store(crate::mem::vaddr(fr), Ordering::SeqCst); }
    // The core that steps does not park: the IPI could have gone stale since the previous stop.
    while STOP_ALL.load(Ordering::Acquire) && STEP_OWNER.load(Ordering::Acquire) != me { core::hint::spin_loop(); }
    if me < NCPU { PARK_FRAME[me].store(0, Ordering::SeqCst); }
    apply_if_changed(me);
    unsafe { crate::smp::lapic_eoi(); }
}
fn rust_gdb_inner(f: *mut Frame) -> u8 {
    let f = unsafe { &mut *f };
    // at an entry by a #BP restore the orig byte and roll the rip back (an int3 is 1 byte)
    if f.vec == 3 {
        unsafe { for i in 0..BPMAX { if BP_USED[i] && BP_ADDR[i] == f.rip.wrapping_sub(1) {
            core::ptr::write_volatile(crate::mem::vptr::<u8>(crate::mem::kimg_write_va(BP_ADDR[i])), BP_ORIG[i]); f.rip -= 1; break; } } }
    }
    if f.vec == 1 { unsafe { core::arch::asm!("mov dr6, {}", in(reg) 0u64, options(nomem, nostack)); } } // clear the DR6 (a watchpoint/a step)
    f.rflags &= !0x100; // clear the TF (after a step)
    let mut buf = [0u8; PACKET];
    // THE THREADS. A thread's number in gdb is the workload's tid (the task + 1), as in `ps` and in
    // the core dump. The thread that stopped gives out its registers from the exception's frame;
    // the others — from the context saved by the scheduler (where they were taken off
    // the core). Before, `Hg` answered OK and did nothing: `thread N` in gdb
    // showed the registers of one and the same thread under different numbers.
    let me = crate::sched::current_id() as u64 + 1;
    let mut sel = me;
    // `core:` is a standard field of the stop answer: on which core the thread stopped.
    let mut stop = [0u8; 40];
    let sn = gdbproto::stop_reply(&mut stop, me, crate::percpu::cpu_id() as u64);
    PORT.send(&stop[..sn]); // we have stopped (a SIGTRAP), and have named which thread
    loop {
        let n = PORT.recv(&mut buf);
        if n == 0 { PORT.send(b""); continue; }
        if gdbproto::common(&PORT, &buf, n, me, &mut sel) { continue; }
        match buf[0] {
            b'?' => PORT.send(&stop[..sn]),
            b'g' if sel == me => send_regs_packet(f),
            b'g' => {
                // The thread stands on a parked core — its registers are in the parking frame.
                let t = sel as usize - 1;
                let mut parked = 0u64;
                for c in 0..NCPU {
                    let pf = PARK_FRAME[c].load(Ordering::SeqCst);
                    if pf != 0 && crate::percpu::cur_task_of(c) == t as u64 { parked = pf; break; }
                }
                if parked != 0 { send_regs_packet(unsafe { &*crate::mem::vptr::<Frame>(parked) }); continue; }
                match crate::sched::saved_ctx(t) {
                    Some(c) => {
                        let tf = Frame { r15: c[0], r14: c[1], r13: c[2], r12: c[3], r11: c[4], r10: c[5], r9: c[6], r8: c[7],
                            rbp: c[8], rdi: c[9], rsi: c[10], rdx: c[11], rcx: c[12], rbx: c[13], rax: c[14],
                            vec: 0, err: 0, rip: c[15], cs: c[16], rflags: c[17], rsp: c[18], ss: c[19] };
                        send_regs_packet(&tf);
                    }
                    // There is no context (the thread has never been taken off a core): zeroes rather than
                    // an error — on an E01 gdb got stuck repeating the request.
                    None => { let z: Frame = unsafe { core::mem::zeroed() }; send_regs_packet(&z); }
                }
            }
            b'G' if sel != me => PORT.send(b"E01"),     // only the one that stopped may be written to
            b'G' => { // a write of the registers: the g-order (LE per reg) back into the frame
                // i-th 64-bit register in g-order: '1' skips the 'G', 16 hex digits each
                let reg = |i: usize| read_le_hex(&buf, 1 + i * 16, 8);
                f.rax = reg(0);  f.rbx = reg(1);
                f.rcx = reg(2);  f.rdx = reg(3);
                f.rsi = reg(4);  f.rdi = reg(5);
                f.rbp = reg(6);  f.rsp = reg(7);
                f.r8  = reg(8);  f.r9  = reg(9);
                f.r10 = reg(10); f.r11 = reg(11);
                f.r12 = reg(12); f.r13 = reg(13);
                f.r14 = reg(14); f.r15 = reg(15);
                f.rip = reg(16);
                f.rflags = read_le_hex(&buf, 1 + 17 * 16, 4);
                f.cs = read_le_hex(&buf, 1 + 17 * 16 + 8, 4);
                f.ss = read_le_hex(&buf, 1 + 17 * 16 + 16, 4);
                PORT.send(b"OK");
            }
            b'Z' if n >= 2 && buf[1] == b'0' => { // Z0,addr,kind — sw breakpoint
                let addr = parse_hex(&buf[3..n]);
                unsafe { for i in 0..BPMAX { if !BP_USED[i] {
                    BP_ADDR[i]=addr; BP_ORIG[i]=core::ptr::read_volatile(crate::mem::vptr::<u8>(addr));
                    core::ptr::write_volatile(crate::mem::vptr::<u8>(crate::mem::kimg_write_va(addr)), 0xCC); BP_USED[i]=true; break; } } }
                PORT.send(b"OK");
            }
            b'z' if n >= 2 && buf[1] == b'0' => { // z0,addr,kind
                let addr = parse_hex(&buf[3..n]);
                unsafe { for i in 0..BPMAX { if BP_USED[i] && BP_ADDR[i]==addr {
                    core::ptr::write_volatile(crate::mem::vptr::<u8>(crate::mem::kimg_write_va(addr)), BP_ORIG[i]); BP_USED[i]=false; break; } } }
                PORT.send(b"OK");
            }
            b'Z' if n >= 2 && (buf[1] == b'2' || buf[1] == b'3' || buf[1] == b'4') => { // hw-watchpoint
                let addr = parse_hex(&buf[3..n]);
                let rw = if buf[1] == b'2' { 0b01 } else { 0b11 }; // write / access (read->access)
                unsafe { set_watchpoint(addr, rw); }
                PORT.send(b"OK");
            }
            b'z' if n >= 2 && (buf[1] == b'2' || buf[1] == b'3' || buf[1] == b'4') => {
                unsafe { clear_watchpoint(); }
                PORT.send(b"OK");
            }
            b'Z' if n >= 2 && buf[1] == b'1' => { // a hw breakpoint on execution (hbreak)
                let addr = parse_hex(&buf[3..n]);
                if unsafe { set_hw_exec(addr) } { PORT.send(b"OK"); } else { PORT.send(b"E0e"); } // there are no slots
            }
            b'z' if n >= 2 && buf[1] == b'1' => { let addr = parse_hex(&buf[3..n]); unsafe { clear_hw_exec(addr); } PORT.send(b"OK"); }
            // Continuing after a HARDWARE breakpoint on execution: without the RF the processor
            // will stop on the same instruction again without having executed it — and gdb
            // will get stuck on `continue` forever.
            b'c' => { if f.vec == 1 { f.rflags |= 1 << 16; } return ACT_CONT; }
            // Detach: remove all the breakpoints and release the machine. Without an answer to a `D`
            // gdb printed "Can't detach" and kept the kernel stopped.
            b'D' => {
                unsafe {
                    for i in 0..BPMAX { if BP_USED[i] {
                        core::ptr::write_volatile(crate::mem::vptr::<u8>(crate::mem::kimg_write_va(BP_ADDR[i])), BP_ORIG[i]); BP_USED[i] = false; } }
                    clear_watchpoint(); clear_hw_exec_all();
                }
                if f.vec == 1 { f.rflags |= 1 << 16; }
                PORT.send(b"OK");
                return ACT_DETACH;
            }
            b's' => { f.rflags |= 0x100; return ACT_STEP; }// single-step (TF)
            _ => PORT.send(b""),
        }
    }
}

/// Switch the gdb stub on: redirect the #DB(1) and the #BP(3) onto the gdb handler.
pub fn enable() {
    init_com2();
    unsafe extern "C" { fn gdb_isr_1(); fn gdb_isr_3(); }
    crate::idt::set_vector(1, crate::mem::vaddr(gdb_isr_1 as *const ()));
    crate::idt::set_vector(3, crate::mem::vaddr(gdb_isr_3 as *const ()));
    unsafe extern "C" { fn gdb_park_stub(); }
    crate::idt::set_vector(PARK_VECTOR, crate::mem::vaddr(gdb_park_stub as *const ()));
}

/// Whether the stub talks over the console's line (arm64's does; this one has COM2).
pub fn on_console() -> bool { false }
/// gdb's ^C on the console's line (arm64); here COM2 is the stub's own.
pub fn interrupt() { breakpoint() }

/// A breakpoint for entering the debugger (an int3).
#[inline(always)]
pub fn breakpoint() { unsafe { core::arch::asm!("int3", options(nomem, nostack)); } }

core::arch::global_asm!(r#"
.section .text
.global gdb_park_stub
gdb_park_stub:
    /* the parking of a core for the time of the conversation with gdb: a full frame, as in gdb_common,
       so that the debugger sees the registers of the interrupted thread (vec/err are stubs) */
    pushq $0
    pushq $0x6e
    push %rax
    push %rbx
    push %rcx
    push %rdx
    push %rsi
    push %rdi
    push %rbp
    push %r8
    push %r9
    push %r10
    push %r11
    push %r12
    push %r13
    push %r14
    push %r15
    mov %rsp, %rdi
    mov %rsp, %rbp
    and $-16, %rsp
    call rust_gdb_park
    mov %rbp, %rsp
    pop %r15
    pop %r14
    pop %r13
    pop %r12
    pop %r11
    pop %r10
    pop %r9
    pop %r8
    pop %rbp
    pop %rdi
    pop %rsi
    pop %rdx
    pop %rcx
    pop %rbx
    pop %rax
    add $16, %rsp
    iretq
.global gdb_isr_1
.global gdb_isr_3
gdb_isr_1:
    pushq $0
    pushq $1
    jmp gdb_common
gdb_isr_3:
    pushq $0
    pushq $3
    jmp gdb_common
gdb_common:
    push %rax
    push %rbx
    push %rcx
    push %rdx
    push %rsi
    push %rdi
    push %rbp
    push %r8
    push %r9
    push %r10
    push %r11
    push %r12
    push %r13
    push %r14
    push %r15
    mov %rsp, %rdi
    mov %rsp, %rbp
    and $-16, %rsp
    call rust_gdb
    mov %rbp, %rsp
    pop %r15
    pop %r14
    pop %r13
    pop %r12
    pop %r11
    pop %r10
    pop %r9
    pop %r8
    pop %rbp
    pop %rdi
    pop %rsi
    pop %rdx
    pop %rcx
    pop %rbx
    pop %rax
    add $16, %rsp
    iretq
"#, options(att_syntax));
