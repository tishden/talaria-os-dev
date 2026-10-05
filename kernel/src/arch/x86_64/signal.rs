//! The x86-64 signal frame: the Linux `ucontext` layout, the `gregs` order, the registers a
//! fork child and a core dump take from a syscall's frame, and the `rt_sigreturn` restore.
//!
//! Everything here is a LAYOUT or a register name; the policy — which signal, SIG_DFL and
//! SIG_IGN, the mask arithmetic, the alternate stack, where the frame lands — stays in
//! `syscall.rs` and is the same on every architecture (`docs/ARM64-PLAN.md` §4.4/§9.1.4).
//! On arm64 the same names hold the AAPCS64 `sigcontext` (`regs[31]`, `sp`, `pc`, `pstate`,
//! the FP/SIMD context under a magic header) and the `svc` restart of `pc - 4` / `x8`.

use super::idt::ExcFrame;
use super::syscall_entry::Frame;

/// The frame on the user stack: a siginfo(128) + a ucontext(936) + AN XSAVE AREA (1024 @ +1088,
/// aligned to 64) + headroom. The area is exactly XSAVE rather than fxsave: the workload has AVX
/// enabled, and a signal handler is free to clobber YMM entirely. While only the low 128 bits lay
/// in the frame, the JVM's JIT code returned from a handler with FOREIGN upper halves of the
/// registers — a silent corruption of data that surfaced as a crash of the garbage collector
/// during the walk over the roots.
pub const SIGFRAME: u64 = 2560;
/// The frame's base is aligned to this (XSAVE wants 64).
pub const SIGFRAME_ALIGN: u64 = 64;
/// The offset of the XSAVE area in the frame (the base is a multiple of 64).
pub const SIGFPU_OFF: u64 = 1088;
/// The siginfo's size: the ucontext follows it.
pub const SIGINFO_SIZE: u64 = 128;
/// The handler returns through a word BELOW the frame (the restorer's address, `ret`
/// pops it): the frame's base sits `RET_SLOT` above the handler's sp. arm64 has no slot —
/// the return address travels in x30.
pub const RET_SLOT: u64 = 8;
/// `ucontext`: `uc_stack` (ss_sp/ss_flags/ss_size) and `uc_sigmask`.
pub const UC_STACK: u64 = 16;
pub const UC_SIGMASK: u64 = 296;
/// `uc_mcontext.gregs` and the `fpregs` pointer.
const UC_GREGS: u64 = 40;
const UC_FPREGS: u64 = 224;
/// The slot of the mask INSIDE gregs (`REG_OLDMASK`), written alongside `uc_sigmask`.
const REG_OLDMASK: usize = 21;

/// The fxsave area of the current syscall (the stub puts it under the frame: (rbx-512)&~15).
pub fn fpu_area(f: &Frame) -> u64 { ((f as *const Frame as u64) - 512) & !0xF }

/// The fork child's registers in the order [rax,rbx,rcx,rdx,rsi,rdi,rbp,r8..r15]: a copy of the
/// parent's at the entry into the syscall (rax=0 — the fork's return in the child; rcx/r11 =
/// rip/rflags as with a sysret).
pub fn fork_regs(f: &Frame) -> [u64; 15] {
    [0, f.rbx, f.pc, f.a3, f.a2, f.a1, f.rbp, f.a5, f.a6, f.a4, f.flags, f.r12, f.r13, f.r14, f.r15]
}


/// Where a thread stood in user space when it left through a syscall — for a core dump
/// (`sched::set_exit_uregs`: rip, rsp, rbp, rbx, r12..r15, rflags, the syscall number).
pub fn exit_uregs(f: &Frame) -> [u64; 10] {
    [f.pc, f.usp, f.rbp, f.rbx, f.r12, f.r13, f.r14, f.r15, f.flags, f.nr]
}

/// A syscall's frame does not carry all the registers (the caller-saved ones are clobbered by
/// the call), so a dump taken from it is assembled from what there is: rip/rsp/rbp/rflags and
/// the callee-saved ones. For the analysis of the memory and the stack that is enough, while
/// lying with garbage in the other fields is not allowed — they are zeroes.
pub fn excframe_of_syscall(f: &Frame) -> ExcFrame {
    ExcFrame {
        r15: f.r15, r14: f.r14, r13: f.r13, r12: f.r12,
        r11: 0, r10: 0, r9: 0, r8: 0,
        rbp: f.rbp, rdi: 0, rsi: 0, rdx: 0, rcx: 0, rbx: f.rbx, rax: 0,
        vec: 0, err: 0, rip: f.pc, cs: 0x2B, rflags: f.flags,
        rsp: f.usp, ss: 0x23,
    }
}

/// The registers of a core dump's `NT_PRSTATUS`, in the order of `user_regs_struct` — that is
/// exactly what GDB expects. The order is not ours and must not be changed: fields swapped
/// round would give not "a format error" but a PLAUSIBLE wrong stack.
pub fn prstatus_regs(f: &ExcFrame, fs_base: u64) -> [u64; 27] {
    [
        f.r15, f.r14, f.r13, f.r12, f.rbp, f.rbx, f.r11, f.r10, f.r9, f.r8,
        f.rax, f.rcx, f.rdx, f.rsi, f.rdi,
        f.rax,          // orig_rax: for an exception's frame it makes no sense
        f.rip, f.cs, f.rflags, f.rsp, f.ss,
        fs_base, 0,     // fs_base, gs_base
        0, 0, 0, 0,     // ds, es, fs, gs
    ]
}
/// A frame "as if" for a thread that left through a syscall (`sched::exit_uregs`: rip, rsp,
/// rbp, rbx, r12..r15, rflags, the syscall number) — enough for gdb to show where it stood.
pub fn frame_of_exit_uregs(r: &[u64; 10]) -> ExcFrame {
    let mut f: ExcFrame = unsafe { core::mem::zeroed() };
    f.rip = r[0]; f.rsp = r[1]; f.rbp = r[2]; f.rbx = r[3];
    f.r12 = r[4]; f.r13 = r[5]; f.r14 = r[6]; f.r15 = r[7]; f.rflags = r[8];
    f.rax = r[9];
    f.cs = 0x2B; f.ss = 0x23;
    f
}
/// A frame "as if" for a thread that stands in the scheduler: the pc, the sp and the frame
/// pointer of its saved context.
pub fn frame_of_saved(pc: u64, sp: u64, fp: u64) -> ExcFrame {
    let mut f: ExcFrame = unsafe { core::mem::zeroed() };
    f.rip = pc; f.rsp = sp; f.rbp = fp;
    f.cs = 0x2B; f.ss = 0x23; f.rflags = 0x202;
    f
}
/// The register file a `clone` child starts with: the return value 0 and the function where
/// musl's `__clone` left it (r9); the rest zero, as it always was here.
pub fn clone_regs(_f: &Frame, func: u64) -> super::switch::Gp { let mut g = super::switch::GP_ZERO; g[8] = func; g }

/// The user stack pointer and the pc an exception's frame holds.
pub fn exc_sp(f: &ExcFrame) -> u64 { f.rsp }
pub fn exc_pc(f: &ExcFrame) -> u64 { f.rip }

/// What the fault was, from the exception's frame: (a page fault?, no page (else a rights
/// violation), a write, an instruction fetch). For a #PF the `err` bits say it; any other
/// exception is reported as a rights violation on a read.
pub fn fault_kind(f: &ExcFrame) -> (bool, bool, bool, bool) {
    (f.vec == 14, f.err & 1 == 0, f.err & 2 != 0, f.err & 0x10 != 0)
}
/// The raw error word, for the log lines that print it.
pub fn fault_err(f: &ExcFrame) -> u64 { f.err }

/// The `si_code` and `si_addr` of a synchronous signal: for a #PF it is SEGV_MAPERR (there is no
/// page) / SEGV_ACCERR (there are no rights) at the faulting address; the others at the rip.
pub fn exception_siginfo(f: &ExcFrame, sig: u64, fault_addr: u64) -> (i32, u64) {
    let code: i32 = match sig {
        11 if f.vec == 14 => if f.err & 1 != 0 { 2 } else { 1 },
        4 => 2, /*ILL_ILLOPN*/ 8 => 1, /*FPE_INTDIV*/ _ => 0x80, /*SI_KERNEL*/
    };
    (code, if f.vec == 14 { fault_addr } else { f.rip })
}

/// The registers from a SYSCALL's frame into `uc_mcontext.gregs`: `ret` plays rax, `usp` is the
/// stack the interrupted code had. `restart`: SA_RESTART on an interrupted restartable syscall —
/// the handler returns onto the `syscall` instruction itself (rip-2) with the number back in rax.
///
/// # Safety
/// `uc` is the ucontext's base inside a user frame that `paging::user_writable` has admitted.
pub unsafe fn write_syscall_context(uc: u64, f: &Frame, ret: u64, usp: u64, restart: bool, old_mask: u64) {
    let g = (uc + UC_GREGS) as *mut u64;
    *g.add(0) = f.a5; *g.add(1) = f.a6; *g.add(2) = f.a4; *g.add(3) = f.flags;
    *g.add(4) = f.r12; *g.add(5) = f.r13; *g.add(6) = f.r14; *g.add(7) = f.r15; *g.add(10) = f.rbp;
    *g.add(8) = f.a1; *g.add(9) = f.a2; *g.add(11) = f.rbx; *g.add(12) = f.a3; *g.add(13) = ret; *g.add(14) = f.pc;
    *g.add(15) = usp; *g.add(16) = f.pc; *g.add(17) = f.flags; *g.add(18) = 0x2b;
    if restart { *g.add(13) = f.nr; *g.add(16) = f.pc - 2; }
    *g.add(REG_OLDMASK) = old_mask;
}

/// The registers from an EXCEPTION's frame into `gregs`, with the real ERR / TRAPNO / CR2.
///
/// # Safety
/// As `write_syscall_context`.
pub unsafe fn write_exception_context(uc: u64, f: &ExcFrame, fault_addr: u64, old_mask: u64) {
    let g = (uc + UC_GREGS) as *mut u64;
    *g.add(0) = f.r8; *g.add(1) = f.r9; *g.add(2) = f.r10; *g.add(3) = f.r11;
    *g.add(4) = f.r12; *g.add(5) = f.r13; *g.add(6) = f.r14; *g.add(7) = f.r15;
    *g.add(8) = f.rdi; *g.add(9) = f.rsi; *g.add(10) = f.rbp; *g.add(11) = f.rbx;
    *g.add(12) = f.rdx; *g.add(13) = f.rax; *g.add(14) = f.rcx; *g.add(15) = f.rsp;
    *g.add(16) = f.rip; *g.add(17) = f.rflags; *g.add(18) = f.cs;
    *g.add(19) = f.err; *g.add(20) = f.vec; *g.add(22) = fault_addr;
    *g.add(REG_OLDMASK) = old_mask;
}

/// The FPU/SSE/AVX state of the interrupted code into the frame (`uc_mcontext.fpregs`): xsave
/// takes the LIVE registers (the upper halves of YMM the kernel does not touch — it is built
/// without AVX), while the low 128 bits on top come from the entry stub's area: there they are
/// "before" the entry into the kernel, while in the registers they are already ours.
///
/// # Safety
/// `base` is the frame's base (`SIGFRAME` bytes admitted); `entry_area` is the stub's 512-byte
/// fxsave area of this entry.
pub unsafe fn write_fpu(base: u64, uc: u64, entry_area: u64) {
    let fp = base + SIGFPU_OFF;
    super::switch::fpu_save(fp as *mut u8);
    core::ptr::copy_nonoverlapping(entry_area as *const u8, fp as *mut u8, 512);
    core::ptr::write_unaligned((uc + UC_FPREGS) as *mut u64, fp);
}

/// Redirect a SYSCALL's return into the handler: rdi=sig, rsi=&siginfo, rdx=&uc, rip=handler,
/// and the user rsp substituted at the sysret (the per-CPU `sig_newrsp`).
/// # Safety
/// `sp` is inside a frame `paging::user_writable` admitted (the restorer word goes there).
pub unsafe fn enter_handler_syscall(f: &mut Frame, sig: u64, si: u64, uc: u64, handler: u64, sp: u64, restorer: u64) {
    core::ptr::write_unaligned(sp as *mut u64, restorer);
    f.a1 = sig; f.a2 = si; f.a3 = uc; f.pc = handler;
    super::percpu::set_sig_newrsp(sp);
}

/// Redirect an EXCEPTION's return (the caller's iretq) into the handler.
/// # Safety
/// As `enter_handler_syscall`.
pub unsafe fn enter_handler_exception(f: &mut ExcFrame, sig: u64, si: u64, uc: u64, handler: u64, sp: u64, restorer: u64) {
    core::ptr::write_unaligned(sp as *mut u64, restorer);
    f.rdi = sig; f.rsi = si; f.rdx = uc;
    f.rip = handler; f.rsp = sp;
}

/// `rt_sigreturn`: the FPU/AVX from the frame straight into the registers, then the whole
/// register file and a hardware iret frame — the return goes THROUGH AN IRETQ rather than a sysret.
///
/// A `sysretq` physically cannot restore the context entirely: it REQUIRES that the return
/// address lie in rcx and the flags in r11. For an ordinary syscall that is lawful (the
/// `syscall` instruction destroys them anyway, the ABI allows that), but `rt_sigreturn`
/// returns not into the syscall's point but into ARBITRARY interrupted code. Giving it rcx and
/// r11 with foreign values means spoiling two live registers.
///
/// For the JVM that was not a theory. HotSpot catches a SIGSEGV at EVERY safepoint poll and at
/// every implicit null check — that is, in the middle of JIT code, where all the registers are
/// busy. After the return the code continued with spoiled rcx/r11: if a reference to an object
/// lay there, either the JIT code itself fell (`ConcurrentHashMap.putVal`) or later the garbage
/// collector during the walk over the roots. The failure looked "random" and was caught once in
/// ten runs. So here we do not return into the syscall's stub but put a full set of registers
/// and a hardware iret frame on the stack ourselves — exactly as Linux does.
///
/// # Safety
/// `uc` is the ucontext of a frame this kernel built (`uaccess::gate` admitted the address);
/// `kstack_top` is the current task's kernel stack top — the array must survive the change of
/// rsp, so it goes ABOVE the current top, where the stub will no longer return. It never
/// returns; the `u64` is the shared dispatcher's type (arm64 does return the new x0).
pub unsafe fn sigreturn(_f: &mut Frame, uc: u64, kstack_top: u64) -> u64 {
    let fp = core::ptr::read_unaligned((uc + UC_FPREGS) as *const u64);
    if fp != 0 { super::switch::fpu_restore(fp as *const u8); }
    let g = (uc + UC_GREGS) as *const u64;
    // The full set in the order in which `sig_iret` takes it off, and after it —
    // the hardware iret frame: rip, cs, rflags, rsp, ss.
    let mut r = [0u64; 20];
    r[0] = *g.add(13);            // rax
    r[1] = *g.add(11);            // rbx
    r[2] = *g.add(14);            // rcx  — the very one that was lost
    r[3] = *g.add(12);            // rdx
    r[4] = *g.add(9);             // rsi
    r[5] = *g.add(8);             // rdi
    r[6] = *g.add(10);            // rbp
    r[7] = *g.add(0);             // r8
    r[8] = *g.add(1);             // r9
    r[9] = *g.add(2);             // r10
    r[10] = *g.add(3);            // r11  — and this one
    r[11] = *g.add(4);            // r12
    r[12] = *g.add(5);            // r13
    r[13] = *g.add(6);            // r14
    r[14] = *g.add(7);            // r15
    r[15] = *g.add(16);           // rip
    r[16] = 0x2B;                 // cs (user code64 DPL3)
    r[17] = (*g.add(17) & 0x3F_7FD5) | 0x202; // rflags: IF=1, we do not let the garbage bits through
    r[18] = *g.add(15);           // rsp
    r[19] = 0x23;                 // ss (user data DPL3)
    let dst = ((kstack_top - 256) & !0xF) as *mut u64;
    core::ptr::copy_nonoverlapping(r.as_ptr(), dst, 20);
    super::syscall_entry::sig_iret(dst as *const u64)
}
