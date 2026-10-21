//! The kernel's per-process state for multi-process workloads (PostgreSQL and the like):
//! descriptor tables (a user fd -> an internal handle, a refcount on a handle, inheritance
//! at a fork, dup/dup2), real pipes, signals (rt_sigaction/sigprocmask/kill, pending
//! per-task), the current directory (cwd) and the normalisation of paths. A process = the scheduler's
//! leader task (sched::proc_of); the threads share the leader's tables.
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

pub const NT: usize = crate::sched::NTASKS;
pub const FDN: usize = 256;               // user fds per process (the kernel's memory < 32 MB)
const HMAX: usize = 4096;                 // the space of the internal handles
pub const AT_FDCWD: i64 = -100;

// ── the fd tables ───────────────────────────────────────────────────────────────
static mut FDT: [[i32; FDN]; NT] = [[-1; FDN]; NT];
static mut FDFL: [[u8; FDN]; NT] = [[0; FDN]; NT];   // FD_CLOEXEC
static mut HREF: [u16; HMAX] = [0; HMAX];            // the references to a handle (from all the tables)
static mut INITED: [bool; NT] = [false; NT];
static mut MMAP_NEXT: [u64; NT] = [0; NT];           // the bump pointer of the private mmap region (file/FIXED-compatible mappings)
const MMAP_BASE: u64 = 0x6000_0000_0000;
/// Hand out a range of virtual addresses for an mmap without identity (ld.so, file mappings).
pub fn mmap_alloc(len: u64) -> u64 {
    let p = cur_proc(); ensure(p);
    // Large mappings we hand out ALIGNED TO 2 MB. Without that the kernel has nowhere to
    // take a huge page from: a 2M leaf requires both the virtual and the physical address to be
    // multiples of 2 MB, while the bump pointer stops being a multiple after the very first mmap.
    let want = (len + 0xFFF) & !0xFFF;
    let align: u64 = if want >= 0x20_0000 { 0x20_0000 } else { 0x1000 };
    unsafe {
        let slot = &*(core::ptr::addr_of_mut!(MMAP_NEXT[p]) as *mut core::sync::atomic::AtomicU64);
        let _ = slot.compare_exchange(0, MMAP_BASE, core::sync::atomic::Ordering::SeqCst, core::sync::atomic::Ordering::SeqCst);
        loop {
            let cur = slot.load(core::sync::atomic::Ordering::SeqCst);
            let base = (cur + align - 1) & !(align - 1);
            let next = base + want + 0x1000; // a separator page between the mappings
            if slot.compare_exchange(cur, next, core::sync::atomic::Ordering::SeqCst, core::sync::atomic::Ordering::SeqCst).is_ok() {
                return base;
            }
        }
    }
}

#[inline] fn cur_proc() -> usize { crate::sched::proc_of(crate::sched::current_id()) }
fn fdt(p: usize) -> &'static mut [i32; FDN] { unsafe { &mut *(addr_of_mut!(FDT) as *mut [i32; FDN]).add(p) } }

/// Initialise the table of the process p: stdin/stdout/stderr -> the handles 0/1/2.
pub fn init_proc(p: usize) {
    let t = fdt(p);
    for i in 0..FDN { t[i] = -1; }
    t[0] = 0; t[1] = 1; t[2] = 2;
    unsafe {
        INITED[p] = true; NOTIFIED[p].store(false, Ordering::Relaxed);
        for s in 0..65 { SA_HANDLER[p][s] = 0; SA_FLAGS[p][s] = 0; SA_MASK[p][s] = 0; SA_RESTORER[p][s] = 0; }
        PPEND[p].store(0, Ordering::Relaxed);
        CWD[p][0] = b'/'; CWDL[p] = 1;
        MMAP_NEXT[p] = MMAP_BASE;
    }
}
fn ensure(p: usize) { if !unsafe { core::ptr::read_volatile(addr_of!(INITED[p])) } { init_proc(p); } }

/// execve: the image has been replaced, but the process is the same. The descriptors STAY open
/// (except FD_CLOEXEC), the signal handlers are reset to SIG_DFL (Linux),
/// the mask of the mmap region starts anew — the old AS has gone entirely.
pub fn exec_reset(p: usize) {
    ensure(p);
    let t = fdt(p);
    for i in 0..FDN {
        if t[i] >= 0 && unsafe { FDFL[p][i] } & 1 != 0 { let h = t[i]; t[i] = -1; href_dec_close(h as i64); }
    }
    unsafe {
        for s in 0..65 { SA_HANDLER[p][s] = 0; SA_FLAGS[p][s] = 0; SA_MASK[p][s] = 0; SA_RESTORER[p][s] = 0; }
        MMAP_NEXT[p] = MMAP_BASE;
    }
}
/// A reset of all the tables (a restart of the workload by the supervisor).
pub fn reset_all() {
    unsafe {
        for p in 0..NT { INITED[p] = false; PENDING[p].store(0, Ordering::Relaxed); PPEND[p].store(0, Ordering::Relaxed); SIGMASK[p] = 0; NOTIFIED[p].store(false, Ordering::Relaxed); }
        for h in 0..HMAX { HREF[h] = 0; }
        for i in 0..NPIPE { PR[i] = false; PW[i] = false; }
    }
}

/// A user fd -> a handle (the current process). None = EBADF.
pub fn get(ufd: i64) -> Option<i64> {
    if ufd < 0 || ufd as usize >= FDN { return None; }
    let p = cur_proc(); ensure(p);
    let h = fdt(p)[ufd as usize];
    if h < 0 { None } else { Some(h as i64) }
}
/// The translation for the syscalls: ufd -> a handle, or EBADF (-9) as a "handle" (the further
/// checks is_*(h) will not pass and will return EBADF).
pub fn tr(ufd: u64) -> i64 { get(ufd as i64).unwrap_or(-9) }
/// A cell of the descriptor table as an atomic: the table is ONE PER PROCESS, while the process's
/// threads run on different cores and open/close descriptors
/// simultaneously. Without a CAS two threads found one and the same free number and
/// both wrote their own handle into it: one of the two sockets was left without a
/// descriptor FOREVER (nobody will close it — the stack's slot leaked, the
/// server accumulated CLOSE-WAITs), while closing the second one returned EBADF.
/// It is caught by the gate `talaria.test=tickcmd`: three threads on the APs spin
/// socket()+close(), and without a CAS the eight UDP slots ran out within milliseconds.
fn slot(p: usize, i: usize) -> &'static core::sync::atomic::AtomicI32 {
    unsafe { &*(addr_of_mut!(FDT) as *mut i32).add(p * FDN + i).cast::<core::sync::atomic::AtomicI32>() }
}
/// Register a handle in the current process's table -> a user fd (the smallest free one >= min).
pub fn install_min(h: i64, min: usize) -> i64 {
    if h < 0 { return h; }
    let p = cur_proc(); ensure(p);
    use core::sync::atomic::Ordering::*;
    for i in min..FDN {
        if slot(p, i).compare_exchange(-1, h as i32, AcqRel, Relaxed).is_ok() {
            unsafe { FDFL[p][i] = 0; } href_inc(h); return i as i64;
        }
    }
    -24 // EMFILE
}
pub fn install(h: i64) -> i64 { install_min(h, 0) }
/// Put a handle into a SPECIFIC ufd (dup2): the previous one is closed. It returns the ufd.
pub fn install_at(h: i64, ufd: i64) -> i64 {
    if ufd < 0 || ufd as usize >= FDN { return -9; }
    let p = cur_proc(); ensure(p);
    let t = fdt(p);
    let old = t[ufd as usize];
    if old == h as i32 { return ufd; }
    if old >= 0 { t[ufd as usize] = -1; href_dec_close(old as i64); }
    t[ufd as usize] = h as i32; unsafe { FDFL[p][ufd as usize] = 0; } href_inc(h);
    ufd
}
/// close(ufd): remove the entry; at the last reference — really close the handle.
pub fn close(ufd: i64) -> i64 {
    if ufd < 0 || ufd as usize >= FDN { return -9; }
    let p = cur_proc(); ensure(p);
    // a swap rather than "read and zero out": two close()s of the same number from different
    // cores would have closed the handle twice.
    let h = slot(p, ufd as usize).swap(-1, core::sync::atomic::Ordering::AcqRel);
    if h < 0 { return -9; }
    href_dec_close(h as i64);
    0
}
/// The reference counter of a handle is an atomic for the same reason as a cell of the table:
/// forked processes and the threads of one process close a SHARED handle from different
/// cores, and "read, subtract, write" lost a reference.
fn href(h: usize) -> &'static core::sync::atomic::AtomicU16 {
    unsafe { &*(addr_of_mut!(HREF) as *mut u16).add(h).cast::<core::sync::atomic::AtomicU16>() }
}
fn href_inc(h: i64) {
    if h >= 3 && (h as usize) < HMAX {
        let n = href(h as usize).fetch_add(1, Ordering::AcqRel) + 1;
        if TRACE_REF.load(core::sync::atomic::Ordering::Relaxed) && (100..200).contains(&h) {
            crate::println!("href: +1 socket h={} -> {} (the task {})", h, n, crate::sched::current_id());
        }
    }
}
/// `talaria.trefs=1` — print the accounting of the references to SOCKET descriptors.
pub static TRACE_REF: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
fn href_dec_close(h: i64) {
    if h < 3 || h as usize >= HMAX { return; }
    // The last reference closes the handle EXACTLY ONCE: whoever decremented
    // the counter to zero closes it; at r == 0 (the accounting was not kept — the handle
    // came past install) we close it as before.
    let a = href(h as usize);
    let r = a.load(Ordering::Acquire);
    if TRACE_REF.load(core::sync::atomic::Ordering::Relaxed) && (100..200).contains(&h) {
        crate::println!("href: -1 socket h={} it was {} (the task {})", h, r, crate::sched::current_id());
    }
    if r == 0 { crate::syscall::close_handle(h); return; }
    if a.fetch_sub(1, Ordering::AcqRel) <= 1 { a.store(0, Ordering::Release); crate::syscall::close_handle(h); }
    else {
        // THERE ARE STILL REFERENCES, BUT THIS TASK NO LONGER HAS THE DESCRIPTOR —
        // so it is not a waiter for this socket either (see `netsys::disown`).
        crate::netsys::disown(h, crate::sched::current_id() as u8);
    }
}
pub fn fd_get_cloexec(ufd: i64) -> i64 { let p = cur_proc(); if ufd < 0 || ufd as usize >= FDN { return -9; } unsafe { FDFL[p][ufd as usize] as i64 } }
pub fn fd_set_cloexec(ufd: i64, v: i64) -> i64 { let p = cur_proc(); if ufd < 0 || ufd as usize >= FDN { return -9; } unsafe {
    FDFL[p][ufd as usize] = (v & 1) as u8; } 0 }
/// fork: the child inherits the parent's table (the references +1), the signal handlers, the mask, the cwd.
pub fn fork_into(parent: usize, child: usize) {
    ensure(parent);
    let pt = fdt(parent);
    let ct = fdt(child);
    for i in 0..FDN { ct[i] = pt[i]; if pt[i] >= 0 { href_inc(pt[i] as i64); } }
    unsafe {
        for i in 0..FDN { FDFL[child][i] = FDFL[parent][i]; }
        INITED[child] = true;
        for s in 0..65 { SA_HANDLER[child][s] = SA_HANDLER[parent][s]; SA_FLAGS[child][s] = SA_FLAGS[parent][s]; SA_MASK[child][s] = SA_MASK[parent][s];
            SA_RESTORER[child][s] = SA_RESTORER[parent][s]; }
        SIGMASK[child] = SIGMASK[parent]; PENDING[child].store(0, Ordering::Relaxed); PPEND[child].store(0, Ordering::Relaxed);
        NOTIFIED[child].store(false, Ordering::Relaxed);
        CWD[child] = CWD[parent]; CWDL[child] = CWDL[parent];
        MMAP_NEXT[child] = MMAP_NEXT[parent];
    }
}
/// The end of a process: close all its descriptors (the references), notify the parent with a SIGCHLD.
pub fn exit_cleanup(p: usize) {
    if !unsafe { INITED[p] } { if p < NT { NOTIFIED[p].store(true, Ordering::Release); } return; }
    let t = fdt(p);
    for i in 0..FDN { let h = t[i]; if h >= 0 { t[i] = -1; href_dec_close(h as i64); } }
    let pp = crate::sched::ppid_of(p);
    if pp >= 0 { send_proc(pp as usize, 17); } // a SIGCHLD to the parent (the process)
    NOTIFIED[p].store(true, Ordering::Release);
}
/// The process HAS FINISHED FINISHING: the threads are stopped, the files are closed, the parent has been notified with a SIGCHLD.
/// Only after that is `wait4` entitled to give out its status: otherwise the parent,
/// having got the core while the dying process waits for its threads to stop or
/// flushes the FS cache, saw a zombie before the SIGCHLD (sigtest: "SIGCHLD delivered
/// to parent" failed exactly like that).
///
/// Release on the set, Acquire on the read: `wait4` on the parent's core must see what the
/// exit did before it (the descriptors closed, the SIGCHLD bit) — on arm64 a plain flag may
/// be seen ahead of them.
static NOTIFIED: [AtomicBool; NT] = [const { AtomicBool::new(false) }; NT];
pub fn exit_notified(p: usize) -> bool { p < NT && NOTIFIED[p].load(Ordering::Acquire) }

// ── pipe ────────────────────────────────────────────────────────────────────────
const NPIPE: usize = 32;
const PBUF: usize = 4096;
pub const PIPE_BASE: i64 = 2400;
static mut PB: [[u8; PBUF]; NPIPE] = [[0; PBUF]; NPIPE];
/// The ring is single-producer/single-consumer with an index per side: the READER alone
/// moves `PHEAD` (bytes taken since creation), the WRITER alone moves `PTAIL` (bytes put),
/// the count is their difference. Each publishes with Release after its bytes and reads the
/// other's with Acquire before touching the buffer. It used to be one count (`PCNT`) both
/// ends read, changed and wrote back with plain stores: on two cores one end's update was
/// lost (a reader's `c - n` over a writer's `c + n` — bytes vanished or were read twice),
/// and on arm64 the reader could see the count before the bytes.
static PHEAD: [AtomicUsize; NPIPE] = [const { AtomicUsize::new(0) }; NPIPE];
static PTAIL: [AtomicUsize; NPIPE] = [const { AtomicUsize::new(0) }; NPIPE];
/// One reader and one writer at a time per pipe (threads, or a forked child, may share an
/// end): a spin lock per END, so that the two ends never wait for each other.
static PLOCK: [[AtomicBool; 2]; NPIPE] = [const { [const { AtomicBool::new(false) }; 2] }; NPIPE];
fn pipe_lock(i: usize, e: usize) {
    while PLOCK[i][e].compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() { core::hint::spin_loop(); }
}
fn pipe_unlock(i: usize, e: usize) { PLOCK[i][e].store(false, Ordering::Release); }
/// Bytes in the pipe as this side sees them.
fn pipe_count(i: usize) -> usize {
    PTAIL[i].load(Ordering::Acquire).wrapping_sub(PHEAD[i].load(Ordering::Acquire))
}
static mut PR: [bool; NPIPE] = [false; NPIPE];  // the reading end is open
static mut PW: [bool; NPIPE] = [false; NPIPE];  // the writing end is open
static mut PNB: [[bool; 2]; NPIPE] = [[false; 2]; NPIPE];
/// THE PIPE'S WAKE-UP WORD: it grows on every write, read and closing of an end.
///
/// Why. A read from an empty pipe POLLED it once every 200 us
/// (`syscall::wait_window`), that is five thousand wake-ups a second per
/// every blocked reader. In ClickHouse a signal-watchdog thread sits on such a read,
/// and `top` showed it FIRST by the consumption of the core: 20-69 %
/// at ZERO syscalls a second and one and a half thousand switches. It was possible to recognise it
/// only by the number of the last syscall (0 = `read`) — it has no name,
/// and that is why a column with that number appeared in `top` in the same go.
///
/// Now the reader SLEEPS on this word, and the writer wakes it. The word lives in the
/// KERNEL, so the wake-up goes without a selection by process
/// (`sync::wake_addr_kernel`): a pipe's ends happen to be in different processes —
/// `sh: a | b` — and a selection by the leader would have left the reader asleep.
static PSEQ: [core::sync::atomic::AtomicU32; NPIPE] =
    [const { core::sync::atomic::AtomicU32::new(0) }; NPIPE];
/// An event on the pipe i: move the word and wake both sides.
fn pipe_bump(i: usize) {
    PSEQ[i].fetch_add(1, core::sync::atomic::Ordering::SeqCst);
    crate::sync::wake_addr_kernel(crate::mem::vaddr(PSEQ[i].as_ptr()), usize::MAX);
    crate::netsys::mix_kick(); // a pipe in an epoll/poll set (`talaria.netmix=`)
}
pub fn pipe_is(h: i64) -> bool { h >= PIPE_BASE && ((h - PIPE_BASE) as usize) < NPIPE * 2 && unsafe { let i = ((h - PIPE_BASE) / 2) as usize; PR[i] || PW[i] } }
fn pidx(h: i64) -> (usize, usize) { let k = (h - PIPE_BASE) as usize; (k / 2, k % 2) }
pub fn pipe_create(nonblock: bool) -> Option<(i64, i64)> {
    unsafe {
        for i in 0..NPIPE {
            if !PR[i] && !PW[i] {
                PR[i] = true; PW[i] = true; PNB[i] = [nonblock; 2];
                PHEAD[i].store(0, Ordering::Relaxed); PTAIL[i].store(0, Ordering::Relaxed);
                return Some((PIPE_BASE + 2 * i as i64, PIPE_BASE + 2 * i as i64 + 1));
            }
        }
    }
    None
}
pub fn pipe_close(h: i64) {
    let (i, e) = pidx(h);
    // Release: a reader that sees the end closed must also see the bytes written before
    // (it re-reads `PTAIL` behind an Acquire fence — `pipe_read`).
    core::sync::atomic::fence(Ordering::Release);
    unsafe { if e == 0 { core::ptr::write_volatile(addr_of_mut!(PR[i]), false); } else { core::ptr::write_volatile(addr_of_mut!(PW[i]), false); } }
    pipe_bump(i); // an EOF/EPIPE is an event too: a waiter is bound to wake up at once
}
pub fn pipe_set_nonblock(h: i64, on: bool) { let (i, e) = pidx(h); unsafe { PNB[i][e] = on; } }
pub fn pipe_get_nonblock(h: i64) -> bool { let (i, e) = pidx(h); unsafe { PNB[i][e] } }
pub fn pipe_read(h: i64, buf: *mut u8, len: usize) -> i64 {
    let (i, e) = pidx(h);
    if e != 0 { return -9; }
    if len == 0 { return 0; } // as Linux: nothing asked, nothing to wait for (an empty non-blocking pipe said EAGAIN)
    loop {
        // The snapshot of the sequence is taken BEFORE the check of the data: a write
        // that came between the check and the falling asleep is then not lost — the word is already
        // not equal to the snapshot, and the waiter will not fall asleep at all.
        let seq = PSEQ[i].load(core::sync::atomic::Ordering::SeqCst);
        pipe_lock(i, 0);
        let hd = PHEAD[i].load(Ordering::Relaxed);
        let c = PTAIL[i].load(Ordering::Acquire).wrapping_sub(hd);
        if c > 0 {
            let n = c.min(len);
            // SAFETY: the bytes [hd, hd+c) are the writer's, published by its Release on
            // PTAIL; `buf` was validated by `uaccess::gate`.
            unsafe { for k in 0..n { *buf.add(k) = PB[i][(hd + k) % PBUF]; } }
            PHEAD[i].store(hd.wrapping_add(n), Ordering::Release); // the room, after the bytes are read
            pipe_unlock(i, 0);
            pipe_bump(i);                      // room has been freed — for the writer
            return n as i64;
        }
        pipe_unlock(i, 0);
        unsafe {
            if !core::ptr::read_volatile(addr_of!(PW[i])) {
                // The writer may have put its last bytes and closed on another core: what
                // it wrote before the close is visible after this fence.
                core::sync::atomic::fence(Ordering::Acquire);
                if PTAIL[i].load(Ordering::Acquire) != hd { continue; }
                return 0; // EOF
            }
            if PNB[i][0] { return -11; }
        }
        if sig_pending() { return -4; }
        crate::sync::wait_on(PSEQ[i].as_ptr(), seq, Some(crate::tsc::mono_ns() + PIPE_BACKSTOP_NS));
    }
}
/// The insurance deadline of a sleep on a pipe: a lost wake-up must not
/// turn into an eternity. Fifty milliseconds is four orders of magnitude rarer than
/// the former polling (200 us) and an order of magnitude cheaper than any mistake.
const PIPE_BACKSTOP_NS: u64 = 50_000_000;
/// The capacity of a pipe's buffer (F_GETPIPE_SZ/F_SETPIPE_SZ): ours is fixed.
pub fn pipe_capacity() -> usize { PBUF }

pub fn pipe_write(h: i64, buf: *const u8, len: usize) -> i64 {
    let (i, e) = pidx(h);
    if e != 1 { return -9; }
    let mut done = 0usize;
    loop {
        unsafe {
            if !core::ptr::read_volatile(addr_of!(PR[i])) { return if done > 0 { done as i64 } else { -32 }; } // EPIPE
            pipe_lock(i, 1);
            let tl = PTAIL[i].load(Ordering::Relaxed);
            let space = PBUF - tl.wrapping_sub(PHEAD[i].load(Ordering::Acquire));
            if space > 0 {
                let n = space.min(len - done);
                for k in 0..n { PB[i][(tl + k) % PBUF] = *buf.add(done + k); }
                PTAIL[i].store(tl.wrapping_add(n), Ordering::Release); // the bytes, then the index
                pipe_unlock(i, 1);
                done += n;
                if n > 0 { pipe_bump(i); }         // data has appeared — for the reader
                if done >= len { return done as i64; }
            } else {
                pipe_unlock(i, 1);
                if PNB[i][1] { return if done > 0 { done as i64 } else { -11 }; }
            }
        }
        if sig_pending() { return if done > 0 { done as i64 } else { -4 }; }
        let seq = PSEQ[i].load(core::sync::atomic::Ordering::SeqCst);
        crate::sync::wait_on(PSEQ[i].as_ptr(), seq, Some(crate::tsc::mono_ns() + PIPE_BACKSTOP_NS));
    }
}
/// The readiness for epoll/poll (EPOLLIN=1, EPOLLOUT=4, EPOLLERR=8, EPOLLHUP=0x10).
pub fn pipe_readiness(h: i64) -> u32 {
    let (i, e) = pidx(h);
    unsafe {
        if e == 0 { let mut m = 0; if pipe_count(i) > 0 { m |= 1; } if !PW[i] { m |= 1 | 0x10; } m }
        else { let mut m = 0; if pipe_count(i) < PBUF { m |= 4; } if !PR[i] { m |= 8; } m }
    }
}

// ── the signals ─────────────────────────────────────────────────────────────────
static mut SA_HANDLER: [[u64; 65]; NT] = [[0; 65]; NT];
static mut SA_FLAGS: [[u64; 65]; NT] = [[0; 65]; NT];
static mut SA_RESTORER: [[u64; 65]; NT] = [[0; 65]; NT];
static mut SA_MASK: [[u64; 65]; NT] = [[0; 65]; NT];
// The pending sets are set from any core (a `kill` on one, the target's `take` on another):
// an atomic OR/AND, Release on the send and Acquire on the take, so that two senders, or a
// send racing a take, lose no signal (the plain read-or-write of before did, on both machines).
static PENDING: [AtomicU64; NT] = [const { AtomicU64::new(0) }; NT];   // per-task (tkill/tgkill)
// per-process (kill/SIGCHLD): the leader's index; it is taken by any thread for
// which the signal is not masked (sigwait and the like)
static PPEND: [AtomicU64; NT] = [const { AtomicU64::new(0) }; NT];
static mut SIGMASK: [u64; NT] = [0; NT];   // per-task

/// rt_sigaction(sig, act*, oldact*): kernel struct {handler, flags, restorer, mask}.
pub fn sigaction(sig: u64, act: u64, old: u64) -> i64 {
    if sig == 0 || sig > 64 { return -22; }
    let p = cur_proc(); ensure(p);
    let s = sig as usize;
    unsafe {
        if old != 0 {
            core::ptr::write_unaligned(crate::uaccess::uptr::<u64>(old), SA_HANDLER[p][s]);
            core::ptr::write_unaligned(crate::uaccess::uptr::<u64>(old + 8), SA_FLAGS[p][s]);
            core::ptr::write_unaligned(crate::uaccess::uptr::<u64>(old + 16), SA_RESTORER[p][s]);
            core::ptr::write_unaligned(crate::uaccess::uptr::<u64>(old + 24), SA_MASK[p][s]);
        }
        if act != 0 {
            if sig == 9 || sig == 19 { return -22; } // SIGKILL/SIGSTOP
            SA_HANDLER[p][s] = core::ptr::read_unaligned(crate::uaccess::uptr::<u64>(act));
            SA_FLAGS[p][s] = core::ptr::read_unaligned(crate::uaccess::uptr::<u64>(act + 8));
            SA_RESTORER[p][s] = core::ptr::read_unaligned(crate::uaccess::uptr::<u64>(act + 16));
            SA_MASK[p][s] = core::ptr::read_unaligned(crate::uaccess::uptr::<u64>(act + 24));
        }
    }
    0
}
/// rt_sigprocmask(how, set*, old*).
pub fn sigprocmask(how: u64, set: u64, old: u64) -> i64 {
    let t = crate::sched::current_id();
    unsafe {
        let cur = SIGMASK[t];
        if old != 0 { core::ptr::write_unaligned(crate::uaccess::uptr::<u64>(old), cur); }
        if set != 0 {
            let m = core::ptr::read_unaligned(crate::uaccess::uptr::<u64>(set)) & !((1u64 << 8) | (1u64 << 18)); // SIGKILL/SIGSTOP are not blocked
            SIGMASK[t] = match how { 0 => cur | m, 1 => cur & !m, 2 => m, _ => return -22 };
        }
    }
    0
}
pub fn sigmask_of(t: usize) -> u64 { unsafe { SIGMASK[t] } }
// ── rt_sigsuspend: the mask is TEMPORARY ────────────────────────────────────────
// It used to be ignored: the syscall waited under the CURRENT mask, so the textbook
// `sigprocmask(BLOCK, CHLD); fork(); ... sigsuspend(&unblocked)` never saw its SIGCHLD and looped
// on EINTR for good. Now the given mask is installed for the wait, the old one is kept here and
// goes into the signal frame's `uc_sigmask` (rt_sigreturn restores it, as on Linux), or is put
// back at the syscall's return when no handler ran.
static mut SUSP_SAVED: [u64; NT] = [0; NT];
static mut SUSP_ON: [bool; NT] = [false; NT];
/// Install `rt_sigsuspend`'s mask (read from ring 3 at `set`) for the current thread.
pub fn sigsuspend_begin(set: u64) {
    let t = crate::sched::current_id();
    // SAFETY: the thread's own rows; `set` was validated by `uaccess::gate` (rt_sigsuspend, rd 8).
    unsafe {
        let m = core::ptr::read_unaligned(crate::uaccess::uptr::<u64>(set)) & !((1u64 << 8) | (1u64 << 18));
        SUSP_SAVED[t] = SIGMASK[t]; SUSP_ON[t] = true; SIGMASK[t] = m;
    }
}
/// The mask `rt_sigsuspend` replaced, once (None: the thread is not in one).
pub fn sigsuspend_take(t: usize) -> Option<u64> {
    // SAFETY: the thread's own rows.
    unsafe { if SUSP_ON[t] { SUSP_ON[t] = false; Some(SUSP_SAVED[t]) } else { None } }
}
/// clone(CLONE_VM): the thread inherits the creator's mask, it has no pending signals.
pub fn thread_inherit_sigmask(parent: usize, child: usize) { unsafe { SIGMASK[child] = SIGMASK[parent]; } PENDING[child].store(0, Ordering::Relaxed); }
pub fn set_sigmask(t: usize, m: u64) { unsafe { SIGMASK[t] = m; } }
/// Is the signal IGNORED by the process `p` for a recipient with the mask `mask` — Linux's
/// `sig_ignored`: the disposition is SIG_IGN, or SIG_DFL for a signal whose default is to
/// do nothing (SIGCHLD, SIGURG, SIGWINCH), and the recipient has not blocked it (a blocked
/// one stays pending: `sigwait`, `signalfd` and a later handler are entitled to it).
///
/// Such a signal is DISCARDED WHEN IT IS SENT. Left pending, it interrupted the very wait
/// it announced: a child's SIGCHLD reaches the parent a moment before the child has left
/// its core (`sched::reap` still says "dying"), and a `wait4` that looked in between
/// returned EINTR for a signal nobody handles. x86 under KVM never lost that race; arm64
/// under TCG lost it 30 times of 40 (`crashme forkap`).
fn sig_ignored(p: usize, sig: u64, mask: u64) -> bool {
    if mask & (1u64 << (sig - 1)) != 0 { return false; }
    // SAFETY: a read of the process's disposition table (one word, written by `sigaction`).
    let h = unsafe { core::ptr::read_volatile(addr_of!(SA_HANDLER[p][sig as usize])) };
    h == 1 || (h == 0 && matches!(sig, 17 | 23 | 28))
}
/// Put a signal to the task t (pending).
pub fn send(t: usize, sig: u64) { if t < NT && (1..=64).contains(&sig) {
    if sig_ignored(crate::sched::proc_of(t), sig, sigmask_of(t)) { return; }
    PENDING[t].fetch_or(1u64 << (sig - 1), Ordering::Release);
    crate::netsys::mix_kick(); } } // a signalfd in an epoll set, a waiter's EINTR (`talaria.netmix=`)
/// A signal to a PROCESS (the leader p): any of its threads for which it is not masked will take it.
pub fn send_proc(p: usize, sig: u64) { if p < NT && (1..=64).contains(&sig) {
    if sig_ignored(p, sig, sigmask_of(p)) { return; }
    PPEND[p].fetch_or(1u64 << (sig - 1), Ordering::Release);
    crate::netsys::mix_kick(); } }
/// A SIGTERM to all the live processes (by their leaders) — the regular stop of the machine. It returns the number of processes.
pub fn term_all() -> usize {
    let mut n = 0;
    for t in 0..crate::sched::task_count().min(NT) {
        if crate::sched::task_alive(t) && crate::sched::proc_of(t) == t { send_proc(t, 15); n += 1; }
    }
    n
}
/// kill(pid, sig): pid = the tid+1 of the process's leader. sig=0 — a check of existence.
pub fn kill(pid: i64, sig: u64) -> i64 {
    if pid <= 0 { return 0; } // groups/everyone — we do not support them (ok)
    let t = (pid - 1) as usize;
    if t >= NT || !crate::sched::task_alive(t) { return -3; } // ESRCH
    if sig == 0 { return 0; }
    let p = crate::sched::proc_of(t);
    send_proc(p, sig);
    // THE SIGNAL'S TRACE NEXT TO THE SYSCALL'S TRACE. `kill` returns success having done
    // nothing exactly as easily as any other syscall: the recipient could have
    // turned out to be the wrong one (the task's number against the process's leader), and the signal —
    // masked. By a single "62(...) = 0" in the trace that is indistinguishable.
    if crate::syscall::strace_on() {
        crate::println!("[signal] kill(pid={}, {}) -> t{} the leader t{} the mask {:#x}",
            pid, sig, t, p, unsafe { SIGMASK[t] });
    }
    0
}
/// tgkill/tkill: a specific task.
pub fn tkill(tid: i64, sig: u64) -> i64 {
    if tid <= 0 { return -22; }
    let t = (tid - 1) as usize;
    if t >= NT || !crate::sched::task_alive(t) { return -3; }
    if sig != 0 { send(t, sig); }
    0
}
/// Whether the current task has a deliverable (not masked) signal.
///
/// The death of a thread (it was killed by an `exit_group`, by a sister's crash or by a restart) is counted
/// as a pending signal that cannot be masked — like a SIGKILL in Linux.
/// That way any interruptible wait (the network, `nanosleep`, `pause`, a futex) leaves
/// with an EINTR, the syscall ends, and the thread stops on the way out of it
/// (`syscall::stop_if_dead`), having released everything it held in the kernel.
pub fn sig_pending() -> bool {
    let t = crate::sched::current_id(); let p = crate::sched::proc_of(t);
    if !crate::sched::task_alive(t) { return true; }
    (PENDING[t].load(Ordering::Acquire) | PPEND[p].load(Ordering::Acquire)) & !unsafe { SIGMASK[t] } != 0
}
/// Whether the TASK t has a deliverable signal (for the scheduler: to wake one sleeping on a futex).
pub fn sig_pending_t(t: usize) -> bool {
    let p = crate::sched::proc_of(t);
    if !crate::sched::task_alive(t) { return true; }
    (PENDING[t].load(Ordering::Acquire) | PPEND[p].load(Ordering::Acquire)) & !unsafe { SIGMASK[t] } != 0
}
/// Take and return the lowest deliverable signal of the current task (None — there is none).
pub fn take_pending() -> Option<u64> { take_pending_in(!0u64 & !sigmask_of(crate::sched::current_id())) }
/// Whether there is a pending signal from the set `set` (signalfd readiness).
pub fn pending_in(set: u64) -> bool { let t = crate::sched::current_id(); let p = crate::sched::proc_of(t);
    (PENDING[t].load(Ordering::Acquire) | PPEND[p].load(Ordering::Acquire)) & set != 0 }
/// Take and return the lowest pending signal from the set `set` (sigtimedwait; the mask is not taken into account).
pub fn take_pending_in(set: u64) -> Option<u64> {
    let t = crate::sched::current_id(); let p = crate::sched::proc_of(t);
    // The bit is TAKEN by the atomic AND: a sibling thread taking from the same process set
    // at once gets the bit only if it was still there (the old value says), else looks again.
    for w in [&PENDING[t], &PPEND[p]] {
        loop {
            let v = w.load(Ordering::Acquire) & set;
            if v == 0 { break; }
            let b = 1u64 << v.trailing_zeros();
            if w.fetch_and(!b, Ordering::AcqRel) & b != 0 { return Some(b.trailing_zeros() as u64 + 1); }
        }
    }
    None
}
/// The parameters of the signal handler (handler, flags, restorer, mask) of the current process.
pub fn handler_of(sig: u64) -> (u64, u64, u64, u64) {
    let p = cur_proc(); let s = sig as usize;
    unsafe { (SA_HANDLER[p][s], SA_FLAGS[p][s], SA_RESTORER[p][s], SA_MASK[p][s]) }
}

// ── the cwd and the paths ───────────────────────────────────────────────────────
const CWDMAX: usize = 160;
static mut CWD: [[u8; CWDMAX]; NT] = [[0; CWDMAX]; NT];
static mut CWDL: [usize; NT] = [0; NT];
/// The buffer of a normalised path — PER CORE, two slots per core (rename: old+new).
///
/// It used to be one for the whole machine, and that quietly broke the file system on any
/// multithreaded workload. The parsing of a path string goes OUTSIDE the FS lock (the lock is taken
/// already inside ext2), and what is given outwards is a SLICE of this buffer, which lives until the end
/// of the syscall. So two cores opening files simultaneously wrote one and the same
/// string: the first managed to put `/tmp_1_0`, the second overwrote it with its own
/// `/tmp_3_0`, and then ext2 honestly, under the lock, did the operation ON A FOREIGN PATH.
///
/// From the outside that looked like anything at all, only not like a race for a buffer:
/// `mkdir` returns EEXIST on a name that does not exist (because it looked at a foreign
/// name); `open` — ENOTDIR on a directory; a file just written does not open;
/// after a `rename` files "disappear". In ClickHouse on a table of 10 million rows —
/// "Cannot open file s.cmrk2" and "Cannot read all data" in about half the
/// runs, while `e2fsck` is clean: the structure is intact, the operations were done not on
/// those files. On ONE core it never reproduces — the syscalls do not
/// overlap.
///
/// PER TASK RATHER THAN PER CORE — and that too is a measurement, not a taste.
///
/// At first the buffer was made per core: "a syscall goes with IF=0, a task can neither
/// migrate nor be pre-empted while it holds a slice". The invariant was true
/// exactly until a VOLUNTARY yield of the core appeared inside a syscall:
/// a waiter for the FS lock now does not burn the processor but gives it to a neighbour
/// (`ext2::fs_lock`, `sched::yield_now`), and the waiting tier does the same. An interrupt
/// is not needed for that — IF=0 does not forbid such a handover.
///
/// Then the old trouble repeated letter for letter: while a task stands in the queue for
/// the FS lock, a neighbour on THE SAME core enters its own `open`/`stat`, writes into the same
/// slot its own path, and the first one, having got the lock, honestly does the operation ON A FOREIGN
/// PATH. In ClickHouse on a table of 10 million rows that gave
/// "filesystem error: in file_size: No such file or directory" on the file of a piece just
/// created in about every third run, while `e2fsck` was clean:
/// the structure is intact, the wrong name was asked about. On one core it does not reproduce.
///
/// The price of the honest variant is 192x2x512 = 192 KB in the .bss against 64x2. Cheap for
/// an invariant that no longer depends on who and where decides to yield the core.
static mut PATHBUF: [[[u8; 512]; 2]; crate::sched::NTASKS] =
    [[[0; 512]; 2]; crate::sched::NTASKS];

/// Normalise: glue the cwd (for relative ones) and the path, remove ".", "..", "//".
/// The result is an absolute path without a trailing '/', in PATHBUF[slot]. It returns a slice.
pub fn resolve_slot(ptr: u64, slot: usize) -> &'static [u8] { resolve_base(None, ptr, slot) }
/// The *at variants: a relative path from the directory dirfd (a user fd) instead of the cwd; AT_FDCWD/an absolute one -> as in resolve.
pub fn resolve_at_slot(dirfd: i64, ptr: u64, slot: usize) -> &'static [u8] {
    let first = if ptr == 0 { 0 } else { unsafe { core::ptr::read_volatile(crate::uaccess::uptr::<u8>(ptr)) } };
    if dirfd == AT_FDCWD || first == b'/' { return resolve_base(None, ptr, slot); }
    let h = tr(dirfd as u64);
    let base = if h >= 0 && crate::ext2::active() { crate::ext2::fd_path(h) } else { b"" };
    if base.is_empty() { return resolve_base(None, ptr, slot); }
    let mut bb = [0u8; 160]; let l = base.len().min(160); bb[..l].copy_from_slice(&base[..l]);
    resolve_base(Some(&bb[..l]), ptr, slot)
}
pub fn resolve_at(dirfd: i64, ptr: u64) -> &'static [u8] { resolve_at_slot(dirfd, ptr, 0) }
fn resolve_base(base: Option<&[u8]>, ptr: u64, slot: usize) -> &'static [u8] {
    let p = cur_proc(); ensure(p);
    let out = unsafe {
        let t = crate::sched::current_id().min(crate::sched::NTASKS - 1);
        &mut *(addr_of_mut!(PATHBUF) as *mut [u8; 512]).add(t * 2 + (slot & 1))
    };
    let mut n = 0usize;
    let first = if ptr == 0 { 0 } else { unsafe { core::ptr::read_volatile(crate::uaccess::uptr::<u8>(ptr)) } };
    if first != b'/' { // relative: start from the base (the dirfd directory) or the cwd
        match base {
            Some(b) => { let l = b.len().min(400); out[..l].copy_from_slice(&b[..l]); n = l; }
            None => unsafe { let l = CWDL[p]; out[..l].copy_from_slice(&CWD[p][..l]); n = l; }
        }
        if n == 0 { out[0] = b'/'; n = 1; }
        if n > 1 || (n == 1 && out[0] != b'/') { out[n] = b'/'; n += 1; }
    }
    if n == 0 { out[0] = b'/'; n = 1; }
    // the components of the input
    let mut i = 0usize;
    loop {
        let c = if ptr == 0 { 0 } else { unsafe { core::ptr::read_volatile(crate::uaccess::uptr::<u8>(ptr).add(i)) } };
        if c == 0 || i > 400 { break; }
        if c == b'/' { i += 1; continue; }
        let s = i;
        while { let d = unsafe { core::ptr::read_volatile(crate::uaccess::uptr::<u8>(ptr).add(i)) }; d != 0 && d != b'/' && i < 400 } { i += 1; }
        let comp = unsafe { core::slice::from_raw_parts(crate::uaccess::uptr::<u8>(ptr).add(s), i - s) };
        if comp == b"." { continue; }
        if comp == b".." {
            // remove the last component
            while n > 1 && out[n - 1] == b'/' { n -= 1; }
            while n > 1 && out[n - 1] != b'/' { n -= 1; }
            while n > 1 && out[n - 1] == b'/' { n -= 1; }
            continue;
        }
        if n + comp.len() + 1 >= 500 { break; }
        if out[n - 1] != b'/' { out[n] = b'/'; n += 1; }
        out[n..n + comp.len()].copy_from_slice(comp); n += comp.len();
    }
    while n > 1 && out[n - 1] == b'/' { n -= 1; }
    &out[..n]
}
pub fn resolve(ptr: u64) -> &'static [u8] { resolve_slot(ptr, 0) }
/// chdir(path) — the path must be a directory (the caller checks that).
pub fn set_cwd(path: &[u8]) -> i64 {
    let p = cur_proc(); ensure(p);
    let n = path.len().min(CWDMAX - 1);
    unsafe { CWD[p][..n].copy_from_slice(&path[..n]); CWDL[p] = n; }
    0
}
pub fn getcwd(buf: u64, size: u64) -> i64 {
    let p = cur_proc(); ensure(p);
    let l = unsafe { CWDL[p] };
    if buf == 0 || (size as usize) < l + 1 { return -34; } // ERANGE
    unsafe { core::ptr::copy_nonoverlapping(CWD[p].as_ptr(), crate::uaccess::uptr::<u8>(buf), l); core::ptr::write(crate::uaccess::uptr::<u8>(buf).add(l), 0); }
    (l + 1) as i64
}
#[allow(dead_code)] fn _unused() { let _ = Ordering::Relaxed; }
