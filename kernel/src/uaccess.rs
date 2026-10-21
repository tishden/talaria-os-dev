//! Validation of the pointers a process passes to system calls.
//!
//! The kernel copies to and from process memory with plain loads and stores,
//! so a pointer argument that points into the kernel (its image, page tables,
//! kernel stacks, the block cache) would make the kernel corrupt itself on the
//! process's behalf. Every syscall entry from ring 3 is therefore checked here
//! BEFORE dispatch: each pointer argument, with the length the kernel is going
//! to touch, must lie in memory the process itself can reach
//! (`paging::user_range_ok`). A failed check returns `-EFAULT`, exactly as
//! Linux does, and the handler never runs.
//!
//! The per-syscall knowledge lives in one declarative table (`sig`) so that it
//! can be read against the Linux ABI and unit-tested on the host. Arguments
//! whose extent is data-dependent (iovec arrays, `msghdr`, `select` fd sets,
//! futex operations, `execve` vectors, ...) are checked by `nested`.
//!
//! What this does not cover: `ioctl`/`fcntl` commands that the kernel does not
//! implement (it does not touch their argument), and modes that run the
//! workload in the kernel's own identity address space (`talaria.cell=identity`,
//! the embedded demo modes) — there the identity window above the kernel image
//! is user-accessible by construction, and only the kernel image is protected.
//!
//! `talaria.uacheck=0` turns the check off (A/B measurement of its cost).
//!
//! # Safety
//!
//! All the `unsafe` in this module is one thing: `Live` dereferences a raw
//! address the PROCESS gave us. It rests on two invariants, and both are local
//! enough to check by reading this file:
//!
//! - **U1. Read only what `ok` has already accepted.** `Mem::read_*` is declared
//!   for addresses `Mem::ok` has returned `true` for, and every caller in this
//!   module (`ptr_ok`, `str_ok`, `nested`) asks `ok` for the same extent first.
//!   `ok` is `paging::user_range_ok`, which walks the process's own page tables,
//!   so an accepted address is mapped and reachable from ring 3 — the load cannot
//!   fault and cannot reach the kernel's own memory.
//! - **U2. The process may change the bytes under us.** It is another thread's
//!   memory: a value read here may be stale by the time the handler uses it. That
//!   is why nothing is CACHED across the check — the extents derived from a read
//!   value (an `iovec` length, a `msghdr` count) are re-validated by the handler
//!   against the same memory, and a torn read can only make the check refuse a
//!   call that would have been fine. It can never let through an address the
//!   handler then trusts, because the handler checks the address it is about to
//!   touch, not the one we read.
//!
//! The reads are unaligned and volatile on purpose: a process is entitled to pass
//! a misaligned pointer, and the compiler must not fuse or hoist a load of memory
//! somebody else writes.

use core::sync::atomic::AtomicBool;

const EFAULT: u64 = (-14i64) as u64;

/// A ring-3 address in the current address space as a pointer — the one kind of address the
/// direct map never translates (`mem::kva` and its kin name the others).
#[inline(always)]
pub fn uptr<T>(va: u64) -> *mut T { core::ptr::with_exposed_provenance_mut(va as usize) }

/// Is the check enabled (`talaria.uacheck`, default on).
pub static ENABLED: AtomicBool = AtomicBool::new(true);

/// How long the memory behind a pointer argument is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Len {
    /// A fixed-size structure.
    Fixed(u32),
    /// The value of another argument (1-based: `Arg(3)` is the third).
    Arg(u8),
    /// Another argument times an element size (`poll` fds, `epoll_wait` events).
    ArgMul(u8, u32),
    /// Another argument's value, but no more than the first argument's descriptor can
    /// deliver (`Mem::read_cap`): a `read` of 1 MiB from a pipe or a socket writes at most
    /// the pipe's ring or the socket's receive buffer, so only that much of the buffer must
    /// be the process's. Checking the whole asked length cost 3 us per EMPTY receive on a
    /// Graviton (tlgen asks for 1 MiB on every poll; ARM64-PLAN §11 item 2a); Linux checks
    /// nothing up front and faults on the bytes it writes.
    ArgCap(u8),
    /// A NUL-terminated string (paths, names).
    Str,
}

/// One pointer argument of a syscall.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ptr {
    /// Argument number, 1-based as in the Linux ABI documentation.
    pub arg: u8,
    pub len: Len,
    /// The kernel writes through it (otherwise it only reads).
    pub write: bool,
    /// NULL is a valid value (an optional argument).
    pub opt: bool,
}

const fn rd(arg: u8, len: Len) -> Ptr { Ptr { arg, len, write: false, opt: false } }
const fn wr(arg: u8, len: Len) -> Ptr { Ptr { arg, len, write: true, opt: false } }
const fn rd_opt(arg: u8, len: Len) -> Ptr { Ptr { arg, len, write: false, opt: true } }
const fn wr_opt(arg: u8, len: Len) -> Ptr { Ptr { arg, len, write: true, opt: true } }

use Len::{Arg, ArgCap, ArgMul, Fixed, Str};

// Sizes of the structures the kernel reads or writes. They are x86-64's except the two
// an arm64 process sees in another layout (`arch::aarch64::abi` translates them at the
// dispatcher's door): the gate judges the extent THE PROCESS's structure has.
const STAT: u32 = if cfg!(target_arch = "aarch64") { 128 } else { 144 };
const STATFS: u32 = 120;
const STATX: u32 = 256;
const TIMESPEC: u32 = 16;
const TIMEVAL: u32 = 16;
const ITIMERVAL: u32 = 32;
const ITIMERSPEC: u32 = 32;
const SIGSET: u32 = 8;
const SIGACTION: u32 = 32;
const STACK_T: u32 = 24;
const SIGINFO: u32 = 128;
const RUSAGE: u32 = 144;
const RLIMIT: u32 = 16;
const SYSINFO: u32 = 112;
const TMS: u32 = 32;
const UTSNAME: u32 = 390;
const SOCKADDR: u32 = 16;
const SOCKLEN: u32 = 4;
const POLLFD: u32 = 8;
const EPOLL_EVENT: u32 = if cfg!(target_arch = "aarch64") { 16 } else { 12 };
const IOVEC: u32 = 16;
const MSGHDR: u32 = 56;
const UTIMBUF: u32 = 16;
const CAP_HEADER: u32 = 8;
const CAP_DATA: u32 = 24;

/// A `'static` table row (a `const` item, so that `const fn` rows are promoted).
macro_rules! t {
    ($($p:expr),*) => {{ const ROW: &[Ptr] = &[$($p),*]; ROW }};
}

/// Pointer arguments of syscall `nr` whose extent is known from the arguments
/// alone. An empty slice means "no pointer arguments, or only the ones
/// `nested` handles".
pub fn sig(nr: u64) -> &'static [Ptr] {
    match nr {
        0 => t![wr(2, ArgCap(3))],                                     // read (a pipe or a socket caps it)
        17 => t![wr(2, Arg(3))],                                       // pread64
        1 | 18 => t![rd(2, Arg(3))],                                   // write, pwrite64
        2 | 21 | 76 | 80 | 83 | 84 | 87 | 90 | 92 | 94 => t![rd(1, Str)], // open access truncate chdir mkdir rmdir unlink chmod chown lchown
        4 | 6 => t![rd(1, Str), wr(2, Fixed(STAT))],                   // stat, lstat
        5 => t![wr(2, Fixed(STAT))],                                   // fstat
        7 => t![wr_opt(1, ArgMul(2, POLLFD))],                         // poll
        13 => t![rd_opt(2, Fixed(SIGACTION)), wr_opt(3, Fixed(SIGACTION))], // rt_sigaction
        14 => t![rd_opt(2, Fixed(SIGSET)), wr_opt(3, Fixed(SIGSET))],  // rt_sigprocmask
        22 | 293 => t![wr(1, Fixed(8))],                               // pipe, pipe2
        35 => t![rd(1, Fixed(TIMESPEC)), wr_opt(2, Fixed(TIMESPEC))],  // nanosleep
        36 => t![wr(2, Fixed(ITIMERVAL))],                             // getitimer
        38 => t![rd_opt(2, Fixed(ITIMERVAL)), wr_opt(3, Fixed(ITIMERVAL))], // setitimer
        42 | 49 => t![rd(2, Arg(3))],                                  // connect, bind
        43 | 288 | 51 | 52 => t![wr_opt(2, Fixed(SOCKADDR)), wr_opt(3, Fixed(SOCKLEN))], // accept accept4 getsockname getpeername
        44 => t![rd(2, Arg(3)), rd_opt(5, Fixed(SOCKADDR))],           // sendto
        45 => t![wr(2, ArgCap(3)), wr_opt(5, Fixed(SOCKADDR)), wr_opt(6, Fixed(SOCKLEN))], // recvfrom (the socket caps it)
        53 => t![wr(4, Fixed(8))],                                     // socketpair
        54 => t![rd_opt(4, Arg(5))],                                   // setsockopt
        61 => t![wr_opt(2, Fixed(4)), wr_opt(4, Fixed(RUSAGE))],       // wait4
        63 => t![wr(1, Fixed(UTSNAME))],                               // uname
        79 => t![wr(1, Arg(2))],                                       // getcwd
        82 | 86 | 88 => t![rd(1, Str), rd(2, Str)],                    // rename, link, symlink
        89 => t![rd(1, Str), wr(2, Arg(3))],                           // readlink
        96 => t![wr_opt(1, Fixed(TIMEVAL)), wr_opt(2, Fixed(8))],      // gettimeofday
        97 => t![wr(2, Fixed(RLIMIT))],                                // getrlimit
        98 => t![wr(2, Fixed(RUSAGE))],                                // getrusage
        99 => t![wr(1, Fixed(SYSINFO))],                               // sysinfo
        100 => t![wr_opt(1, Fixed(TMS))],                              // times
        118 | 120 => t![wr(1, Fixed(4)), wr(2, Fixed(4)), wr(3, Fixed(4))], // getresuid, getresgid
        125 => t![wr(1, Fixed(CAP_HEADER)), wr_opt(2, Fixed(CAP_DATA))], // capget
        127 => t![wr_opt(1, Fixed(SIGSET))],                           // rt_sigpending
        128 => t![rd(1, Fixed(SIGSET)), wr_opt(2, Fixed(SIGINFO)), rd_opt(3, Fixed(TIMESPEC))], // rt_sigtimedwait
        130 => t![rd(1, Fixed(SIGSET))],                               // rt_sigsuspend
        131 => t![rd_opt(1, Fixed(STACK_T)), wr_opt(2, Fixed(STACK_T))], // sigaltstack
        132 => t![rd(1, Str), rd_opt(2, Fixed(UTIMBUF))],              // utime
        137 => t![rd(1, Str), wr(2, Fixed(STATFS))],                   // statfs
        138 => t![wr(2, Fixed(STATFS))],                               // fstatfs
        142 => t![rd(2, Fixed(4))],                                    // sched_setparam
        144 => t![rd_opt(3, Fixed(4))],                                // sched_setscheduler
        148 => t![wr(2, Fixed(TIMESPEC))],                             // sched_rr_get_interval
        201 => t![wr_opt(1, Fixed(8))],                                // time
        203 => t![rd(3, Arg(2))],                                      // sched_setaffinity
        204 => t![wr(3, Arg(2))],                                      // sched_getaffinity
        217 => t![wr(2, Arg(3))],                                      // getdents64
        218 => t![wr_opt(1, Fixed(4))],                                // set_tid_address
        228 | 229 => t![wr_opt(2, Fixed(TIMESPEC))],                   // clock_gettime, clock_getres
        230 => t![rd(3, Fixed(TIMESPEC)), wr_opt(4, Fixed(TIMESPEC))], // clock_nanosleep
        232 => t![wr(2, ArgMul(3, EPOLL_EVENT))],                      // epoll_wait
        239 => t![wr_opt(1, Fixed(4)), wr_opt(2, Fixed(8))],           // get_mempolicy (the mode; the first word of the mask)
        233 => t![rd_opt(4, Fixed(EPOLL_EVENT))],                      // epoll_ctl (NULL for EPOLL_CTL_DEL)
        235 => t![rd(1, Str), rd_opt(2, Fixed(2 * TIMEVAL))],          // utimes
        257 | 258 | 260 | 263 | 268 | 269 | 439 => t![rd(2, Str)],     // openat mkdirat fchownat unlinkat fchmodat faccessat faccessat2
        262 => t![rd(2, Str), wr(3, Fixed(STAT))],                     // newfstatat
        264 | 265 | 316 => t![rd(2, Str), rd(4, Str)],                 // renameat, linkat, renameat2
        266 => t![rd(1, Str), rd(3, Str)],                             // symlinkat
        267 => t![rd(2, Str), wr(3, Arg(4))],                          // readlinkat
        271 => t![wr_opt(1, ArgMul(2, POLLFD)), rd_opt(3, Fixed(TIMESPEC)), rd_opt(4, Fixed(SIGSET))], // ppoll
        280 => t![rd_opt(2, Str), rd_opt(3, Fixed(2 * TIMESPEC))],     // utimensat
        281 => t![wr(2, ArgMul(3, EPOLL_EVENT)), rd_opt(5, Fixed(SIGSET))], // epoll_pwait
        282 | 289 => t![rd(2, Fixed(SIGSET))],                         // signalfd, signalfd4
        286 => t![rd(3, Fixed(ITIMERSPEC)), wr_opt(4, Fixed(ITIMERSPEC))], // timerfd_settime
        287 => t![wr(2, Fixed(ITIMERSPEC))],                           // timerfd_gettime
        302 => t![rd_opt(3, Fixed(RLIMIT)), wr_opt(4, Fixed(RLIMIT))], // prlimit64
        309 => t![wr_opt(1, Fixed(4)), wr_opt(2, Fixed(4))],           // getcpu
        318 => t![wr(1, Arg(2))],                                      // getrandom
        332 => t![rd(2, Str), wr(5, Fixed(STATX))],                    // statx
        4602 => t![rd(1, Fixed(4)), wr_opt(5, Fixed(4))],              // upark (PLAN-SYNC)
        _ => &[],
    }
}

/// Raw syscall arguments, 1-based access.
#[derive(Clone, Copy)]
pub struct Args(pub [u64; 6]);

impl Args {
    #[inline]
    pub fn get(&self, n: u8) -> u64 { self.0[n as usize - 1] }
}

/// The memory check, abstracted so that the table logic can be unit-tested on
/// the host against a fake address space. In the kernel it is
/// `paging::user_range_ok` plus plain loads (after the range was checked).
pub trait Mem {
    fn ok(&self, addr: u64, len: u64, write: bool) -> bool;
    /// Read a u64 / u32 that `ok` has already accepted.
    fn read_u64(&self, addr: u64) -> u64;
    fn read_u32(&self, addr: u64) -> u32;
    /// Byte at `addr` — only called for addresses `ok` accepted.
    fn read_u8(&self, addr: u64) -> u8;
    /// The most a `read` can write through the process's descriptor `fd` (a pipe's ring, a
    /// socket's receive buffer), or None when the whole asked length may be filled (a file).
    fn read_cap(&self, _fd: u64) -> Option<u64> { None }
}

/// Longest string a path-like argument may have (PATH_MAX, with the NUL).
pub const STR_MAX: u64 = 4096;

/// A NUL-terminated string at `addr` is readable up to and including its NUL.
/// Checked page by page, so a name that runs into an unmapped page is refused
/// before the kernel reads past the boundary. Longer than `STR_MAX` — refused.
pub fn str_ok<M: Mem>(m: &M, addr: u64) -> bool {
    let mut a = addr;
    while a - addr < STR_MAX {
        let page_end = (a & !0xFFF) + 0x1000;
        let chunk = (page_end - a).min(STR_MAX - (a - addr));
        if !m.ok(a, chunk, false) { return false; }
        for i in 0..chunk {
            if m.read_u8(a + i) == 0 { return true; }
        }
        a += chunk;
    }
    false
}

fn ptr_ok<M: Mem>(m: &M, p: &Ptr, a: &Args) -> bool {
    let addr = a.get(p.arg);
    let len = match p.len {
        Fixed(n) => n as u64,
        Arg(i) => a.get(i),
        ArgMul(i, sz) => match a.get(i).checked_mul(sz as u64) { Some(n) => n, None => return false },
        ArgCap(i) => match m.read_cap(a.get(1)) { Some(c) => a.get(i).min(c), None => a.get(i) },
        Str => {
            if addr == 0 { return p.opt; }
            return str_ok(m, addr);
        }
    };
    if addr == 0 { return p.opt || len == 0; }
    m.ok(addr, len, p.write)
}

/// An array of `n` iovecs at `iov`: the array itself, then every buffer it
/// names (`write` — the kernel fills the buffers, as in readv/recvmsg).
fn iovecs_ok<M: Mem>(m: &M, iov: u64, n: u64, write: bool) -> bool {
    if n == 0 { return true; }
    if n > 1024 { return false; } // IOV_MAX
    if iov == 0 || !m.ok(iov, n * IOVEC as u64, false) { return false; }
    for i in 0..n {
        let base = m.read_u64(iov + i * 16);
        let len = m.read_u64(iov + i * 16 + 8);
        if len != 0 && (base == 0 || !m.ok(base, len, write)) { return false; }
    }
    true
}

/// A `msghdr` at `msg`: the header (the kernel writes `msg_controllen` back),
/// the optional address, and the iovec array.
fn msghdr_ok<M: Mem>(m: &M, msg: u64, recv: bool) -> bool {
    if msg == 0 || !m.ok(msg, MSGHDR as u64, true) { return false; }
    let name = m.read_u64(msg);
    if name != 0 && !m.ok(name, SOCKADDR as u64, recv) { return false; }
    iovecs_ok(m, m.read_u64(msg + 16), m.read_u64(msg + 24), recv)
}

/// A NULL-terminated vector of strings (`execve` argv/envp).
fn strvec_ok<M: Mem>(m: &M, vec: u64) -> bool {
    if vec == 0 { return true; }
    for i in 0..4096u64 {
        let slot = vec + i * 8;
        if !m.ok(slot, 8, false) { return false; }
        let s = m.read_u64(slot);
        if s == 0 { return true; }
        if !str_ok(m, s) { return false; }
    }
    false
}

/// Pointer arguments whose extent depends on the data (or on another
/// argument's meaning). `true` — nothing to check or everything checked.
pub fn nested<M: Mem>(m: &M, nr: u64, a: &Args) -> bool {
    match nr {
        19 | 295 | 327 => iovecs_ok(m, a.get(2), a.get(3), true),   // readv, preadv, preadv2
        20 | 296 | 328 => iovecs_ok(m, a.get(2), a.get(3), false),  // writev, pwritev, pwritev2
        46 => msghdr_ok(m, a.get(2), false),                        // sendmsg
        47 => msghdr_ok(m, a.get(2), true),                         // recvmsg
        299 | 307 => {                                              // recvmmsg, sendmmsg: mmsghdr = msghdr + u32 len (64 bytes)
            let (base, n) = (a.get(2), a.get(3).min(1024));
            if n == 0 { return true; }
            if base == 0 || !m.ok(base, n * 64, true) { return false; }
            (0..n).all(|k| msghdr_ok(m, base + k * 64, nr == 299))
                && (nr == 307 || a.get(5) == 0 || m.ok(a.get(5), TIMESPEC as u64, false))
        }
        23 | 270 => {                                              // select, pselect6: three fd_sets, read and written in u64 words
            let bytes = a.get(1).min(1024).div_ceil(64) * 8;
            (2..=4).all(|i| a.get(i) == 0 || m.ok(a.get(i), bytes, true))
                && (a.get(5) == 0 || m.ok(a.get(5), TIMEVAL as u64, false))
        }
        55 => {                                                     // getsockopt(fd, level, name, val, *len)
            let lenp = a.get(5);
            if lenp == 0 { return a.get(4) == 0; }
            if !m.ok(lenp, SOCKLEN as u64, true) { return false; }
            let len = m.read_u32(lenp) as u64;
            a.get(4) == 0 || m.ok(a.get(4), len, true)
        }
        56 => {                                                     // clone(flags, stack, ptid, ctid, tls)
            let fl = a.get(1);
            (fl & 0x0010_0000 == 0 || m.ok(a.get(3), 4, true))      // CLONE_PARENT_SETTID
                && (fl & 0x0120_0000 == 0 || m.ok(a.get(4), 4, true)) // CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID
        }
        59 => str_ok(m, a.get(1)) && strvec_ok(m, a.get(2)) && strvec_ok(m, a.get(3)), // execve
        157 => match a.get(1) {                                     // prctl
            15 => str_ok(m, a.get(2)),                              // PR_SET_NAME
            16 => m.ok(a.get(2), 16, true),                         // PR_GET_NAME
            _ => true,
        },
        158 => match a.get(1) {                                     // arch_prctl
            0x1003 | 0x1004 => m.ok(a.get(2), 8, true),             // ARCH_GET_FS / ARCH_GET_GS
            _ => true,
        },
        202 => {                                                    // futex(uaddr, op, val, timeout|val2, uaddr2, val3)
            let op = a.get(2) & 0x7F;
            if !m.ok(a.get(1), 4, false) { return false; }
            let timeout = matches!(op, 0 | 6 | 9 | 11);             // WAIT, LOCK_PI, WAIT_BITSET, WAIT_REQUEUE_PI
            if timeout && a.get(4) != 0 && !m.ok(a.get(4), TIMESPEC as u64, false) { return false; }
            match op {
                3 | 4 | 12 => m.ok(a.get(5), 4, false),             // REQUEUE, CMP_REQUEUE, CMP_REQUEUE_PI
                5 => m.ok(a.get(5), 4, true),                       // WAKE_OP writes *uaddr2
                _ => true,
            }
        }
        16 if a.get(2) == 0x5421 => m.ok(a.get(3), 4, false),      // ioctl FIONBIO reads an int
        31 if a.get(2) == 2 => a.get(3) == 0 || m.ok(a.get(3), 112, true), // shmctl IPC_STAT
        _ => true,
    }
}

/// Check all pointer arguments of a syscall. `true` — dispatch it.
pub fn check<M: Mem>(m: &M, nr: u64, a: &Args) -> bool {
    sig(nr).iter().all(|p| ptr_ok(m, p, a)) && nested(m, nr, a)
}

/// The kernel's view of process memory for `check`.
#[cfg(not(talaria_host))]
pub struct Live;

#[cfg(not(talaria_host))]
impl Mem for Live {
    #[inline]
    fn ok(&self, addr: u64, len: u64, write: bool) -> bool { crate::paging::user_range_ok(addr, len, write) }
    #[inline]
    fn read_u64(&self, addr: u64) -> u64 {
        // SAFETY: U1 — the trait declares `read_u64` for addresses `ok` accepted, and
        // every caller asks `ok` for these 8 bytes first; unaligned because a process
        // may pass a misaligned pointer. U2: the value may be stale, never trusted as
        // an address.
        unsafe { core::ptr::read_unaligned(uptr::<u64>(addr)) }
    }
    #[inline]
    fn read_u32(&self, addr: u64) -> u32 {
        // SAFETY: U1 for 4 bytes, U2 for the value — as `read_u64`.
        unsafe { core::ptr::read_unaligned(uptr::<u32>(addr)) }
    }
    #[inline]
    fn read_u8(&self, addr: u64) -> u8 {
        // SAFETY: U1 — `str_ok` accepts the page-sized chunk before walking it byte by
        // byte, so this byte is mapped. Volatile: the loop looks for a NUL in memory a
        // process's other thread may be writing, and the compiler must not fuse the
        // loads.
        unsafe { core::ptr::read_volatile(uptr::<u8>(addr)) }
    }
    /// The gate runs before the fd translation, so `fd` is the process's number. The caps
    /// are the owners' own constants (`netsys::read_cap`, `proc::pipe_capacity`): the handler
    /// copies into the buffer through those same bounded rings, never past them.
    fn read_cap(&self, fd: u64) -> Option<u64> {
        let h = crate::proc::tr(fd);
        if crate::proc::pipe_is(h) { return Some(crate::proc::pipe_capacity() as u64); }
        crate::netsys::read_cap(h).map(|c| c as u64)
    }
}

/// Entry point from the syscall path: `Some(-EFAULT)` refuses the call.
#[cfg(not(talaria_host))]
#[inline]
pub fn gate(nr: u64, args: [u64; 6]) -> Option<u64> {
    if !ENABLED.load(core::sync::atomic::Ordering::Relaxed) { return None; }
    if check(&Live, nr, &Args(args)) { None } else { Some(EFAULT) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// A fake address space: pages (4 KiB) mapped read-only or read-write,
    /// with byte contents. Everything else is unmapped.
    #[derive(Default)]
    struct Fake {
        pages: BTreeMap<u64, bool>, // page base -> writable
        bytes: RefCell<BTreeMap<u64, u8>>,
        cap: Option<u64>,           // what a `read` through any fd can deliver (None: everything)
    }

    impl Fake {
        fn map(mut self, base: u64, len: u64, writable: bool) -> Self {
            let mut p = base & !0xFFF;
            while p < base + len { self.pages.insert(p, writable); p += 0x1000; }
            self
        }
        fn put(&self, addr: u64, data: &[u8]) {
            for (i, b) in data.iter().enumerate() { self.bytes.borrow_mut().insert(addr + i as u64, *b); }
        }
        fn put_u64(&self, addr: u64, v: u64) { self.put(addr, &v.to_le_bytes()); }
    }

    impl Mem for Fake {
        fn ok(&self, addr: u64, len: u64, write: bool) -> bool {
            if len == 0 { return true; }
            let end = match addr.checked_add(len) { Some(e) => e, None => return false };
            let mut p = addr & !0xFFF;
            while p < end {
                match self.pages.get(&p) { Some(&w) if w || !write => {}, _ => return false }
                p += 0x1000;
            }
            true
        }
        fn read_u64(&self, a: u64) -> u64 {
            let mut v = [0u8; 8];
            for (i, b) in v.iter_mut().enumerate() { *b = self.read_u8(a + i as u64); }
            u64::from_le_bytes(v)
        }
        fn read_u32(&self, a: u64) -> u32 { self.read_u64(a) as u32 }
        fn read_u8(&self, a: u64) -> u8 { *self.bytes.borrow().get(&a).unwrap_or(&0xAA) }
        fn read_cap(&self, _fd: u64) -> Option<u64> { self.cap }
    }

    const RW: u64 = 0x1000_0000; // 64 KiB read-write
    const RO: u64 = 0x2000_0000; // 4 KiB read-only
    const KERNEL: u64 = 0x20_1000; // not in the fake map: "the kernel"

    fn mem() -> Fake { Fake::default().map(RW, 0x10000, true).map(RO, 0x1000, false) }
    fn args(a: &[u64]) -> Args { let mut v = [0u64; 6]; v[..a.len()].copy_from_slice(a); Args(v) }

    #[test]
    fn read_into_owned_buffer_is_allowed() {
        assert!(check(&mem(), 0, &args(&[3, RW, 4096])));
    }

    #[test]
    fn read_is_checked_only_as_far_as_the_descriptor_can_fill() {
        // A 1 MiB buffer whose tail is unmapped: refused for a file (the whole length may be
        // written), accepted for a pipe whose ring is 4 KiB — the kernel never writes past it.
        // The cap never grows the extent, and recvfrom (45) is capped the same way.
        let mut m = mem();
        assert!(!check(&m, 0, &args(&[3, RW, 1 << 20])));
        assert!(!check(&m, 45, &args(&[3, RW, 1 << 20])));
        m.cap = Some(4096);
        assert!(check(&m, 0, &args(&[3, RW, 1 << 20])));
        assert!(check(&m, 45, &args(&[3, RW, 1 << 20])));
        assert!(!check(&m, 0, &args(&[3, RW + 0x10000 - 0x800, 4096]))); // the capped extent still runs off
        assert!(!check(&m, 17, &args(&[3, RW, 1 << 20])));                // pread64: a file, uncapped
        m.cap = Some(1 << 30);
        assert!(!check(&m, 0, &args(&[3, RW, 1 << 20])));
    }

    #[test]
    fn read_into_kernel_is_refused() {
        assert!(!check(&mem(), 0, &args(&[3, KERNEL, 16])));
    }

    #[test]
    fn read_into_read_only_page_is_refused_write_from_it_allowed() {
        let m = mem();
        assert!(!check(&m, 0, &args(&[3, RO, 16])));
        assert!(check(&m, 1, &args(&[1, RO, 16])));
    }

    #[test]
    fn buffer_running_off_the_mapping_is_refused() {
        let m = mem();
        assert!(check(&m, 0, &args(&[3, RW + 0x10000 - 16, 16])));
        assert!(!check(&m, 0, &args(&[3, RW + 0x10000 - 16, 17])));
    }

    #[test]
    fn length_overflow_is_refused() {
        assert!(!check(&mem(), 0, &args(&[3, RW, u64::MAX])));
        assert!(!check(&mem(), 7, &args(&[RW, u64::MAX / 4])));  // poll: nfds * 8 overflows
    }

    #[test]
    fn null_with_zero_length_is_allowed_null_with_length_is_not() {
        assert!(check(&mem(), 0, &args(&[3, 0, 0])));
        assert!(!check(&mem(), 0, &args(&[3, 0, 8])));
    }

    #[test]
    fn optional_pointers_may_be_null() {
        // rt_sigaction(sig, NULL, NULL); nanosleep(req, NULL)
        assert!(check(&mem(), 13, &args(&[2, 0, 0, 8])));
        assert!(check(&mem(), 35, &args(&[RW, 0])));
        assert!(!check(&mem(), 35, &args(&[0, 0])));
    }

    #[test]
    fn fixed_size_structures_are_checked_whole() {
        let m = mem();
        // fstat writes 144 bytes: a buffer 100 bytes before the end is refused
        assert!(!check(&m, 5, &args(&[1, RW + 0x10000 - 100])));
        assert!(check(&m, 5, &args(&[1, RW + 0x10000 - 144])));
    }

    #[test]
    fn string_must_end_inside_readable_memory() {
        let m = mem();
        m.put(RW, b"/etc/hosts\0");
        assert!(check(&m, 2, &args(&[RW])));
        // no NUL up to the end of the mapping
        let end = RW + 0x10000 - 4;
        m.put(end, b"abcd");
        assert!(!check(&m, 2, &args(&[end])));
        // NUL right at the last byte
        m.put(end, b"abc\0");
        assert!(check(&m, 2, &args(&[end])));
        assert!(!check(&m, 2, &args(&[KERNEL])));
    }

    #[test]
    fn string_longer_than_path_max_is_refused() {
        let m = mem();
        m.put(RW, &[b'a'; 5000]);
        assert!(!str_ok(&m, RW));
    }

    #[test]
    fn iovec_buffers_are_checked_individually() {
        let m = mem();
        let iov = RW + 0x100;
        m.put_u64(iov, RW + 0x1000); m.put_u64(iov + 8, 100);
        m.put_u64(iov + 16, KERNEL); m.put_u64(iov + 24, 100);
        assert!(check(&m, 20, &args(&[1, iov, 1])));   // writev, first buffer only
        assert!(!check(&m, 20, &args(&[1, iov, 2])));  // second points into the kernel
        m.put_u64(iov + 16, RO);
        assert!(check(&m, 20, &args(&[1, iov, 2])));   // writev may read a read-only page
        assert!(!check(&m, 19, &args(&[1, iov, 2])));  // readv may not write it
        assert!(!check(&m, 20, &args(&[1, KERNEL, 1]))); // the array itself in the kernel
    }

    #[test]
    fn msghdr_name_and_iov_are_checked() {
        let m = mem();
        let msg = RW + 0x200; let iov = RW + 0x300;
        m.put_u64(msg, 0); m.put_u64(msg + 16, iov); m.put_u64(msg + 24, 1);
        m.put_u64(iov, RW + 0x2000); m.put_u64(iov + 8, 64);
        assert!(check(&m, 47, &args(&[3, msg, 0])));
        m.put_u64(msg, KERNEL);
        assert!(!check(&m, 47, &args(&[3, msg, 0])));
    }

    #[test]
    fn futex_checks_uaddr_and_op_dependent_arguments() {
        let m = mem();
        assert!(check(&m, 202, &args(&[RW, 0, 0, 0])));              // WAIT, no timeout
        assert!(!check(&m, 202, &args(&[KERNEL, 1, 1])));            // WAKE on a kernel address
        assert!(!check(&m, 202, &args(&[RW, 0, 0, KERNEL])));        // WAIT with a bad timeout
        assert!(check(&m, 202, &args(&[RW, 1, 1, KERNEL])));         // WAKE: arg 4 is not a pointer
        assert!(!check(&m, 202, &args(&[RW, 5, 1, 1, RO])));         // WAKE_OP writes *uaddr2
        assert!(check(&m, 202, &args(&[RW, 128, 0, 0])));            // FUTEX_PRIVATE_FLAG is masked
    }

    #[test]
    fn select_checks_word_sized_fd_sets() {
        let m = mem();
        let end = RW + 0x10000;
        assert!(check(&m, 23, &args(&[64, end - 8, 0, 0, 0])));
        assert!(!check(&m, 23, &args(&[65, end - 8, 0, 0, 0])));      // second word needed
    }

    #[test]
    fn getsockopt_length_comes_from_memory() {
        let m = mem();
        let lenp = RW + 0x10;
        m.put(lenp, &200u32.to_le_bytes());
        assert!(check(&m, 55, &args(&[3, 1, 1, RW + 0x10000 - 200, lenp])));
        assert!(!check(&m, 55, &args(&[3, 1, 1, RW + 0x10000 - 100, lenp])));
    }

    #[test]
    fn execve_vectors_are_walked() {
        let m = mem();
        m.put(RW, b"/bin/sh\0");
        let argv = RW + 0x100;
        m.put_u64(argv, RW); m.put_u64(argv + 8, 0);
        assert!(check(&m, 59, &args(&[RW, argv, 0])));
        m.put_u64(argv + 8, KERNEL); m.put_u64(argv + 16, 0);
        assert!(!check(&m, 59, &args(&[RW, argv, 0])));
    }

    #[test]
    fn clone_tid_pointers_only_with_their_flags() {
        let m = mem();
        assert!(check(&m, 56, &args(&[0x3d0f00 & !0x0130_0000, 0, KERNEL, KERNEL])));
        assert!(!check(&m, 56, &args(&[0x0010_0000, 0, KERNEL, 0])));
        assert!(!check(&m, 56, &args(&[0x0020_0000, 0, 0, KERNEL])));
    }

    #[test]
    fn prctl_and_arch_prctl_depend_on_the_option() {
        let m = mem();
        assert!(!check(&m, 157, &args(&[16, KERNEL])));  // PR_GET_NAME
        assert!(check(&m, 157, &args(&[38, KERNEL])));   // unrelated option: integer arg
        assert!(!check(&m, 158, &args(&[0x1003, KERNEL]))); // ARCH_GET_FS
        assert!(check(&m, 158, &args(&[0x1002, KERNEL])));  // ARCH_SET_FS: value, not pointer
    }

    #[test]
    fn syscalls_without_pointers_pass_untouched() {
        for nr in [3u64, 8, 9, 10, 11, 12, 24, 32, 33, 39, 41, 50, 60, 62, 186, 231] {
            assert!(check(&mem(), nr, &args(&[KERNEL, KERNEL, KERNEL, KERNEL, KERNEL, KERNEL])), "nr {nr}");
        }
    }

    #[test]
    fn table_uses_valid_argument_numbers() {
        for nr in 0..5000u64 {
            for p in sig(nr) {
                assert!((1..=6).contains(&p.arg), "nr {nr}: arg {}", p.arg);
                match p.len {
                    Len::Arg(i) | Len::ArgCap(i) | Len::ArgMul(i, _) => {
                        assert!((1..=6).contains(&i) && i != p.arg, "nr {nr}: len arg {i}")
                    }
                    Len::Fixed(n) => assert!(n > 0, "nr {nr}: zero size"),
                    Len::Str => {}
                }
            }
        }
    }
}
