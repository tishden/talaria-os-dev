//! An IDT with the handlers of the CPU's exceptions: instead of a triple fault — diagnostics into COM1.
//! The stubs are in global_asm (stable Rust), the common handler is in Rust.
use core::arch::global_asm;
#[allow(unused_imports)]
use crate::serial;

#[repr(C)]
#[derive(Clone, Copy)]
struct IdtEntry { off_lo: u16, sel: u16, ist: u8, attr: u8, off_mid: u16, off_hi: u32, zero: u32 }
impl IdtEntry {
    const fn missing() -> Self { IdtEntry { off_lo:0, sel:0, ist:0, attr:0, off_mid:0, off_hi:0, zero:0 } }
    fn set(&mut self, handler: u64) {
        self.off_lo = handler as u16;
        self.sel = 0x08; // 64-bit code segment (the GDT from boot.rs)
        self.ist = 0;
        self.attr = 0x8E; // present, DPL0, interrupt gate
        self.off_mid = (handler >> 16) as u16;
        self.off_hi = (handler >> 32) as u32;
        self.zero = 0;
    }
}

static mut IDT: [IdtEntry; 256] = [IdtEntry::missing(); 256];
pub static IPI_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub const IPI_VECTOR: usize = 0x60;
pub static TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub const TIMER_VECTOR: usize = 0x41;
// THE PRIORITY CLASSES OF THE VECTORS. In the LAPIC the priority is the vector's upper nibble:
// while an interrupt of class N is being serviced, the interrupts of THE SAME class are not
// delivered (the PPR). Class 4 used to hold everything at once — the scheduler's tick, the
// wake-up of a thread, a TLB flush, IPIs, the devices' interrupts — and the tick (two thousand
// times a second on every core) occupied the class on a par with them.
//
// Now the tick has been left alone in class 4, and everything urgent has been raised into class 6. That way
// the most frequent and the least urgent interrupt in the system stops standing in the
// queue in front of the rarest and the most urgent one.
//
// Separately this matters with the KVM PV EOI (`smp::lapic_eoi`): there the acknowledgement is
// deferred — the ISR bit goes out not by a write into the LAPIC but at the nearest exit to
// the host, and the window in which the class is occupied stretches from a hundred nanoseconds to an
// arbitrary time. Separated classes make that window harmless.
/// The wake-up of a sleeping core (sync/futex, PLAN-SYNC): the handler is only an EOI.
pub const WAKE_VECTOR: usize = 0x63;
/// A TLB flush on a foreign core (a shootdown): the handler's only work is to reload
/// the CR3. It is needed when the kernel REMOVES or TIGHTENS a mapping (munmap, mprotect,
/// MADV_DONTNEED): without it another core keeps writing by a stale TLB entry —
/// into a frame already given to another. It was caught as a random corruption of a multithreaded JVM's heap.
pub const TLB_VECTOR: usize = 0x62;
/// The network card's line through the I/O APIC (`talaria.netirq=1`). The priority class is 6 —
/// the same as the IPI/TLB/WAKE ones, and above the tick's: a frame matters more than a rotation.
pub const NIC_VECTOR: usize = 0x61;
/// The first MSI/MSI-X vector of the devices (then in a row): the same urgency class
/// as the wake-up's and the TLB flush's.
pub const MSI_VECTOR_BASE: usize = 0x64;

#[repr(C, packed)]
struct Idtr { limit: u16, base: u64 }

// the addresses of the stubs: isr_stub_0 .. isr_stub_31, a step of 16 bytes (see global_asm)
unsafe extern "C" { static isr_stub_table: [u64; 32]; }
unsafe extern "C" { static ipi_stub_ptr: u64; }
unsafe extern "C" { static timer_stub_ptr: u64; }
unsafe extern "C" { static wake_stub_ptr: u64; }
unsafe extern "C" { static tlb_stub_ptr: u64; }
unsafe extern "C" { static nic_stub_ptr: u64; }

pub fn init() {
    unsafe {
        let table = &isr_stub_table;
        for v in 0..32 {
            IDT[v].set(table[v]);
        }
        IDT[IPI_VECTOR].set(ipi_stub_ptr);
        // The scheduler now spins not only on the BSP, and an AP brought up AFTER
        // `sched::install` would with its own `idt::init` have returned the timer vector to the bare
        // tick counter — there would be no pre-emption on that core, silently. The IDT
        // table is one for all, so we respect an installed scheduler.
        let sch = SCHED_TIMER.load(core::sync::atomic::Ordering::SeqCst);
        IDT[TIMER_VECTOR].set(if sch != 0 { sch } else { timer_stub_ptr });
        IDT[WAKE_VECTOR].set(wake_stub_ptr);
        IDT[TLB_VECTOR].set(tlb_stub_ptr);
        IDT[NIC_VECTOR].set(nic_stub_ptr);
        IDT[0xFF].set(ipi_stub_ptr); // spurious
        // The MSI-X vectors of the devices (ENA, §4.4): the handler is the same IPI stub
        // (a counter + an EOI). The datapath polls the rings, the interrupts are needed only so as
        // not to catch a #GP if Nitro does deliver a message after all.
        for v in MSI_VECTOR_BASE..MSI_VECTOR_BASE + 8 { IDT[v].set(ipi_stub_ptr); }
        let idtr = Idtr { limit: (core::mem::size_of::<[IdtEntry;256]>() - 1) as u16,
                          base: core::ptr::addr_of!(IDT) as u64 };
        core::arch::asm!("lidt [{}]", in(reg) &idtr, options(readonly, nostack, preserves_flags));
    }
}

/// The handler of the timer vector installed by the scheduler (0 — there is no scheduler
/// yet). It is needed so that `idt::init` on a late-brought-up core does not overwrite it.
pub static SCHED_TIMER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The handler of the network card's line (`nic_stub` -> `netsys::nic_irq`) — so as to
/// put it on the MSI-X vector too, not only on `NIC_VECTOR`.
pub fn nic_handler() -> u64 { unsafe { nic_stub_ptr } }

pub fn set_vector(vec: usize, handler: u64) {
    if vec == TIMER_VECTOR { SCHED_TIMER.store(handler, core::sync::atomic::Ordering::SeqCst); }
    unsafe { IDT[vec].set(handler); }
}

/// The handler of the LAPIC timer (the vector 0x41): a tick + an EOI.
#[unsafe(no_mangle)]
extern "C" fn rust_timer() {
    TICKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    unsafe { crate::smp::lapic_eoi(); }
}

/// The handler of an IPI (the vector `IPI_VECTOR`): an increment of a counter + an EOI into the LAPIC.
#[unsafe(no_mangle)]
extern "C" fn rust_ipi() {
    IPI_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    crate::pmu::ipi_check(); // `perf poke <core>`: take the counters off THERE, where alone they can be taken off
    unsafe { crate::smp::lapic_eoi(); } // EOI
}

/// The full frame of an exception (the order = the pushes in isr_common + the hardware frame).
#[repr(C)]
pub struct ExcFrame {
    pub r15: u64, pub r14: u64, pub r13: u64, pub r12: u64, pub r11: u64, pub r10: u64, pub r9: u64, pub r8: u64,
    pub rbp: u64, pub rdi: u64, pub rsi: u64, pub rdx: u64, pub rcx: u64, pub rbx: u64, pub rax: u64,
    pub vec: u64, pub err: u64, pub rip: u64, pub cs: u64, pub rflags: u64, pub rsp: u64, pub ss: u64,
}

/// The call stack into the log: the addresses from the point of the fault to the callers.
///
/// The symbols are resolved on the HOST (`addr2line -f -C -e <binary> <address>`) — dragging
/// a symbol table into the kernel for the sake of a line printed at a crash is pointless, and
/// for the workload the kernel does not have one at all.
fn backtrace(what: &str, rip: u64, rbp: u64, user: bool) {
    let mut stk = [0u64; crate::obs::unwind::MAXDEPTH];
    let n = crate::obs::unwind::walk(rip, rbp, user, &mut stk);
    crate::print!("║ the stack {} ({} frame(s)):", what, n);
    for i in 0..n { crate::print!(" {:#x}", stk[i]); }
    crate::println!("");
    if n == 1 {
        crate::println!("║   the depth is 1 — the code is built without frame pointers (see build-prof.sh / -fno-omit-frame-pointer)");
    }
    crate::println!("║   the symbols: addr2line -f -C -e <binary> <address>");
}

/// The common Rust handler of an exception: a full crash dump and a halt.
/// Return into the interrupted context out of an exception (the `isr_common` frame + the FPU).
unsafe fn exc_return(fp: *mut ExcFrame, fpu: u64) -> ! {
    core::arch::asm!(
        "fxrstor [{fpu}]",
        "mov rsp, {fp}",
        "pop r15", "pop r14", "pop r13", "pop r12", "pop r11", "pop r10", "pop r9", "pop r8",
        "pop rbp", "pop rdi", "pop rsi", "pop rdx", "pop rcx", "pop rbx", "pop rax",
        "add rsp, 16",   // skip vec, err
        "iretq",
        fp = in(reg) fp as u64, fpu = in(reg) fpu, options(noreturn)
    );
}
#[unsafe(no_mangle)]
extern "C" fn rust_exception(fp: *mut ExcFrame, fpu: u64) -> ! {
    let f: &ExcFrame = unsafe { &*fp };
    // The COW fast path: a write #PF (err bit 1) into a present page with a COW mark -> a copy
    // and a return to retry the instruction (an iretq from the saved frame). Not a crash.
    if f.vec == 14 { crate::metrics::inc_page_fault(); }
    // THE UNWINDING OF THE STACK BROKE DOWN — that is not a crash. The profiler reads the chain of
    // frames, and the RBP of code without frame pointers contains anything at all; the page
    // could moreover have gone away from under us on a neighbouring core. We return control
    // right after the load that broke down: diagnostics is not entitled to bring the kernel down.
    // An NMI is the question "where are you?" from `sched::dump_cores` to a core that does not take
    // interrupts: the answer is put into a snapshot, the core continues from the same place.
    if f.vec == 2 {
        crate::sched::nmi_note(f.rip, f.cs, f.rflags, f.rsp);
        unsafe { exc_return(fp, fpu); }
    }
    if f.vec == 14 && f.cs & 3 == 0 {
        if let Some(fix) = crate::obs::unwind::fault_fixup() {
            unsafe {
                (*fp).rip = fix;
                exc_return(fp, fpu);
            }
        }
    }
    if f.vec == 14 && f.err & 2 != 0 {
        let cr2: u64;
        unsafe { core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack)); }
        if crate::paging::cow_fault(cr2) {
            crate::metrics::inc_cow_fault();
            unsafe {
                core::arch::asm!(
                    "fxrstor [{fpu}]",
                    "mov rsp, {fp}",
                    "pop r15", "pop r14", "pop r13", "pop r12", "pop r11", "pop r10", "pop r9", "pop r8",
                    "pop rbp", "pop rdi", "pop rsi", "pop rdx", "pop rcx", "pop rbx", "pop rax",
                    "add rsp, 16",   // skip vec, err
                    "iretq",
                    fp = in(reg) f as *const ExcFrame as u64, fpu = in(reg) fpu, options(noreturn)
                );
            }
        }
    }
    // DEMAND-ZERO: an access to a reserved (LAZY) page is not an error but the
    // first touch: we allocate a frame, zero it, set the rights and retry the instruction.
    // It is suitable for ring 0 too (the kernel writes into a user buffer that the workload has not touched yet).
    if f.vec == 14 && f.err & 1 == 0 {
        let cr2: u64;
        unsafe { core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack)); }
        if crate::paging::lazy_fault(cr2) {
            unsafe {
                core::arch::asm!(
                    "fxrstor [{fpu}]",
                    "mov rsp, {fp}",
                    "pop r15", "pop r14", "pop r13", "pop r12", "pop r11", "pop r10", "pop r9", "pop r8",
                    "pop rbp", "pop rdi", "pop rsi", "pop rdx", "pop rcx", "pop rbx", "pop rax",
                    "add rsp, 16",
                    "iretq",
                    fp = in(reg) fp as u64, fpu = in(reg) fpu, options(noreturn)
                );
            }
        }
    }
    // A SYNCHRONOUS SIGNAL TO THE WORKLOAD. An exception in ring 3 is not always "it crashed":
    // the JVM catches a SIGSEGV on EVERY implicit null check, on a safepoint poll and on a
    // stack depth check and carries on working, having fixed the context. While the kernel
    // printed a dump on any fault, such a workload could not even start.
    // The handler gets a siginfo (si_addr = CR2) and a ucontext (gregs/err/trapno/cr2,
    // the FPU) — the same as in Linux; the return is an ordinary rt_sigreturn.
    if f.cs & 3 == 3 {
        let cr2: u64;
        unsafe { core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack)); }
        let sig = match f.vec {
            11..=14 => 11,          // #PF/#GP/#SS/#NP -> SIGSEGV
            6 => 4,                           // #UD -> SIGILL
            0 | 16 | 19 => 8,                 // #DE/#MF/#XM -> SIGFPE
            17 => 7,                          // #AC -> SIGBUS
            3 | 1 => 5,                       // #BP/#DB -> SIGTRAP
            _ => 0,
        };
        if sig != 0 {
            if crate::syscall::deliver_exception_signal(unsafe { &mut *fp }, sig, cr2, fpu) {
                unsafe {
                    core::arch::asm!(
                        "fxrstor [{fpu}]",
                        "mov rsp, {fp}",
                        "pop r15", "pop r14", "pop r13", "pop r12", "pop r11", "pop r10", "pop r9", "pop r8",
                        "pop rbp", "pop rdi", "pop rsi", "pop rdx", "pop rcx", "pop rbx", "pop rax",
                        "add rsp, 16",   // skip vec, err
                        "iretq",
                        fp = in(reg) fp as u64, fpu = in(reg) fpu, options(noreturn)
                    );
                }
            }
        }
    }
    crate::fr::record(crate::fr::FAULT, f.rip);
    // The recovery after a ring 3 fault: the workload has crashed, but the kernel is intact — return
    // control to the supervisor (a longjmp), which will restart the workload.
    use core::sync::atomic::Ordering;
    // A multithreaded workload in ITS OWN address space (`mode=cell` ever since
    // the cell got its own AS) does NOT jump into the supervisor from here.
    // The jump carries onto the supervisor's stack the core on which the thread crashed — and
    // it can crash on any AP. Then the supervisor announced "the workload has crashed,
    // a halt" on core 2, while the sisters of the crashed thread lived on on the other cores
    // (`examples/mdpipe`: the main thread kept printing a report for another 12 seconds), and
    // there was no core dump at all — the writing of the dump stands in the branch below. Such a crash
    // goes the process path: the stack and the dump, all the process's threads are dead, and
    // control will be returned to the supervisor by the BSP when it sees "all the tasks have finished".
    let own_as_threads = f.cs & 3 == 3
        && crate::syscall::THREADED.load(Ordering::SeqCst)
        && crate::sched::task_cr3(crate::sched::current_id()) != 0;
    if crate::supervise::RECOVERY_ARMED.load(Ordering::SeqCst) && f.cs & 3 == 3 && !own_as_threads {
        crate::supervise::CRASH_RIP.store(f.rip, Ordering::SeqCst);
        crate::supervise::CRASH_VEC.store(f.vec, Ordering::SeqCst);
        crate::supervise::RECOVERY_ARMED.store(false, Ordering::SeqCst);
        unsafe { crate::supervise::tal_longjmp(core::ptr::addr_of_mut!(crate::supervise::RECOVERY), 1); }
    }
    // There is nowhere to deliver the signal AND the supervisor does not catch this error. The kernel used to
    // print a dump and HALT; a workload's error in ring 3 must not cost
    // the whole machine — we kill the process, as Linux would have done. The modes without a
    // process model (the kernel's self-tests) do not get here: there THREADED=false.
    if f.cs & 3 == 3
        && crate::syscall::THREADED.load(Ordering::SeqCst)
        && crate::sched::task_cr3(crate::sched::current_id()) != 0
    {
        let cr2v: u64;
        unsafe { core::arch::asm!("mov {}, cr2", out(reg) cr2v, options(nomem, nostack)); }
        // THE FS BASE GOES IN THE SAME LINE. In any workload on musl an access to
        // `errno`, to the TLS and to `__pthread_self()` goes through `%fs`, so
        // HALF the crashes in ring 3 are "FS points to the wrong place", and telling them
        // apart from an honest application error by the rip and the cr2 is impossible. On the stand
        // on 8 September a PostgreSQL backend crashed on a write to the address 0x131: that is
        // `__errno_location()`, that is `&__pthread_self()->errno_val`, and
        // a small value meant exactly one thing — `%fs:0` had read as garbage.
        // To see that, someone else's binary had to be disassembled; one
        // number in the crash line answers the same question at once.
        let fsb = unsafe { crate::syscall::rdmsr(0xC000_0100) };
        // READ ONLY THROUGH THE TABLES. Dereferencing `fs:0` directly is impossible: if
        // FS points into unmapped memory (and that is exactly what we suspect), the result is
        // AN ERROR INSIDE THE ERROR HANDLER, that is a double one and the machine's death
        // instead of a diagnosis. We check the mapping in the workload's AS and read by the
        // physical address (identity), or honestly print "it is not mapped".
        let cr3 = crate::sched::task_cr3(crate::sched::current_id()) & 0x000f_ffff_ffff_f000;
        let tls0 = if fsb != 0 && fsb < 0x0000_8000_0000_0000 && cr3 != 0 {
            crate::paging::phys_of_in(cr3, fsb).map(|ph| unsafe { core::ptr::read_volatile((ph + (fsb & 0xFFF)) as *const u64) })
        } else { None };
        // The TLS self-pointer: in a healthy thread on musl `[fs:0] == fs`.
        crate::println!("\n[fault] ring 3: the exception #{} err={:#x} rip={:#x} cr2={:#x} rsp={:#x} fs={:#x} [fs:0]={:#x}{}",
            f.vec, f.err, f.rip, cr2v, f.rsp, fsb,
            tls0.unwrap_or(0),
            match tls0 {
                None if fsb != 0 => " (fs IS NOT MAPPED) <-- THE TLS IS BROKEN",
                Some(v) if v != fsb => " <-- THE TLS IS BROKEN: the self-pointer did not match fs",
                _ => "",
            });
        let sig = match f.vec { 6 => 4, 0 | 16 | 19 => 8, 17 => 7, _ => 11 };
        // The crashed thread's stack goes INTO THE LOG, always. A core dump requires an FS and the knob
        // switched on, while the stack is needed on a machine where there is neither.
        backtrace("workload", f.rip, f.rbp, true);
        // FIRST STOP THE SISTERS, THEN WRITE THE DUMP — as Linux does. The dump
        // is written into the FS, and while it was being written, the sisters kept working (mdpipe
        // managed to process 20 000 ticks), and the registers of a running thread in the dump
        // were garbage. The threads are remembered while alive, killed, finish executing
        // what they started in the kernel (releasing the ext2 journal) and stop; the dump takes their
        // registers at the point of the stop.
        let armed = crate::supervise::RECOVERY_ARMED.load(Ordering::SeqCst);
        if armed { crate::supervise::crashed(f.rip, f.vec, sig); }
        crate::coredump::snapshot_threads();
        crate::sched::exit_all(128 + sig as i32);
        crate::sched::drain_proc(1000);
        crate::coredump::dump_process(f, sig as u32);
        if armed {
            // Control will be returned to the supervisor by the BSP when it sees that all the tasks are stopped;
            // the process cleanup it will do itself (a reset).
            crate::sched::exit_current_code(128 + sig as i32); // an AP goes into the pickup loop
        }
        crate::syscall::kill_current_process(sig);
    }
    let names = [
        "#DE div","#DB","NMI","#BP","#OF","#BR","#UD invalid-op","#NM",
        "#DF double-fault","copro","#TS","#NP","#SS","#GP general-prot","#PF page-fault","-",
        "#MF","#AC","#MC","#XM","#VE","#CP",
    ];
    let name = if (f.vec as usize) < names.len() { names[f.vec as usize] } else { "?" };
    crate::println!("\n╔═══ CRASH DUMP: THE EXCEPTION #{} {} err={:#x} ═══", f.vec, name, f.err);
    crate::println!("║ rip={:#018x} cs={:#x} rflags={:#x} rsp={:#018x} ss={:#x}",
        f.rip, f.cs, f.rflags, f.rsp, f.ss);
    let ring = if f.cs & 3 == 3 { "ring 3 (user)" } else { "ring 0 (kernel)" };
    crate::println!("║ the mode: {}", ring);
    {   // where the task came from: the last syscall (the number and the return point in user)
        let t = crate::sched::current_id();
        let v = crate::syscall::LAST_SYS[t].load(core::sync::atomic::Ordering::Relaxed);
        if v != u64::MAX { crate::println!("║ the task's last syscall: #{} (the return into {:#x})", v >> 48, v & 0xFFFF_FFFF_FFFF); }
    }
    crate::println!("║ the task t{} (DEAD={}) CR3={:#x} cr3reg={:#x}", crate::sched::current_id(), !crate::sched::task_alive(crate::sched::current_id()),
        crate::sched::task_cr3(crate::sched::current_id()), crate::paging::current_cr3());
    crate::println!("║ rax={:#018x} rbx={:#018x} rcx={:#018x} rdx={:#018x}", f.rax, f.rbx, f.rcx, f.rdx);
    crate::println!("║ rsi={:#018x} rdi={:#018x} rbp={:#018x} r8 ={:#018x}", f.rsi, f.rdi, f.rbp, f.r8);
    crate::println!("║ r9 ={:#018x} r10={:#018x} r11={:#018x} r12={:#018x}", f.r9, f.r10, f.r11, f.r12);
    crate::println!("║ r13={:#018x} r14={:#018x} r15={:#018x}", f.r13, f.r14, f.r15);
    if f.vec == 14 {
        let cr2: u64;
        unsafe { core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack)); }
        let d = if f.err & 1 == 0 { "not mapped" } else { "protection" };
        let rw = if f.err & 2 != 0 { "a write" } else { "a read" };
        crate::println!("║ #PF CR2={:#018x} ({}, {})", cr2, d, rw);
    }
    // a hex dump of the stack (up to 8 qwords from the rsp), if the address looks valid
    if f.rsp >= 0x1000 && f.rsp < 0x4000_0000_0000 {
        crate::println!("║ the stack @ {:#x}:", f.rsp);
        for i in 0..8u64 {
            let a = f.rsp + i * 8;
            let v = unsafe { core::ptr::read_volatile(a as *const u64) };
            crate::println!("║   [{:#014x}] = {:#018x}", a, v);
        }
    }
    // THE CALL STACK. The registers answer "where it crashed", the stack — "how we got here", and
    // without the second the first is usually useless. The unwinding is the same as the
    // profiler's (`obs::unwind`), and the same caveat applies: there will be names in the chain in
    // a build with frame pointers (`kernel/build-prof.sh`), without it the
    // top frame is visible. We print it in any case: the address is resolved on the host.
    backtrace(if f.cs & 3 == 3 { "workload" } else { "kernel" }, f.rip, f.rbp, f.cs & 3 == 3);
    crate::fr::dump(); // the post-mortem: what led to the failure
    // The kernel's core dump — if the operator asked and the FS is alive. It goes AFTER the text:
    // the text always arrives over the serial line, while a write to the disk at the moment of a fault may well
    // not succeed, and the order decides what will be left.
    if f.cs & 3 == 0 { crate::coredump::dump_kernel(f, 11); }
    crate::println!("╚═══ the kernel has been halted ═══");
    // A10: a crash dump is bound to REACH the disk and not eat the rest of the set of modes.
    // `park` flushes the trace and, if a set is running, reboots the machine for the
    // next item; without a set it behaves like the former `loop { hlt }`.
    // But only the BSP is entitled to flush the trace (and all the more to reboot the machine):
    // the NVMe queue is one, and a write from an AP racing with the BSP would spoil the very log
    // for whose sake all this is done.
    if crate::smp::my_logical() == 0 { crate::suite::park(); }
    loop { unsafe { core::arch::asm!("hlt"); } }
}

// The stubs: each one puts (for the vectors without an err) a fictitious 0, the vector's number,
// and jumps into the common one. The common one reads the vec/err/rip off the stack and calls rust_exception.
global_asm!(r#"
.section .text
.macro ISR_NOERR n
isr_stub_\n:
    pushq $0
    pushq $\n
    jmp isr_common
.endm
.macro ISR_ERR n
isr_stub_\n:
    pushq $\n
    jmp isr_common
.endm

/* 0..31: with an error code — 8,10,11,12,13,14,17,21,29,30 */
ISR_NOERR 0
ISR_NOERR 1
ISR_NOERR 2
ISR_NOERR 3
ISR_NOERR 4
ISR_NOERR 5
ISR_NOERR 6
ISR_NOERR 7
ISR_ERR   8
ISR_NOERR 9
ISR_ERR   10
ISR_ERR   11
ISR_ERR   12
ISR_ERR   13
ISR_ERR   14
ISR_NOERR 15
ISR_NOERR 16
ISR_ERR   17
ISR_NOERR 18
ISR_NOERR 19
ISR_NOERR 20
ISR_ERR   21
ISR_NOERR 22
ISR_NOERR 23
ISR_NOERR 24
ISR_NOERR 25
ISR_NOERR 26
ISR_NOERR 27
ISR_NOERR 28
ISR_ERR   29
ISR_ERR   30
ISR_NOERR 31

.global timer_stub
timer_stub:
    push %rax
    push %rcx
    push %rdx
    push %rsi
    push %rdi
    push %r8
    push %r9
    push %r10
    push %r11
    push %rbp
    mov %rsp, %rbp
    sub $512, %rsp
    and $-16, %rsp
    fxsave (%rsp)         /* the SSE/x87 state of the interrupted code (the kernel is built with SSE) */
    call rust_timer
    fxrstor (%rsp)
    mov %rbp, %rsp
    pop %rbp
    pop %r11
    pop %r10
    pop %r9
    pop %r8
    pop %rdi
    pop %rsi
    pop %rdx
    pop %rcx
    pop %rax
    iretq

.global ipi_stub
ipi_stub:
    push %rax
    push %rcx
    push %rdx
    push %rsi
    push %rdi
    push %r8
    push %r9
    push %r10
    push %r11
    push %rbp
    mov %rsp, %rbp
    sub $512, %rsp
    and $-16, %rsp        /* align the stack to 16 bytes for a call */
    fxsave (%rsp)
    call rust_ipi
    fxrstor (%rsp)
    mov %rbp, %rsp
    pop %rbp
    pop %r11
    pop %r10
    pop %r9
    pop %r8
    pop %rdi
    pop %rsi
    pop %rdx
    pop %rcx
    pop %rax
    iretq

isr_common:
    /* the stack: [vec][err][rip][cs][rflags][rsp][ss]. We save ALL the GP registers below,
       then pass a pointer to the full frame into rust_exception (the crash dump). */
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
    mov %rsp, %rdi        /* -> ExcFrame (r15 is at [rsp]) */
    sub $512, %rsp
    and $-16, %rsp
    fxsave (%rsp)         /* the FPU/SSE of the interrupted code — the COW fast path will return by an iretq */
    mov %rsp, %rsi        /* the 2nd arg: the fxsave area (for the restore before the iretq) */
    call rust_exception
1:  hlt
    jmp 1b

.global tlb_stub
tlb_stub:
    /* the shootdown: carry out the neighbours' requests for a TLB flush (tlb_service decides itself
       what exactly is awaited, and reports) + an EOI */
    push %rax
    push %rcx
    push %rdx
    push %rsi
    push %rdi
    push %r8
    push %r9
    push %r10
    push %r11
    push %rbp
    mov %rsp, %rbp
    sub $512, %rsp
    and $-16, %rsp        /* align the stack to 16 bytes for a call */
    fxsave (%rsp)
    call tlb_service
    call lapic_eoi
    fxrstor (%rsp)
    mov %rbp, %rsp
    pop %rbp
    pop %r11
    pop %r10
    pop %r9
    pop %r8
    pop %rdi
    pop %rsi
    pop %rdx
    pop %rcx
    pop %rax
    iretq

.global nic_stub
nic_stub:
    /* the network card's line: drive the stack and report to the LAPIC. The body is the same as
       tlb_stub's: the handler calls Rust, so the SSE registers are bound to be
       saved (otherwise the interrupted code would get spoilt xmm). */
    push %rax
    push %rcx
    push %rdx
    push %rsi
    push %rdi
    push %r8
    push %r9
    push %r10
    push %r11
    push %rbp
    mov %rsp, %rbp
    sub $512, %rsp
    and $-16, %rsp
    fxsave (%rsp)
    call nic_irq
    call lapic_eoi
    fxrstor (%rsp)
    mov %rbp, %rsp
    pop %rbp
    pop %r11
    pop %r10
    pop %r9
    pop %r8
    pop %rdi
    pop %rsi
    pop %rdx
    pop %rcx
    pop %rax
    iretq

.global wake_stub
wake_stub:
    /* the wake-up out of a hlt/mwait: the work is done by the very fact of the interrupt — an EOI +
       an ack for the asymmetric membarrier (the iret serialises: the store buffer is drained) */
    push %rax
    push %rcx
    push %rdx
    lock incq WAKE_ACKS(%rip)
    call lapic_eoi
    pop %rdx
    pop %rcx
    pop %rax
    iretq

.section .rodata
.global ipi_stub_ptr
ipi_stub_ptr:
    .quad ipi_stub
.global wake_stub_ptr
wake_stub_ptr:
    .quad wake_stub
.global tlb_stub_ptr
tlb_stub_ptr:
    .quad tlb_stub
.global nic_stub_ptr
nic_stub_ptr:
    .quad nic_stub
.global timer_stub_ptr
timer_stub_ptr:
    .quad timer_stub
.global isr_stub_table
isr_stub_table:
    .quad isr_stub_0, isr_stub_1, isr_stub_2, isr_stub_3
    .quad isr_stub_4, isr_stub_5, isr_stub_6, isr_stub_7
    .quad isr_stub_8, isr_stub_9, isr_stub_10, isr_stub_11
    .quad isr_stub_12, isr_stub_13, isr_stub_14, isr_stub_15
    .quad isr_stub_16, isr_stub_17, isr_stub_18, isr_stub_19
    .quad isr_stub_20, isr_stub_21, isr_stub_22, isr_stub_23
    .quad isr_stub_24, isr_stub_25, isr_stub_26, isr_stub_27
    .quad isr_stub_28, isr_stub_29, isr_stub_30, isr_stub_31
"#, options(att_syntax));
