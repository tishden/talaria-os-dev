//! The AArch64 signal frame: the Linux `rt_sigframe` layout (`siginfo` + `ucontext` with
//! the `sigcontext` of 31 registers, sp, pc, pstate and the FP/SIMD record under a magic
//! header in `__reserved`), the registers a fork child and a core dump take from a frame,
//! the fault decode of `ESR_EL1`, and `rt_sigreturn` — which here is a rewrite of the
//! `svc` frame the stub restores, no `iretq` trick needed. The policy is `syscall.rs`'s,
//! shared with x86 (docs/ARM64-PLAN.md §4.4).

use super::trap::Frame;
use super::switch::Gp;

/// A siginfo (128) + a ucontext (4560: uc_flags, uc_link, uc_stack, uc_sigmask (128), the
/// padding to 16, `sigcontext` = fault_address + 31 regs + sp + pc + pstate + 4096 of
/// `__reserved`) — Linux's `rt_sigframe`, rounded to 16.
pub const SIGFRAME: u64 = 4704;
pub const SIGFRAME_ALIGN: u64 = 16;
pub const SIGINFO_SIZE: u64 = 128;
/// No return slot below the frame: the handler returns through x30 (the restorer).
pub const RET_SLOT: u64 = 0;
/// `ucontext`: `uc_stack` and `uc_sigmask` (a 128-byte `sigset_t`; the first word is used).
pub const UC_STACK: u64 = 16;
pub const UC_SIGMASK: u64 = 40;
/// `uc_mcontext` (16-aligned): fault_address, regs[31], sp, pc, pstate, `__reserved`.
const UC_MCONTEXT: u64 = 176;
const MC_FAULT: u64 = UC_MCONTEXT;
const MC_REGS: u64 = UC_MCONTEXT + 8;
const MC_SP: u64 = MC_REGS + 31 * 8;
const MC_PC: u64 = MC_SP + 8;
const MC_PSTATE: u64 = MC_PC + 8;
/// `__reserved` begins 16-aligned; the first record is `fpsimd_context`.
const MC_RESERVED: u64 = (MC_PSTATE + 8 + 15) & !15;
const FPSIMD_MAGIC: u32 = 0x4650_8001;
/// `fpsimd_context`: the header (magic, size), fpsr, fpcr, vregs[32].
const FPSIMD_SIZE: u32 = 16 + 512;
const FP_VREGS: u64 = MC_RESERVED + 16;
/// The PSTATE bits a process may set on itself: the condition flags.
const PSTATE_USER_MASK: u64 = 0xF000_0000;

/// The FP/SIMD area of the current entry: the stub's, 528 bytes below the frame
/// (q0..q31, then fpcr, fpsr — `trap.rs`'s FP_SAVE).
pub fn fpu_area(f: &Frame) -> u64 { (f as *const Frame as u64) - 528 }

/// The fork child's registers: the parent's file with x0 = 0 (the fork's return in the child).
pub fn fork_regs(f: &Frame) -> Gp {
    let mut g = [0u64; 31];
    for (i, w) in g.iter_mut().enumerate() { *w = f.x(i); }
    g[0] = 0;
    g
}
/// The callee-saved registers a frame carries (x19..x29), for a context that moves cores.
pub fn callee_saved(f: &Frame) -> Gp {
    let mut g = [0u64; 31];
    for i in 19..30 { g[i] = f.x(i); }
    g
}
/// Where a thread stood when it left through a syscall — for a core dump.
pub fn exit_uregs(f: &Frame) -> [u64; 10] {
    [f.pc, f.usp, f.fp, f.x(19), f.x(20), f.x(21), f.x(22), f.x(23), f.flags, f.nr]
}
/// An exception frame from a syscall's: the same structure on this machine.
pub fn excframe_of_syscall(f: &Frame) -> Frame { Frame { ..*f } }
pub fn exc_sp(f: &Frame) -> u64 { f.usp }
pub fn exc_pc(f: &Frame) -> u64 { f.pc }

/// The registers of a core dump's `NT_PRSTATUS`: `user_pt_regs` — regs[31], sp, pc, pstate.
pub fn prstatus_regs(f: &Frame, _tls: u64) -> [u64; 34] {
    let mut r = [0u64; 34];
    for (i, w) in r.iter_mut().take(31).enumerate() { *w = f.x(i); }
    r[31] = f.usp; r[32] = f.pc; r[33] = f.flags;
    r
}
/// A frame "as if" for a thread that left through a syscall (`exit_uregs` above).
pub fn frame_of_exit_uregs(r: &[u64; 10]) -> Frame {
    let mut f: Frame = unsafe { core::mem::zeroed() };
    f.pc = r[0]; f.usp = r[1]; f.fp = r[2];
    f.set_x(19, r[3]); f.set_x(20, r[4]); f.set_x(21, r[5]); f.set_x(22, r[6]); f.set_x(23, r[7]);
    f.flags = r[8]; f.nr = r[9];
    f
}
/// A frame "as if" for a thread that stands in the scheduler.
pub fn frame_of_saved(pc: u64, sp: u64, fp: u64) -> Frame {
    let mut f: Frame = unsafe { core::mem::zeroed() };
    f.pc = pc; f.usp = sp; f.fp = fp;
    f
}
/// The register file a `clone` child starts with: the parent's whole file (musl's `__clone`
/// keeps the function in x10 and the argument in x12 across the `svc`) with x0 = 0.
pub fn clone_regs(f: &Frame, _func: u64) -> Gp { fork_regs(f) }

fn ec(f: &Frame) -> u64 { (f.esr >> 26) & 0x3F }
/// What the fault was: (an abort?, no translation (else a permission fault), a write, an
/// instruction fetch). The classes: 0x20/0x21 instruction aborts, 0x24/0x25 data aborts;
/// DFSC/IFSC 0x4..0x7 are translation faults, WnR is ISS bit 6 of a data abort.
pub fn fault_kind(f: &Frame) -> (bool, bool, bool, bool) {
    let e = ec(f);
    let abort = matches!(e, 0x20 | 0x21 | 0x24 | 0x25);
    let fsc = f.esr & 0x3F;
    let no_page = abort && (0x4..=0x7).contains(&fsc);
    let write = matches!(e, 0x24 | 0x25) && f.esr & (1 << 6) != 0;
    (abort, no_page, write, matches!(e, 0x20 | 0x21))
}
pub fn fault_err(f: &Frame) -> u64 { f.esr }
/// `si_code` and `si_addr`: SEGV_MAPERR for a translation fault, SEGV_ACCERR for a
/// permission one, at the faulting address; the others at the pc.
pub fn exception_siginfo(f: &Frame, sig: u64, fault_addr: u64) -> (i32, u64) {
    let (abort, no_page, _, _) = fault_kind(f);
    let code: i32 = match sig {
        11 if abort => if no_page { 1 } else { 2 },
        4 => 2, /*ILL_ILLOPN*/ 8 => 1, /*FPE_INTDIV*/ 7 => 1, /*BUS_ADRALN*/ _ => 0x80, /*SI_KERNEL*/
    };
    (code, if abort { fault_addr } else { f.pc })
}

/// # Safety
/// `uc` is the ucontext's base inside a user frame that `paging::user_writable` admitted.
unsafe fn write_regs(uc: u64, f: &Frame, x0: u64, usp: u64, pc: u64, fault_addr: u64) {
    let r = (uc + MC_REGS) as *mut u64;
    for i in 0..31 { core::ptr::write_unaligned(r.add(i), f.x(i)); }
    core::ptr::write_unaligned(r, x0);
    core::ptr::write_unaligned((uc + MC_SP) as *mut u64, usp);
    core::ptr::write_unaligned((uc + MC_PC) as *mut u64, pc);
    core::ptr::write_unaligned((uc + MC_PSTATE) as *mut u64, f.flags);
    core::ptr::write_unaligned((uc + MC_FAULT) as *mut u64, fault_addr);
}
/// The registers from a SYSCALL's frame into the sigcontext: `ret` plays x0. `restart`
/// (SA_RESTART on an interrupted restartable syscall): the handler returns onto the `svc`
/// itself (pc - 4) with the original x0 back — x8 still holds the number.
/// # Safety
/// As `write_regs`.
pub unsafe fn write_syscall_context(uc: u64, f: &Frame, ret: u64, usp: u64, restart: bool, _old_mask: u64) {
    if restart { write_regs(uc, f, f.a1, usp, f.pc.wrapping_sub(4), 0); }
    else { write_regs(uc, f, ret, usp, f.pc, 0); }
}
/// The registers from an EXCEPTION's frame, with the faulting address.
/// # Safety
/// As `write_regs`.
pub unsafe fn write_exception_context(uc: u64, f: &Frame, fault_addr: u64, _old_mask: u64) {
    write_regs(uc, f, f.a1, f.usp, f.pc, fault_addr);
}
/// The FP/SIMD state of the interrupted code into `__reserved`: the `fpsimd_context`
/// record from the entry stub's area, and the terminating zero header after it.
/// # Safety
/// `base` is the frame's base; `entry_area` the stub's 528-byte area of this entry.
pub unsafe fn write_fpu(_base: u64, uc: u64, entry_area: u64) {
    let h = (uc + MC_RESERVED) as *mut u32;
    core::ptr::write_unaligned(h, FPSIMD_MAGIC);
    core::ptr::write_unaligned(h.add(1), FPSIMD_SIZE);
    let fpcr = core::ptr::read_unaligned((entry_area + 512) as *const u64);
    let fpsr = core::ptr::read_unaligned((entry_area + 520) as *const u64);
    core::ptr::write_unaligned(h.add(2), fpsr as u32);
    core::ptr::write_unaligned(h.add(3), fpcr as u32);
    core::ptr::copy_nonoverlapping(entry_area as *const u8, (uc + FP_VREGS) as *mut u8, 512);
    core::ptr::write_unaligned((uc + FP_VREGS + 512) as *mut u64, 0); // the terminator header
}
/// Redirect a syscall's return into the handler: x0 = sig, x1 = &siginfo, x2 = &uc, the
/// restorer in x30, the frame's sp slot = the handler's stack (the stub restores SP_EL0).
/// # Safety
/// The frame is this entry's; `sp` is in the admitted frame.
pub unsafe fn enter_handler_syscall(f: &mut Frame, sig: u64, si: u64, uc: u64, handler: u64, sp: u64, restorer: u64) {
    f.a1 = sig; f.a2 = si; f.a3 = uc; f.lr = restorer; f.pc = handler; f.usp = sp;
}
/// # Safety
/// As `enter_handler_syscall`.
pub unsafe fn enter_handler_exception(f: &mut Frame, sig: u64, si: u64, uc: u64, handler: u64, sp: u64, restorer: u64) {
    enter_handler_syscall(f, sig, si, uc, handler, sp, restorer);
}

/// `rt_sigreturn`: the sigcontext back into THIS frame — the stub's restore is the return —
/// and the FP/SIMD record back into the entry's area, which the stub reloads. PSTATE takes
/// only the condition flags from the process. The value returned is the frame's new x0.
/// # Safety
/// `uc` is the ucontext of a frame this kernel built (`uaccess::gate` admitted the address).
pub unsafe fn sigreturn(f: &mut Frame, uc: u64, _kstack_top: u64) -> u64 {
    let r = (uc + MC_REGS) as *const u64;
    for i in 0..31 { f.set_x(i, core::ptr::read_unaligned(r.add(i))); }
    f.usp = core::ptr::read_unaligned((uc + MC_SP) as *const u64);
    f.pc = core::ptr::read_unaligned((uc + MC_PC) as *const u64);
    f.flags = core::ptr::read_unaligned((uc + MC_PSTATE) as *const u64) & PSTATE_USER_MASK;
    let h = (uc + MC_RESERVED) as *const u32;
    if core::ptr::read_unaligned(h) == FPSIMD_MAGIC {
        let area = fpu_area(f);
        core::ptr::copy_nonoverlapping((uc + FP_VREGS) as *const u8, area as *mut u8, 512);
        let fpsr = core::ptr::read_unaligned(h.add(2)) as u64;
        let fpcr = core::ptr::read_unaligned(h.add(3)) as u64;
        core::ptr::write_unaligned((area + 512) as *mut u64, fpcr);
        core::ptr::write_unaligned((area + 520) as *mut u64, fpsr);
    }
    f.a1
}
