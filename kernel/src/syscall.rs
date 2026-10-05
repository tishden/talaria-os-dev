//! The Linux syscall ABI: SYSCALL/SYSRET (the MSRs LSTAR/STAR/FMASK/EFER.SCE) + a dispatcher
//! of the basic syscalls. A user binary does a `syscall`, the kernel executes it.
//!
//! # Safety
//!
//! Almost every `unsafe` in this file is the same act — the kernel touches the
//! PROCESS's memory at an address the process itself passed — so the invariants
//! are stated once here and named S1..S4 at the sites.
//!
//! - **S1. A pointer argument has already been validated.** `rust_syscall_inner`
//!   calls `uaccess::gate` BEFORE `dispatch`: every pointer argument of the call,
//!   with the length the kernel is going to touch, has been checked against the
//!   process's own page tables (`paging::user_range_ok`), and a failed check
//!   returned `-EFAULT` without reaching the handler. So inside `dispatch` an
//!   argument-derived address is mapped and reachable from ring 3 — the load or
//!   store cannot fault and cannot reach the kernel's memory.
//!
//!   Two consequences worth keeping in mind. A value READ out of that memory (an
//!   `iovec`'s base and length, a `msghdr`'s vectors) is not itself validated by
//!   being read: the nested checks in `uaccess::nested` cover the ones the ABI
//!   defines, and a handler that derives a further address re-checks it. And the
//!   process may be rewriting those bytes from another thread while we read them,
//!   so nothing here may assume that two reads of the same word agree.
//!
//! - **S2. A `static mut` here is the syscall layer's own state.** The heap
//!   bounds, the pseudo-file buffers, the per-task last-syscall records: they are
//!   written by the task that owns them (indexed by `sched::current_id()`), or
//!   only on the launch path before the workload runs. Where a static is shared
//!   between cores it is an atomic, not a `static mut`.
//!
//! - **S3. A call into another module's `unsafe fn` carries that module's
//!   contract.** `user::execve*`, `paging::switch`, `sched::migrate_current_to`
//!   and the like are documented at their definitions; here the relevant fact is
//!   always the same one: we are on the task's own kernel stack, with the task's
//!   CR3 active, and the task is the current one.
//!
//! - **S4. `asm!` touches THIS core's machine state** (the MSRs at `init`, the
//!   FPU area, the segment bases). The `options(...)` on each block say what the
//!   compiler may assume.
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub static EXITED: AtomicBool = AtomicBool::new(false);
/// A diagnostic toggle: switch the TLB shootdown off (talaria.noshoot=1).
/// Every task's last syscall (the number + the return rip): the only cheap
/// way to understand WHERE the kernel came into garbage from, when a crash dump shows
/// a corrupted control flow.
/// How many times every syscall was called and how many cycles were spent in it.
///
/// A common counter "there are so many syscalls" says only that there are many of them.
/// The question is always a different one: WHICH of them eats the time. Without a breakdown by number
/// that question is answered by going through hypotheses, and that costs an evening.
/// The breakdown of futex by operation (WAIT=0, WAKE=1, REQUEUE=3, CMP_REQUEUE=4,
/// WAKE_OP=5, WAIT_BITSET=9, WAKE_BITSET=10): "there are many futexes" is not a diagnosis.
pub static FUTEX_OPS: [core::sync::atomic::AtomicU64; 16] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 16];

/// Is the syscall profile on (`talaria.sysprof=1`).
pub static SYSPROF: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The `mremap` counters: in total, without a move, with a move, the refusals. Without them one cannot
/// see whether the mechanism works at all — and that is the first thing to check.
pub static MREMAP_CALLS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static MREMAP_INPLACE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static MREMAP_MOVED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static MREMAP_FAIL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub static SYS_COUNT: [core::sync::atomic::AtomicU64; 512] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 512];
pub static SYS_TICKS: [core::sync::atomic::AtomicU64; 512] =
    [const { core::sync::atomic::AtomicU64::new(0) }; 512];

pub static LAST_SYS: [core::sync::atomic::AtomicU64; crate::sched::NTASKS] =
    [const { core::sync::atomic::AtomicU64::new(u64::MAX) }; crate::sched::NTASKS];
/// The last syscall's first argument (usually an fd) — together with the number it gives
/// an understanding of what exactly the thread got stuck on.
pub static LAST_ARG0: [core::sync::atomic::AtomicU64; crate::sched::NTASKS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; crate::sched::NTASKS];
/// The TSC stamps of the entry into and the exit from a task's last syscall — for "the tick's gap"
/// (`sched::tick_gap_note`): by them one can see whether the syscall COVERED the gap entirely
/// (then the kernel's path under IF=0 is to blame) or the task was computing in ring 3.
pub static LAST_SYS_T0: [core::sync::atomic::AtomicU64; crate::sched::NTASKS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; crate::sched::NTASKS];
pub static LAST_SYS_T1: [core::sync::atomic::AtomicU64; crate::sched::NTASKS] =
    [const { core::sync::atomic::AtomicU64::new(0) }; crate::sched::NTASKS];
pub static NO_SHOOTDOWN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
pub static THREADED: AtomicBool = AtomicBool::new(false);
/// A10: about a refusal of sched_setaffinity (the core is not among the online ones) we speak ONCE —
/// the workload is entitled to call it in a loop over all the cores, and the log would choke.
static AFF_WARNED: AtomicBool = AtomicBool::new(false);
/// A12: the workload's first syscall is printed once (see dispatch).
static FIRST_SYSCALL: AtomicBool = AtomicBool::new(false);
/// A12: how many more syscalls to print with the state of the interrupts (`talaria.irr=N`).
pub static IRR_TRACE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static EXIT_CODE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static HEAP_BASE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static HEAP_BRK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static HEAP_HIGH: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0); // the maximum of brk over all time (for the zeroing at a restart)
// ── the table of null/discard fds (an honest /dev/null + eventfd/pipe/sem stubs) ──
// An fd >= NULL_BASE is a "null device": a write is discarded, a read is an EOF.
// Real alloc/free (unlike the former monotonic FAKE_FD counter).
/// The buffer for assembling a writev over a socket — its own per core (see `20 =>`). 16 KB cover
/// a header with the body of an ordinary answer; what did not fit goes along the former path in
/// pieces.
const WVBUF: usize = 16 * 1024;
static mut WV: [[u8; WVBUF]; crate::smp::MAXCPU] = [[0; WVBUF]; crate::smp::MAXCPU];

const NULL_BASE: i64 = 2000;
const NULL_N: usize = 256;
/// The slot is taken. An ATOMIC array rather than a `static mut`: the allocation
/// below is a compare-exchange, because two threads on different cores may call
/// `eventfd`/`timerfd_create` at the same instant, and a plain "read false, write
/// true" hands them both the same descriptor — one pseudo-file for two owners.
static NULL_OPEN: [AtomicBool; NULL_N] = [const { AtomicBool::new(false) }; NULL_N];
static mut NULL_KIND: [u8; NULL_N] = [0; NULL_N]; // 0 = /dev/null (EOF), 1 = /dev/urandom|random (RDRAND), 2 = /dev/zero, 4 = eventfd, 5 = timerfd
static mut NULL_CTR: [u64; NULL_N] = [0; NULL_N];   // eventfd: the counter; timerfd: the deadline (mono ns, 0 = not armed)
static mut NULL_IVL: [u64; NULL_N] = [0; NULL_N];   // timerfd: the interval (ns)
static mut NULL_FL: [u8; NULL_N] = [0; NULL_N];     // bit0 nonblock, bit1 EFD_SEMAPHORE, bit2 timerfd CLOCK_REALTIME
static mut NULL_GEN: [u64; NULL_N] = [0; NULL_N];   // eventfd: the generation — it grows at every write (an edge for EPOLLET)
/// An eventfd's counter and generation as atomics: a write on one core and a read on another
/// change them at once (threads, or two processes sharing the descriptor). They were a plain
/// read-modify-write each — an increment lost under a concurrent read — and on arm64 the
/// reader had no Acquire to see the data the writer published before `eventfd_write`.
fn ev_ctr(i: usize) -> &'static AtomicU64 {
    // SAFETY: S2 — the row is the descriptor's from the CAS in `null_alloc_kind`; the word
    // is a naturally aligned u64 in a static, and every concurrent access to an eventfd's
    // counter goes through this view.
    unsafe { AtomicU64::from_ptr(core::ptr::addr_of_mut!(NULL_CTR[i])) }
}
fn ev_gen(i: usize) -> &'static AtomicU64 {
    // SAFETY: as `ev_ctr`.
    unsafe { AtomicU64::from_ptr(core::ptr::addr_of_mut!(NULL_GEN[i])) }
}
/// The readiness of an eventfd/timerfd/signalfd for epoll/poll (EPOLLIN=1, EPOLLOUT=4). Other null fds: OUT.
/// THE GENERATION of a null fd's readiness for EPOLLET: it changes at every edge
/// (a write into an eventfd; another firing of a timerfd). 0 — this kind has no
/// edges, epoll counts the descriptor by level.
pub fn nullfd_gen(fd: i64) -> u64 {
    let i = (fd - NULL_BASE) as usize;
    // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's owner from the
    // CAS in `null_alloc_kind` until `null_free`.
    unsafe { match NULL_KIND[i] {
        4 => ev_gen(i).load(Ordering::Acquire),
        5 => { let d = NULL_CTR[i]; if d == 0 { return 1; } let now = crate::tsc::mono_ns(); let iv = NULL_IVL[i];
               if now < d || iv == 0 { d } else { d + ((now - d) / iv) * iv } }
        _ => 0,
    } }
}
pub fn nullfd_readiness(fd: i64) -> u32 {
    let i = (fd - NULL_BASE) as usize;
    // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's owner from the
    // CAS in `null_alloc_kind` until `null_free`.
    unsafe { match NULL_KIND[i] {
        4 => (if ev_ctr(i).load(Ordering::Acquire) > 0 { 1 } else { 0 }) | 4,
        5 => if NULL_CTR[i] != 0 && crate::tsc::mono_ns() >= NULL_CTR[i] { 1 } else { 0 },
        6 => if crate::proc::pending_in(NULL_CTR[i]) { 1 } else { 0 },
        7 | 8 => 1 | 4,
        _ => 4,
    } }
}
// ── a procfs/sysfs minimum: virtual files (kind 7) and directories (kind 8) ──────────────────
// The content is generated at a read (processes like mongod/FTDC and the Go runtime read /proc/meminfo,
// cpuinfo, self/status, /sys/kernel/mm/transparent_hugepage/enabled and so on).
const VIRT_FILES: [&[u8]; 19] = [b"/proc/meminfo", b"/proc/cpuinfo", b"/proc/stat", b"/proc/uptime", b"/proc/version", b"/proc/self/status", b"/proc/self/stat",
    b"/proc/self/maps", b"/proc/self/cmdline", b"/proc/self/mounts", b"/proc/mounts", b"/proc/sys/vm/max_map_count", b"/proc/sys/vm/overcommit_memory",
    b"/proc/sys/kernel/pid_max", b"/proc/sys/kernel/osrelease", b"/sys/kernel/mm/transparent_hugepage/enabled", b"/sys/kernel/mm/transparent_hugepage/defrag",
        b"/proc/diskstats", b"/proc/self/smaps"];
/// `/proc/self/smaps`'s index in `VIRT_FILES`.
const VIRT_SMAPS: usize = 18;
const VIRT_DIRS: [&[u8]; 8] = [b"/proc", b"/proc/self", b"/proc/sys", b"/proc/sys/vm", b"/proc/sys/kernel", b"/sys", b"/sys/block", b"/sys/kernel"];
/// A virtual path: Some((the index, a directory?)).
fn virt_lookup(path: &[u8]) -> Option<(usize, bool)> {
    let p = match path.iter().position(|&c| c == 0) { Some(i) => &path[..i], None => path };
    for (i, f) in VIRT_FILES.iter().enumerate() { if *f == p { return Some((i, false)); } }
    for (i, d) in VIRT_DIRS.iter().enumerate() { if *d == p { return Some((i, true)); } }
    None
}
fn wr_dec(out: &mut [u8], n: &mut usize, v: u64) {
    let mut t = [0u8; 20]; let mut k = 0; let mut x = v;
    if x == 0 { t[0] = b'0'; k = 1; }
    while x > 0 { t[k] = b'0' + (x % 10) as u8; x /= 10; k += 1; }
    while k > 0 && *n < out.len() { k -= 1; out[*n] = t[k]; *n += 1; }
}
fn wr_s(out: &mut [u8], n: &mut usize, s: &[u8]) { let m = s.len().min(out.len() - *n); out[*n..*n + m].copy_from_slice(&s[..m]); *n += m; }
/// A SELF-CHECK: a virtual file is not truncated on a multi-core machine.
///
/// The gate was introduced because the truncation was SILENT: `/proc/cpuinfo` ran into
/// a buffer of 2048 bytes, and on eight cores the workload saw FIVE processors, while on
/// metal with a hundred and twenty-eight the same five. The file looked
/// plausible meanwhile: the middle of the last record was cut off. ClickHouse, the JVM and
/// PostgreSQL size their pools by it.
pub fn selftest_virtfiles() -> bool {
    let n = crate::affinity::online_count().max(1);
    let mut ok = true;
    let count = |c: &[u8], pat: &[u8]| -> usize {
        let mut k = 0; let mut i = 0;
        while i + pat.len() <= c.len() { if &c[i..i + pat.len()] == pat { k += 1; } i += 1; }
        k
    };
    let cpu = with_virt_content(1, |c| (c.len(), count(c, b"processor\t: ")));
    let st = with_virt_content(2, |c| count(c, b"\ncpu"));
    crate::println!("virtfiles: the cores {}, /proc/cpuinfo {} bytes, the records {}; /proc/stat the cpuN lines {}",
        n, cpu.0, cpu.1, st);
    if cpu.1 != n { crate::println!("virtfiles: FAILURE — /proc/cpuinfo has {} records instead of {}", cpu.1, n); ok = false; }
    // The first line of `/proc/stat` is the summary `cpu  `, and it goes without a leading
    // newline: the pattern `\ncpu` counts exactly the cpuN lines.
    if st != n { crate::println!("virtfiles: FAILURE — /proc/stat has {} cpuN lines instead of {}", st, n); ok = false; }
    crate::println!("virtfiles: {}", if ok { "ALL OK" } else { "FAILED" });
    ok
}

/// HOW MUCH ROOM A VIRTUAL FILE NEEDS.
///
/// The buffer was 2048 bytes for everything, and for `/proc/cpuinfo` that was A SILENT CEILING:
/// a record per core takes about 450 bytes, which means that on an eight-core machine
/// the workload saw FIVE processors, while on metal with a hundred and twenty-eight
/// the same five. It was checked with a probe: `smp=8` -> `cpuinfo=2048 bytes,
/// processor lines=5`. ClickHouse, the JVM and PostgreSQL size their pools by this file,
/// that is, the whole run on a large machine would go in five threads.
///
/// The truncation was SILENT doubly: `wr_s` silently writes as much as fits, while
/// the last record was cut off in the middle of a line — the file looked plausible.
fn virt_bytes_needed(id: usize) -> usize {
    match id {
        1 | 2 => 1024 + crate::affinity::online_count().max(1) * 512, // cpuinfo, stat — a record per core
        VIRT_SMAPS => { let mut k = 0usize; crate::paging::as_runs(smaps_root(), |_| k += 1); 512 + k * SMAPS_RECORD }
        _ => 4096,
    }
}

/// Generate the content of the virtual file id and give it to a closure.
///
/// The buffer is taken from the allocator rather than from the stack: with `/proc/cpuinfo` the size grows
/// with the number of cores, and on metal that is tens of kilobytes — the kernel's stack does not have that much.
/// These files are read rarely (at the workload's start), so the price of the allocation does not matter, while
/// a silent truncation does.
fn with_virt_content<T>(id: usize, f: impl FnOnce(&[u8]) -> T) -> T {
    let need = virt_bytes_needed(id);
    let pages = need.div_ceil(4096) as u64;
    let Some(p) = crate::mem::alloc_contig(pages) else {
        let mut tmp = [0u8; 2048];
        let n = virt_content(id, &mut tmp);
        return f(&tmp[..n]);
    };
    // SAFETY: the frames have just come from the allocator, so this slice is the KERNEL's
    // own scratch buffer (not the process's memory): `pages * 4096` bytes, exactly what was
    // allocated, freed below, and never handed to ring 3.
    let buf = unsafe { core::slice::from_raw_parts_mut(p as *mut u8, (pages * 4096) as usize) };
    let n = virt_content(id, buf);
    let r = f(&buf[..n]);
    crate::mem::free_contig(p, pages);
    r
}

/// Generate the content of the virtual file id into out; return the length.
fn virt_content(id: usize, out: &mut [u8]) -> usize {
    let mut n = 0usize;
    let free_kb = crate::mem::free_frames() * 4; let total_kb = free_kb + 256 * 1024;
    let up = crate::tsc::mono_ns() / 1_000_000_000;
    let pid = (crate::sched::proc_of(crate::sched::current_id()) + 1) as u64;
    match id {
        0 => { wr_s(out, &mut n, b"MemTotal:       "); wr_dec(out, &mut n, total_kb); wr_s(out, &mut n, b" kB
MemFree:        "); wr_dec(out, &mut n, free_kb); wr_s(out, &mut n, b" kB
MemAvailable:   "); wr_dec(out, &mut n, free_kb); wr_s(out, &mut n, b" kB
Buffers:               0 kB
Cached:                0 kB
SwapTotal:             0 kB
SwapFree:              0 kB
Shmem:                 0 kB
HugePagesize:       2048 kB
"); }
        1 => {
            // A RECORD PER EVERY core. Here there was one — and that cost the workloads
            // their parallelism: ClickHouse and the JVM determine the number of threads by
            // /proc/cpuinfo, saw a "single-core machine" and computed in one thread,
            // however many cores they were given.
            let ncpu = crate::affinity::online_count().max(1);
            let mhz = crate::tsc::calibrate_cached() / 1_000_000;
            for c in 0..ncpu {
                wr_s(out, &mut n, b"processor\t: "); wr_dec(out, &mut n, c as u64);
                wr_s(out, &mut n, b"\nvendor_id\t: GenuineIntel\ncpu family\t: 6\nmodel\t\t: 85\nmodel name\t: Talaria vCPU (KVM)\nstepping\t: 4\ncpu MHz\t\t: \
                    ");
                wr_dec(out, &mut n, mhz);
                wr_s(out, &mut n, b".000\ncache size\t: 16384 KB\nphysical id\t: 0\nsiblings\t: ");
                wr_dec(out, &mut n, ncpu as u64);
                wr_s(out, &mut n, b"\ncore id\t\t: "); wr_dec(out, &mut n, c as u64);
                wr_s(out, &mut n, b"\ncpu cores\t: "); wr_dec(out, &mut n, ncpu as u64);
                wr_s(out, &mut n, b"\napicid\t\t: "); wr_dec(out, &mut n, c as u64);
                wr_s(out, &mut n, b"\nflags\t\t: fpu vme de pse tsc msr pae mce cx8 apic sep mtrr pge mca cmov pat pse36 clflush mmx fxsr sse sse2 ht syscall \
                    nx lm constant_tsc nopl pni ssse3 cx16 sse4_1 sse4_2 popcnt aes xsave avx rdrand\nbogomips\t: 7200.00\n\n");
            }
        }
        2 => { wr_s(out, &mut n, b"cpu  "); wr_dec(out, &mut n, up * 100); wr_s(out, &mut n, b" 0 0 0 0 0 0 0 0 0\n");
            for c in 0..crate::affinity::online_count().max(1) { // a line per core: parallelism is counted by it too
                wr_s(out, &mut n, b"cpu"); wr_dec(out, &mut n, c as u64);
                wr_s(out, &mut n, b" "); wr_dec(out, &mut n, up * 100); wr_s(out, &mut n, b" 0 0 0 0 0 0 0 0 0\n");
            }
            wr_s(out, &mut n, b"intr 0
ctxt 0
btime "); wr_dec(out, &mut n, crate::tsc::wall_ns() / 1_000_000_000 - up); wr_s(out, &mut n, b"
processes 1
procs_running 1
procs_blocked 0
"); }
        3 => { wr_dec(out, &mut n, up); wr_s(out, &mut n, b".00 0.00
"); }
        4 => { wr_s(out, &mut n, b"Linux version 5.15.0-talaria (talaria@bare-metal) (rustc nightly) #1 SMP
"); }
        5 => { wr_s(out, &mut n, b"Name:	proc
Umask:	0022
State:	R (running)
Tgid:	"); wr_dec(out, &mut n, pid); wr_s(out, &mut n, b"
Pid:	"); wr_dec(out, &mut n, pid); wr_s(out, &mut n, b"
PPid:	0
Uid:	1000	1000	1000	1000
Gid:	1000	1000	1000	1000
FDSize:	256
VmPeak:	  131072 kB
VmSize:	  131072 kB
VmRSS:	   65536 kB
Threads:	"); wr_dec(out, &mut n, crate::sched::proc_task_count(crate::sched::proc_of(crate::sched::current_id())) as u64); wr_s(out, &mut n, b"
SigPnd:	0000000000000000
Cpus_allowed:	1
Cpus_allowed_list:	0
"); }
        6 => { wr_dec(out, &mut n, pid); wr_s(out, &mut n, b" (proc) R 0 1 1 0 -1 4194560 100 0 0 0 1 1 0 0 20 0 1 0 1 134217728 16384 18446744073709551615 0 \
            0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0 0
"); }
        7 => { wr_s(out, &mut n, b"10000000-10100000 r-xp 00000000 00:00 0
7fffeff00000-7ffff0000000 rw-p 00000000 00:00 0 [stack]
"); }
        8 => { wr_s(out, &mut n, b"talaria\0"); }
        9 | 10 => { wr_s(out, &mut n, b"/dev/vda / ext2 rw,relatime 0 0
proc /proc proc rw 0 0
"); }
        11 => { wr_s(out, &mut n, b"65530
"); }
        12 => { wr_s(out, &mut n, b"0
"); }
        13 => { wr_s(out, &mut n, b"32768
"); }
        14 => { wr_s(out, &mut n, b"5.15.0-talaria
"); }
        15 | 16 => { wr_s(out, &mut n, b"always madvise [never]
"); }
        VIRT_SMAPS => smaps(out, &mut n),
        _ => {}
    }
    n
}
/// The calling process's root table (0 — a kernel thread: nothing to show).
fn smaps_root() -> u64 { crate::sched::task_cr3(crate::sched::current_id()) & 0x0000_ffff_ffff_f000 }
/// The bytes one smaps record takes at most (the header line and twelve fields).
const SMAPS_RECORD: usize = 512;
/// `/proc/self/smaps` from the tables (`paging::as_runs`): a record per run of resident
/// pages, Linux's field names, what the kernel cannot know (Pss as the share it does not
/// split, Swap) given as Linux gives it for a private anonymous mapping. Redis reads
/// Shared_Dirty at start-up (its check for an arm64 kernel that loses the dirty bit across
/// a write-protect) and refuses to run when the file is not there.
fn smaps(out: &mut [u8], n: &mut usize) {
    fn hex(out: &mut [u8], n: &mut usize, v: u64) {
        let mut t = [0u8; 16]; let mut k = 0; let mut x = v;
        loop { t[k] = b"0123456789abcdef"[(x & 0xF) as usize]; x >>= 4; k += 1; if x == 0 && k >= 8 { break; } }
        while k > 0 && *n < out.len() { k -= 1; out[*n] = t[k]; *n += 1; }
    }
    fn field(out: &mut [u8], n: &mut usize, name: &[u8], kb: u64) {
        wr_s(out, n, name);
        let mut t = [0u8; 20]; let mut k = 0; let mut x = kb;
        loop { t[k] = b'0' + (x % 10) as u8; x /= 10; k += 1; if x == 0 { break; } }
        for _ in k..(24usize.saturating_sub(name.len()) + 4) { wr_s(out, n, b" "); }
        while k > 0 && *n < out.len() { k -= 1; out[*n] = t[k]; *n += 1; }
        wr_s(out, n, b" kB\n");
    }
    crate::paging::as_runs(smaps_root(), |r| {
        if *n + SMAPS_RECORD > out.len() { return; }
        hex(out, n, r.start); wr_s(out, n, b"-"); hex(out, n, r.end);
        wr_s(out, n, if r.write { b" rw" } else { b" r-" });
        wr_s(out, n, if r.exec { b"xp 00000000 00:00 0\n" } else { b"-p 00000000 00:00 0\n" });
        let size = (r.end - r.start) / 1024;
        let rss = (r.shared_clean + r.shared_dirty + r.private_clean + r.private_dirty) * 4;
        field(out, n, b"Size:", size);
        field(out, n, b"KernelPageSize:", 4);
        field(out, n, b"MMUPageSize:", 4);
        field(out, n, b"Rss:", rss);
        field(out, n, b"Pss:", rss);
        field(out, n, b"Shared_Clean:", r.shared_clean * 4);
        field(out, n, b"Shared_Dirty:", r.shared_dirty * 4);
        field(out, n, b"Private_Clean:", r.private_clean * 4);
        field(out, n, b"Private_Dirty:", r.private_dirty * 4);
        field(out, n, b"Referenced:", rss);
        field(out, n, b"Anonymous:", rss);
        field(out, n, b"Swap:", 0);
    });
}
/// Open a virtual file/directory -> a handle (kind 7/8), None — it is not a virtual path.
fn virt_open(path: &[u8]) -> Option<i64> {
    let (id, dir) = virt_lookup(path)?;
    let h = null_alloc_kind(if dir { 8 } else { 7 });
    // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's owner from the
    // CAS in `null_alloc_kind` until `null_free`.
    if h >= 0 { unsafe { let i = (h - NULL_BASE) as usize; NULL_IVL[i] = id as u64; NULL_CTR[i] = 0; } }
    Some(h)
}
fn virt_stat(path: &[u8], st: *mut u8) -> Option<i64> {
    let (id, dir) = virt_lookup(path)?;
    let n = if dir { 0 } else { with_virt_content(id, |c| c.len()) };
    // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate` has
    // already validated.
    unsafe { core::ptr::write_bytes(st, 0, 144); core::ptr::write_unaligned(st.add(24) as *mut u32, if dir { 0o40555 } else { 0o100444 });
        core::ptr::write_unaligned(st.add(16) as *mut u64, 1); core::ptr::write_unaligned(st.add(48) as *mut i64, n as i64);
        core::ptr::write_unaligned(st.add(56) as *mut i64, 4096); core::ptr::write_unaligned(st.add(28) as *mut u32, 1000);
        core::ptr::write_unaligned(st.add(32) as *mut u32, 1000); }
    Some(0)
}
/// timerfd: how many firings have accumulated (and rearm a periodic one). 0 — there are none yet.
fn timerfd_expirations(i: usize) -> u64 {
    // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's owner from the
    // CAS in `null_alloc_kind` until `null_free`.
    unsafe {
        let d = NULL_CTR[i]; if d == 0 { return 0; }
        let now = crate::tsc::mono_ns(); if now < d { return 0; }
        let iv = NULL_IVL[i];
        if iv == 0 { NULL_CTR[i] = 0; 1 } else { let n = 1 + (now - d) / iv; NULL_CTR[i] = d + n * iv; n }
    }
}

fn null_alloc() -> i64 { null_alloc_kind(0) }
fn null_alloc_kind(kind: u8) -> i64 {
    for i in 0..NULL_N {
        // The slot is TAKEN BY A CAS: the loser of the race walks on to the next one.
        if NULL_OPEN[i].compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() { continue; }
        // SAFETY: S2 — the slot is ours from the CAS above until `null_free`, so its row of
        // the table (`NULL_KIND`/`NULL_CTR`/`NULL_IVL`/`NULL_GEN`/`NULL_FL`) is ours alone.
        unsafe { NULL_KIND[i] = kind; }
        return NULL_BASE + i as i64;
    }
    -24 // EMFILE
}
fn null_is(fd: i64) -> bool { fd >= NULL_BASE && (fd - NULL_BASE) < NULL_N as i64 }
// ── A3.2 the syscall filter (an allowlist from the cell's manifest) ─────────────
// By default it is OFF (the services use a wide set). It is switched on by the workload's
// declaration; outside the allowlist -> -EPERM + a record into the flight recorder + a counter.
const SYS_MAX: usize = 512;
static FILTER_ON: AtomicBool = AtomicBool::new(false);
static mut SYS_ALLOW: [bool; SYS_MAX] = [false; SYS_MAX];
static SYS_DENIED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Switch the filter on with a given allowlist of syscall numbers.
pub fn filter_enable(allow: &[u32]) {
    // SAFETY: S2 — `SYS_ALLOW`: the manifest's allowlist — filled once at the launch, read-only
    // afterwards.
    unsafe {
        for i in 0..SYS_MAX { core::ptr::write_volatile(core::ptr::addr_of_mut!(SYS_ALLOW[i]), false); }
        for &nr in allow { if (nr as usize) < SYS_MAX { core::ptr::write_volatile(core::ptr::addr_of_mut!(SYS_ALLOW[nr as usize]), true); } }
    }
    FILTER_ON.store(true, Ordering::SeqCst);
}
pub fn filter_denied() -> u64 { SYS_DENIED.load(Ordering::Relaxed) }
#[inline] fn filter_blocks(nr: u64) -> bool {
    if !FILTER_ON.load(Ordering::Relaxed) { return false; }
    let n = nr as usize;
    if n >= SYS_MAX { return true; }
    // SAFETY: S2 — `SYS_ALLOW`: the manifest's allowlist — filled once at the launch, read-only
    // afterwards.
    !unsafe { core::ptr::read_volatile(core::ptr::addr_of!(SYS_ALLOW[n])) }
}

fn null_free(fd: i64) { if null_is(fd) { NULL_OPEN[(fd - NULL_BASE) as usize].store(false, Ordering::SeqCst); } }

/// Compare a path's C string (ptr) with a pattern (without taking the trailing \0 in the pattern into account).
fn path_eq(ptr: u64, pat: &[u8]) -> bool {
    if ptr == 0 { return false; }
    for i in 0..pat.len() {
        // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate`
        // has already validated.
        let c = unsafe { core::ptr::read_volatile((ptr as *const u8).add(i)) };
        if c != pat[i] { return false; }
    }
    unsafe { core::ptr::read_volatile((ptr as *const u8).add(pat.len())) == 0 } // exactly the end of the string
}
fn is_dev_null(ptr: u64) -> bool { path_eq(ptr, b"/dev/null") }
/// Does the C string at ptr start with pat?
fn path_eq_prefix(ptr: u64, pat: &[u8]) -> bool {
    if ptr == 0 { return false; }
    for i in 0..pat.len() {
        // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate`
        // has already validated.
        if unsafe { core::ptr::read_volatile((ptr as *const u8).add(i)) } != pat[i] { return false; }
    }
    true
}
const HEAP_FRAMES: u64 = 32768; // a 128 MB heap for brk (for large C++ software)


use crate::arch::cpu::{set_tls_base, tls_base};

/// The frame of a syscall's arguments: what the entry stub saved, in the stub's push order.
/// The struct is the architecture's (`arch::syscall_entry::Frame`); the shared code here reads
/// the number, the six arguments, `pc`, `flags` and `usp` — the rest (the callee-saved
/// registers a fork child and a signal frame inherit) only through `arch::signal`.
pub use crate::arch::syscall_entry::Frame;
/// Set up SYSCALL: STAR (the selectors), LSTAR (the handler), FMASK, EFER.SCE.
/// kcs/kss are the kernel CS/SS; STAR[63:48] sets the user selectors for sysret.
pub fn init() {
    // SAFETY: S4 — the SYSCALL MSRs of THIS core (STAR/LSTAR/FMASK, EFER.SCE). They are
    // armed once per core, before it ever enters ring 3, so nothing is using them yet.
    unsafe {
        crate::arch::syscall_entry::init_msrs();
        // The common brk heap is allocated ONCE (restarts of the workload by the supervisor
        // reuse the region — without a leak of 128 MB per restart); brk is reset to the base.
        let mut hb = HEAP_BASE.load(Ordering::SeqCst);
        if hb == 0 {
            hb = crate::mem::alloc_contig(HEAP_FRAMES).expect("heap");
            HEAP_BASE.store(hb, Ordering::SeqCst);
            crate::paging::nx_range(hb, HEAP_FRAMES * 4096); // the heap/brk is NX (W^X; the 2M leaves are preserved)
        } else {
            // a repeated start: zero the used part of the heap (the brk memory must be zeroed)
            let hi = HEAP_HIGH.load(Ordering::SeqCst);
            if hi > hb { core::ptr::write_bytes(hb as *mut u8, 0, (hi - hb) as usize); }
        }
        HEAP_BRK.store(hb, Ordering::SeqCst); HEAP_HIGH.store(hb, Ordering::SeqCst);
    }
    crate::fr::record(crate::fr::SYSINIT, 0);
}

/// The brk heap's base (for the introspection/hardening tests).
pub fn heap_base() -> u64 { HEAP_BASE.load(Ordering::SeqCst) }

/// execve: copy the path, argv and envp from the user memory (the current AS) into kernel buffers,
/// then replace the process's image (-> !). It returns only on an error.
///
/// The environment is copied ON A PAR with the arguments: restarting oneself with a corrected
/// LD_LIBRARY_PATH is a regular trick of the launchers (java), and losing envp there means
/// a process without JAVA_HOME/PATH. The buffers' sizes are taken with headroom for such a call
/// (48 arguments, 64 variables, 16 KB of strings in total).
const EXEC_ARGN: usize = 48;
const EXEC_ENVN: usize = 64;
/// # Safety
/// S1 + S2 + S3: the three pointers are the `execve` syscall's arguments, validated
/// by `uaccess::gate` (the vectors too, by `uaccess::nested`), so the strings may
/// be copied out of the process. The static buffers below are S2: an `execve` runs
/// with the process's other threads already stopped, so there is no second copier
/// in them. On success it does not return (`user::execve*`).
unsafe fn do_execve(path_ptr: u64, argv_ptr: u64, envp_ptr: u64) -> u64 {
    static mut PATHBUF: [u8; 256] = [0; 256];
    static mut ARGBUF: [u8; 16384] = [0; 16384];
    static mut ARGOFF: [(usize, usize); EXEC_ARGN + EXEC_ENVN] = [(0, 0); EXEC_ARGN + EXEC_ENVN];
    let pb = &mut *core::ptr::addr_of_mut!(PATHBUF);
    let ab = &mut *core::ptr::addr_of_mut!(ARGBUF);
    let ao = &mut *core::ptr::addr_of_mut!(ARGOFF);
    let mut pl = 0usize;
    while pl < 254 {
        let c = core::ptr::read_volatile((path_ptr as *const u8).add(pl));
        pb[pl] = c; if c == 0 { break; } pl += 1;
    }
    pb[pl] = 0;
    let mut aoff = 0usize;
    // Copy a vector of strings (argv/envp) into ARGBUF; return the number of strings.
    let mut copy_vec = |ptr: u64, slot0: usize, maxn: usize, aoff: &mut usize| -> usize {
        let mut n = 0usize;
        if ptr == 0 { return 0; }
        while n < maxn {
            let p = core::ptr::read_unaligned((ptr as *const u64).add(n));
            if p == 0 { break; }
            let mut l = 0usize;
            while l < 1023 && *aoff + l < ab.len() - 1 {
                let c = core::ptr::read_volatile((p as *const u8).add(l));
                if c == 0 { break; }
                ab[*aoff + l] = c; l += 1;
            }
            ao[slot0 + n] = (*aoff, l);
            *aoff += l + 1; n += 1;
        }
        n
    };
    let argc = copy_vec(argv_ptr, 0, EXEC_ARGN, &mut aoff);
    let envc = copy_vec(envp_ptr, EXEC_ARGN, EXEC_ENVN, &mut aoff);
    let mut argv: [&[u8]; EXEC_ARGN] = [b""; EXEC_ARGN];
    let mut envv: [&[u8]; EXEC_ENVN] = [b""; EXEC_ENVN];
    for k in 0..argc { argv[k] = &ab[ao[k].0..ao[k].0 + ao[k].1]; }
    for k in 0..envc { envv[k] = &ab[ao[EXEC_ARGN + k].0..ao[EXEC_ARGN + k].0 + ao[EXEC_ARGN + k].1]; }
    // ext2 is the full-blown path (dynamic/PIE + envp + auxv); TFS is the former
    // static loader (the regression's images are built statically).
    if crate::ext2::active() { crate::user::execve_dyn(&pb[..pl + 1], &argv[..argc], &envv[..envc]) }
    else { crate::user::execve(&pb[..pl + 1], &argv[..argc]) } // -> ! on success; otherwise exit_current_code(127)
}

// ── the auxiliary: the classification of fds, the statistics of unknown syscalls ──
fn is_fs(fd: i64) -> bool { if crate::ext2::active() { crate::ext2::is_fd(fd) } else { (300..500).contains(&fd) } }
const ENOSYS: u64 = (-38i64) as u64;
static mut UNKNOWN_SEEN: [u16; 32] = [0; 32];
static UNKNOWN_N: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// An unknown syscall: -ENOSYS; we print every NUMBER once (without flooding the serial).
fn unknown(nr: u64) -> u64 {
    let n = UNKNOWN_N.load(Ordering::Relaxed);
    let mut seen = false;
    // SAFETY: S2 — `UNKNOWN_SEEN`: a 'printed once' flag per syscall number; a lost race prints one
    // line twice.
    // The table holds the low 16 bits (a wild x8 of 1075454524 filled all 32 rows with
    // one line each, and the lines were what a probe then measured).
    unsafe { for i in 0..n.min(32) { if core::ptr::read_volatile(core::ptr::addr_of!(UNKNOWN_SEEN[i])) == nr as u16 { seen = true; break; } } }
    if !seen && n < 32 {
        unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(UNKNOWN_SEEN[n]), nr as u16); }
        UNKNOWN_N.store(n + 1, Ordering::Relaxed);
        crate::println!("[syscall] an unknown number {} -> ENOSYS", nr);
    }
    ENOSYS
}
/// The waiting window of a polling syscall (eventfd/timerfd/signalfd, waiting for
/// a child) — until the next recheck.
///
/// There was a `sti; hlt` here, and that gave up THE CYCLES but not THE CORE. The difference is decisive.
/// A task waiting for an event by polling is visible to the scheduler as an ordinary one — it does not sleep
/// on a futex — so its core is listed as busy: the balancer puts nothing there,
/// a thief takes nothing from there, while the task itself lies in a `hlt` meanwhile.
/// In ClickHouse's profile by code that is the first line by a large margin: 36 %
/// of the kernel's samples in `do_read`, and exactly a quarter of all the machine's ticks on ONE such
/// thread — that is, on four cores a watchdog thread on an eventfd held a whole core
/// while the computing ones jostled on three.
///
/// Now the wait goes by THE COMMON tier (`sync::wait_on_coarse`): the task
/// becomes invisible to the scheduler in exactly the same way as one sleeping on a futex — its
/// core goes under foreign work at once — while the recheck is guaranteed by
/// the deadline. The deadline is exactly a COARSE one: the tick's accuracy was the ceiling for polling anyway
/// (it woke up by the tick), while demanding accuracy would mean occupying the core's LVT timer
/// and thereby cancelling the preemption on it — see `sync::wait_on_ex`, `coarse`.
const POLL_WINDOW_NS: u64 = 200_000;
/// The word on which a polling one "sleeps": it never changes — the exit from the
/// wait is given by the deadline. It is needed only so that the wait tier accepts the task
/// into its registry and the scheduler stops scheduling it.
static POLL_WORD: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
pub fn wait_window() {
    if !THREADED.load(Ordering::Relaxed) { core::hint::spin_loop(); return; }
    crate::sync::wait_on_coarse(POLL_WORD.as_ptr() as *const u32, 0, crate::tsc::mono_ns() + POLL_WINDOW_NS);
}
/// Sleep until the moment `until_ns` of the monotonic clock: between the checks we give up THE CORE
/// (`wait_window`) rather than only the cycles. -EINTR at a signal.
fn sleep_until_mono(until_ns: u64) -> i64 {
    let me = crate::sched::current_id();
    while crate::tsc::mono_ns() < until_ns {
        if crate::proc::sig_pending() { return -4; }
        // The process has been killed while the thread slept (a sister's crash, an exec, an exit_group from
        // another core): there is no point in sleeping until the term — the exit from the syscall will
        // stop it (`stop_if_dead`). Otherwise a thread in a `sleep(1)` held its
        // core for up to a second, and the wait "the sisters have left the cores" ran into the ceiling.
        if !crate::sched::task_alive(me) { return -4; }
        wait_window(); // the same contract: there is somebody to run — give up the quantum rather than hold the core in a hlt
    }
    0
}
fn read_timespec(p: u64) -> Option<(i64, i64)> {
    if p == 0 { return None; }
    // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate` has
    // already validated.
    unsafe { Some((core::ptr::read_unaligned(p as *const i64), core::ptr::read_unaligned((p + 8) as *const i64))) }
}
fn write_timespec(p: u64, ns: u64) {
    if p == 0 { return; }
    // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate` has
    // already validated.
    unsafe { core::ptr::write_unaligned(p as *mut i64, (ns / 1_000_000_000) as i64); core::ptr::write_unaligned((p + 8) as *mut i64,
        (ns % 1_000_000_000) as i64); }
}
/// A stat buffer for a pseudo fd: st_mode (S_IFCHR for a null fd, S_IFSOCK for sockets).
fn pseudo_fstat(fd: i64, statbuf: *mut u8) -> i64 {
    let mut size: i64 = 0;
    let mode: u32 = if null_is(fd) {
        // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's owner from
        // the CAS in `null_alloc_kind` until `null_free`.
        match unsafe { NULL_KIND[(fd - NULL_BASE) as usize] } {
            7 => { let mut tmp = [0u8; 2048]; size = virt_content(unsafe { NULL_IVL[(fd - NULL_BASE) as usize] } as usize, &mut tmp) as i64; 0o100444 }
            8 => 0o40555,
            _ => 0o20000 | 0o666 }
    } else if crate::netsys::is_sock(fd) { 0o140000 | 0o666 } else if fd <= 2 { 0o20000 | 0o620 } else { return -9 };
    // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate` has
    // already validated.
    unsafe { core::ptr::write_bytes(statbuf, 0, 144); core::ptr::write_unaligned(statbuf.add(24) as *mut u32, mode);
        core::ptr::write_unaligned(statbuf.add(48) as *mut i64, size); core::ptr::write_unaligned(statbuf.add(56) as *mut i64, 4096);
        core::ptr::write_unaligned(statbuf.add(16) as *mut u64, 1); }
    0
}
/// The read(2) semantics by the fd's type.
fn do_read(fd: i64, buf: *mut u8, len: usize, flags: u64) -> i64 {
    if crate::netsys::is_udp(fd) { crate::netsys::udp_recvfrom(fd, buf, len, core::ptr::null_mut(), core::ptr::null_mut(), flags) }
    else if crate::netsys::is_tcp(fd) { crate::netsys::read(fd, buf, len, flags) }
    else if null_is(fd) {
        let i = (fd - NULL_BASE) as usize;
        // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's owner from
        // the CAS in `null_alloc_kind` until `null_free`.
        match unsafe { NULL_KIND[i] } {
            // SAFETY: S1 — the caller hands in an address of the process's memory that
            // `uaccess::gate` has already validated.
            1 => { let dst = unsafe { core::slice::from_raw_parts_mut(buf, len) }; fill_random(dst); len as i64 } // /dev/urandom
            2 => { unsafe { core::ptr::write_bytes(buf, 0, len); } len as i64 } // /dev/zero
            4 => { // eventfd: wait for the counter > 0; EFD_SEMAPHORE -> give 1, otherwise everything and zero it
                if len < 8 { return -22; }
                loop {
                    // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's
                    // owner from the CAS in `null_alloc_kind` until `null_free`.
                    let c = ev_ctr(i).load(Ordering::Acquire);
                    if c > 0 {
                        let v = if unsafe { NULL_FL[i] } & 2 != 0 { 1 } else { c };
                        // Take exactly what was seen: a write in between fails the exchange,
                        // and the loop reads the new count.
                        if ev_ctr(i).compare_exchange(c, c - v, Ordering::AcqRel, Ordering::Acquire).is_err() { continue; }
                        unsafe { core::ptr::write_unaligned(buf as *mut u64, v); }
                        return 8;
                    }
                    if unsafe { NULL_FL[i] } & 1 != 0 { return -11; }
                    if crate::proc::sig_pending() { return -4; }
                    wait_window();
                }
            }
            6 => { // signalfd: a signalfd_siginfo (128 B) by a pending signal from the mask
                if len < 128 { return -22; }
                loop {
                    // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's
                    // owner from the CAS in `null_alloc_kind` until `null_free`.
                    if let Some(sig) = crate::proc::take_pending_in(unsafe { NULL_CTR[i] }) {
                        // SAFETY: S1 — the caller hands in an address of the process's memory that
                        // `uaccess::gate` has already validated.
                        unsafe { core::ptr::write_bytes(buf, 0, 128); core::ptr::write_unaligned(buf as *mut u32, sig as u32);
                            core::ptr::write_unaligned(buf.add(12) as *mut u32, (crate::sched::proc_of(crate::sched::current_id()) + 1) as u32); }
                        return 128;
                    }
                    // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's
                    // owner from the CAS in `null_alloc_kind` until `null_free`.
                    if unsafe { NULL_FL[i] } & 1 != 0 { return -11; }
                    if crate::proc::sig_pending() { return -4; }
                    wait_window();
                }
            }
            7 => { // a virtual file (/proc, /sys): the content is generated, we read from the position
                // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's
                // owner from the CAS in `null_alloc_kind` until `null_free`.
                let off = unsafe { NULL_CTR[i] } as usize;
                with_virt_content(unsafe { NULL_IVL[i] } as usize, |c| {
                    if off >= c.len() { return 0; }
                    let k = (c.len() - off).min(len);
                    // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's
                    // owner from the CAS in `null_alloc_kind` until `null_free`.
                    unsafe { core::ptr::copy_nonoverlapping(c.as_ptr().add(off), buf, k); NULL_CTR[i] += k as u64; }
                    k as i64
                })
            }
            5 => { // timerfd: the number of firings (u64), it blocks until the deadline
                if len < 8 { return -22; }
                loop {
                    let n = timerfd_expirations(i);
                    // SAFETY: S1 — the caller hands in an address of the process's memory that
                    // `uaccess::gate` has already validated.
                    if n > 0 { unsafe { core::ptr::write_unaligned(buf as *mut u64, n); } return 8; }
                    // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's
                    // owner from the CAS in `null_alloc_kind` until `null_free`.
                    if unsafe { NULL_FL[i] } & 1 != 0 || unsafe { NULL_CTR[i] } == 0 { return -11; }
                    if crate::proc::sig_pending() { return -4; }
                    wait_window();
                }
            }
            _ => 0, // /dev/null: EOF
        }
    }
    else if crate::proc::pipe_is(fd) { crate::proc::pipe_read(fd, buf, len) }
    else if is_fs(fd) { if crate::ext2::active() { crate::ext2::read(fd, buf, len) } else { crate::fs::read(fd, buf, len) } }
    else if fd == 0 { 0 }
    else { -9 }
}
fn do_write(fd: i64, buf: *const u8, len: usize, flags: u64) -> i64 {
    if fd == 1 || fd == 2 {
        // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate`
        // has already validated.
        let s = unsafe { core::slice::from_raw_parts(buf, len) };
        crate::serial::puts_locked(s); // a whole line: the write() of two cores do not mix
        len as i64
    }
    else if crate::netsys::is_udp(fd) { crate::netsys::udp_sendto(fd, buf, len, core::ptr::null()) }
    else if crate::netsys::is_tcp(fd) { crate::netsys::write(fd, buf, len, flags) }
    else if null_is(fd) {
        let i = (fd - NULL_BASE) as usize;
        // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's owner from
        // the CAS in `null_alloc_kind` until `null_free`.
        if unsafe { NULL_KIND[i] } == 4 && len >= 8 { // eventfd
            let v = unsafe { core::ptr::read_unaligned(buf as *const u64) };
            let _ = ev_ctr(i).try_update(Ordering::Release, Ordering::Relaxed, |c| Some(c.saturating_add(v)));
            ev_gen(i).fetch_add(1, Ordering::Release);
            crate::netsys::mix_kick(); // an eventfd in an epoll set (`talaria.netmix=`)
            return 8;
        }
        len as i64
    }
    else if crate::proc::pipe_is(fd) { crate::proc::pipe_write(fd, buf, len) }
    else if is_fs(fd) { if crate::ext2::active() { crate::ext2::write(fd, buf, len) } else { -30 } } // an EROFS on TFS
    else { -9 }
}
/// Fill a buffer with random bytes (RDRAND, a fallback TSC mixer) — getrandom and /dev/urandom.
fn fill_random(dst: &mut [u8]) {
    let mut i = 0;
    while i < dst.len() {
        let v = crate::tsc::rdrand64().unwrap_or_else(|| { let t = crate::tsc::rdtsc(); (t ^ (t << 13) ^ (t >> 7)).wrapping_mul(0x9E37_79B9_7F4A_7C15) });
        let b = v.to_le_bytes(); let n = (dst.len() - i).min(8);
        dst[i..i + n].copy_from_slice(&b[..n]); i += n;
    }
}
/// Really close an internal handle (the last reference from the fd tables).
pub fn close_handle(fd: i64) -> i64 {
    // A descriptor leaves the epoll sets together with the last reference (Linux).
    crate::netsys::epoll_forget(fd);
    if null_is(fd) { null_free(fd); 0 }
    // SAFETY: S2 — one byte of the fd row is released (`-1` = the row is free). Without
    // the lock on purpose: a scanner that sees the row free is right, it IS free, and a
    // byte cannot be written torn. The object itself stays — only this descriptor goes.
    else if shm_is(fd) { unsafe { SHM_FD_OBJ[(fd - SHM_BASE) as usize] = -1; } 0 }
    else if crate::proc::pipe_is(fd) { crate::proc::pipe_close(fd); 0 }
    else if is_fs(fd) { if crate::ext2::active() { crate::ext2::close(fd) } else { crate::fs::close(fd) } }
    else if crate::netsys::is_epoll(fd) { crate::netsys::epoll_close(fd) }
    else if crate::netsys::is_sock(fd) { crate::netsys::close(fd) } else { 0 }
}
/// A stat by path: /dev/shm, /dev/null, ext2 (with the cwd) or TFS.
fn vfs_stat(ptr: u64, st: *mut u8) -> i64 {
    if path_empty(ptr) { return -2; } // ENOENT: an empty path is not "the current directory"
    if let Some(nm) = shm_name_of(ptr) {
        // A lookup WITHOUT creation, but still under the lock: a row being created by a
        // neighbour must be seen whole or not at all.
        let o = { let _g = shm_lock(); shm_lookup(nm, false) };
        return match o { Some(o) => shm_stat(o, st), None => -2 };
    }
    if is_dev_null(ptr) || path_eq(ptr, b"/dev/urandom") { return pseudo_fstat(NULL_BASE, st); }
    if crate::ext2::active() { let p = crate::proc::resolve(ptr); if let Some(r) = virt_stat(p, st) { return r; } crate::ext2::stat(p, st) } else {
        crate::fs::stat_path(ptr as *const u8, st) }
}
fn vfs_stat_at(dirfd: i64, ptr: u64, st: *mut u8) -> i64 {
    if path_empty(ptr) { return -2; }
    if dirfd == crate::proc::AT_FDCWD || !crate::ext2::active() { return vfs_stat(ptr, st); }
    if let Some(nm) = shm_name_of(ptr) {
        // A lookup WITHOUT creation, but still under the lock: a row being created by a
        // neighbour must be seen whole or not at all.
        let o = { let _g = shm_lock(); shm_lookup(nm, false) };
        return match o { Some(o) => shm_stat(o, st), None => -2 };
    }
    let p = crate::proc::resolve_at(dirfd, ptr); if let Some(r) = virt_stat(p, st) { return r; }
    crate::ext2::stat(p, st)
}
fn vfs_exists(ptr: u64) -> bool {
    if path_empty(ptr) { return false; }
    if is_dev_null(ptr) || path_eq(ptr, b"/dev/urandom") { return true; }
    if crate::ext2::active() { let p = crate::proc::resolve(ptr); virt_lookup(p).is_some() || crate::ext2::exists(p) } else {
        crate::fs::exists(ptr as *const u8) }
}
/// An open by path (/dev/null, /dev/shm, ext2 with the cwd and the flags, TFS) -> a handle.
/// AN EMPTY path is not "the current directory" but an error. Linux answers ENOENT to a `stat("")`,
/// an `access("")`, an `open("")`, and code relies on that:
/// ClickHouse writes `fs::exists(path.value_or(""))` and by "the file exists" goes
/// to open an empty string — Poco brings the process down with an assert. With us an empty path
/// used to resolve into the cwd and looked as if it existed.
fn path_empty(ptr: u64) -> bool {
    // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate` has
    // already validated.
    ptr == 0 || unsafe { core::ptr::read_volatile(ptr as *const u8) } == 0
}

fn vfs_open(ptr: u64, flags: u64, mode: u64) -> i64 { vfs_open_at(crate::proc::AT_FDCWD, ptr, flags, mode) }
fn vfs_open_at(dirfd: i64, ptr: u64, flags: u64, mode: u64) -> i64 {
    if path_empty(ptr) { return -2; } // ENOENT
    if is_dev_null(ptr) { return null_alloc(); }
    if path_eq(ptr, b"/dev/urandom") || path_eq(ptr, b"/dev/random") { return null_alloc_kind(1); }
    if path_eq(ptr, b"/dev/zero") { return null_alloc_kind(2); }
    if let Some(nm) = shm_name_of(ptr) {
        // ONE section for the whole `shm_open`: the find-or-create and the claim of the
        // descriptor's row. Split into two, a neighbour can create the same name in between.
        let _g = shm_lock();
        return match shm_lookup(nm, flags & 0o100 != 0) { Some(o) => shm_open_fd(o), None => -2 };
    }
    if crate::ext2::active() {
        let path = crate::proc::resolve_at(dirfd, ptr);
        if let Some(h) = virt_open(path) { return h; } // /proc, /sys are virtual
        let r = crate::ext2::open(path, flags, mode as u32);
        return r;
    }
    let fd = crate::fs::open(ptr as *const u8);
    if fd < 0 && (flags & 0o100) != 0 { null_alloc() } else { fd }
}
fn fstat_any(fd: i64, st: *mut u8) -> i64 {
    if is_fs(fd) { if crate::ext2::active() { crate::ext2::fstat(fd, st) } else { crate::fs::fstat(fd, st) } }
    else if shm_is(fd) { shm_stat(shm_obj(fd), st) }
    // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate` has
    // already validated.
    else if crate::proc::pipe_is(fd) { unsafe { core::ptr::write_bytes(st, 0, 144); core::ptr::write_unaligned(st.add(24) as *mut u32, 0o10000 | 0o600);
        core::ptr::write_unaligned(st.add(56) as *mut i64, 4096); } 0 }
    else { pseudo_fstat(fd, st) }
}
// ── /dev/shm/<name>: the shared memory objects (musl's POSIX shm_open semantics: an open("/dev/shm/..")
//    + an ftruncate + an mmap(MAP_SHARED, fd)). The storage is ivshmem (the host's shared memory, pci.rs):
//    all the names are mapped onto one region (one VM = one ring of the harness). Without ivshmem it is
//    anonymous identity memory (shared by all the processes, since the identity region is a common one). ──
const SHM_BASE: i64 = 2300;
const SHM_N: usize = 8;
static mut SHM_NAME: [[u8; 64]; SHM_N] = [[0; 64]; SHM_N];
static mut SHM_SIZE: [u64; SHM_N] = [0; SHM_N];      // the size by the ftruncate (stat)
static mut SHM_ADDR: [u64; SHM_N] = [0; SHM_N];
/// How many bytes of the region have been handed out (for handing the access out again in another AS).
static mut SHM_MAPSZ: [u64; SHM_N] = [0; SHM_N];      // the region's base (0 = not allocated yet)
static mut SHM_FD_OBJ: [i8; SHM_N * 4] = [-1; SHM_N * 4]; // the open fds -> the object's index
// ── THE LOCK OF THE SHM TABLES ───────────────────────────────────────────────
//
// WHY A LOCK AND NOT A CAS PER ROW. `shm_open(O_CREAT)` and `shmget(IPC_CREAT)`
// are ONE compound operation: "find the object by name or key, and if there is
// none, take a free row". Two threads creating the same name BOTH fail the
// lookup and then both take a free row — the process ends up with two objects
// where POSIX promises one, and the two halves of a workload map different
// memory. Making the row's claim atomic (as `null_alloc_kind` does for the
// pseudo-fds) closes a different hole and leaves this one open, because the hole
// is between the lookup and the claim.
//
// WHAT IT COVERS. The compound operations only: find-or-create, the claim of an
// fd row, the publication of a region's address. A read of ONE already published
// byte (`shm_is`/`shm_obj`) goes without it — that byte was written under the
// lock, and an `i8` cannot be read torn.
//
// The allocation and the zeroing of a segment stay INSIDE the section: a second
// creator spins (giving the core up every 64 turns) while the first one zeroes
// its segment. That is a startup cost of a rare race, and the alternative — a
// reservation protocol with a wait for publication — buys nothing at this
// frequency. `zero_serviced` answers the neighbours' TLB requests between its
// portions, so holding this lock cannot stall a shootdown.
//
// The owner is a TASK, not a core: a holder may be preempted and come back on
// another core. Recursion is impossible by construction — the helpers do not take
// the lock, the syscall-level call sites do.
static SHM_OWNER: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(-1);
/// How many times a creator had to wait for the tables (the gate `shmrace` reads it:
/// a race that never happened proves nothing).
pub static SHM_WAITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub struct ShmGuard(bool);
impl Drop for ShmGuard {
    fn drop(&mut self) { if self.0 { SHM_OWNER.store(-1, Ordering::Release); } }
}
/// `talaria.shmlock=0` — do not take the lock (the A/B for the gate `shmrace`: the same
/// programme must then FAIL, otherwise the gate proves nothing).
static SHM_LOCK_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
/// `talaria.shmdelay=<us>` — widen the creation window so that four threads released from a
/// barrier really collide (without it the section is microseconds long and they miss).
static SHM_DELAY_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub fn shm_from_cmdline(cmdline: &str) {
    if let Some(v) = crate::cmdline::num(cmdline, "talaria.shmdelay") {
        SHM_DELAY_US.store(v, Ordering::SeqCst);
        crate::println!("talaria: shm: the creation section is widened by {} us (the gate shmrace)", v);
    }
    if let Some(v) = crate::cmdline::num(cmdline, "talaria.shmlock") {
        if v == 0 {
            SHM_LOCK_ON.store(false, Ordering::SeqCst);
            crate::println!("talaria: shm: THE LOCK OF THE TABLES IS OFF (talaria.shmlock=0) — an A/B: a concurrent create is expected to break");
        }
    }
}
/// The artificial delay inside the creation section (0 — nothing happens here).
fn shm_gate_delay() {
    let us = SHM_DELAY_US.load(Ordering::Relaxed);
    if us == 0 { return; }
    let hz = crate::tsc::calibrate_cached().max(1);
    let end = crate::tsc::rdtsc() + us * (hz / 1_000_000).max(1);
    while crate::tsc::rdtsc() < end { core::hint::spin_loop(); }
}

/// Take the shm tables. Held for the length of one syscall's table work.
#[must_use]
fn shm_lock() -> ShmGuard {
    if !SHM_LOCK_ON.load(Ordering::Relaxed) { return ShmGuard(false); }
    let me = crate::sched::current_id() as i64;
    let mut turns = 0u32;
    while SHM_OWNER.compare_exchange(-1, me, Ordering::AcqRel, Ordering::Acquire).is_err() {
        if turns == 0 { SHM_WAITS.fetch_add(1, Ordering::Relaxed); }
        turns += 1;
        // A holder can be preempted: spinning a whole slice for it is worse than
        // giving the core up, and this path is never hot.
        if turns.is_multiple_of(64) { crate::sched::yield_now(); } else { core::hint::spin_loop(); }
    }
    ShmGuard(true)
}

// SAFETY: S2 — a read of one row of the POSIX shm table: the row was written when the
// descriptor was created, and it is read by that descriptor's owner.
fn shm_is(fd: i64) -> bool { fd >= SHM_BASE && ((fd - SHM_BASE) as usize) < SHM_N * 4 && unsafe { SHM_FD_OBJ[(fd - SHM_BASE) as usize] } >= 0 }
// SAFETY: S2 — as `shm_is`; the caller has already checked the descriptor with it.
fn shm_obj(fd: i64) -> usize { unsafe { SHM_FD_OBJ[(fd - SHM_BASE) as usize] as usize } }
fn shm_name_of(ptr: u64) -> Option<&'static [u8]> { // "/dev/shm/NAME" -> NAME
    const PFX: &[u8] = b"/dev/shm/";
    if !path_eq_prefix(ptr, PFX) { return None; }
    let p = (ptr as usize + PFX.len()) as *const u8;
    // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate` has
    // already validated.
    let mut n = 0; while n < 63 && unsafe { core::ptr::read_volatile(p.add(n)) } != 0 { n += 1; }
    Some(unsafe { core::slice::from_raw_parts(p, n) })
}
/// Find/create an object by name; return the index (None — there is no room / there is no object with !create).
fn shm_lookup(name: &[u8], create: bool) -> Option<usize> {
    // SAFETY: S2 — the POSIX shm table (`SHM_*`). THE CALLER HOLDS `shm_lock()`: this
    // is the compound "find or create", and without the lock two threads creating one
    // name take two rows (see the note at the lock).
    unsafe {
        for i in 0..SHM_N {
            let l = SHM_NAME[i].iter().position(|&c| c == 0).unwrap_or(64);
            if l > 0 && &SHM_NAME[i][..l] == name { return Some(i); }
        }
        // With ivshmem an object "exists" always (it is created by the host) — an open without O_CREAT
        // and a stat() must find it successfully: the sender waits for the producer's ring to appear.
        if !create && crate::pci::ivshmem().0 == 0 { return None; }
        // THE WINDOW OF THE RACE IS EXACTLY HERE: the lookup found nothing and a free row has
        // not been taken yet. `talaria.shmdelay=` widens it for the gate.
        shm_gate_delay();
        for i in 0..SHM_N {
            if SHM_NAME[i][0] == 0 {
                let n = name.len().min(63); SHM_NAME[i][..n].copy_from_slice(&name[..n]); SHM_NAME[i][n] = 0;
                SHM_SIZE[i] = crate::pci::ivshmem().1; SHM_ADDR[i] = 0; return Some(i);
            }
        }
    }
    None
}
fn shm_open_fd(obj: usize) -> i64 {
    // SAFETY: S2 — the POSIX shm table (`SHM_*`). THE CALLER HOLDS `shm_lock()`: this
    // is the compound "find or create", and without the lock two threads creating one
    // name take two rows (see the note at the lock).
    unsafe { for k in 0..SHM_N * 4 { if SHM_FD_OBJ[k] < 0 { SHM_FD_OBJ[k] = obj as i8; return SHM_BASE + k as i64; } } }
    -24
}
/// The base of an object's region: ivshmem (if there is one) or anonymous identity memory by the size.
fn shm_region(obj: usize, need: u64) -> u64 {
    // SAFETY: S2 — the POSIX shm table (`SHM_*`). THE CALLER HOLDS `shm_lock()`: this
    // is the compound "find or create", and without the lock two threads creating one
    // name take two rows (see the note at the lock).
    unsafe {
        // The region has already been handed to another process: the access from ring 3 in THIS AS
        // is opened explicitly (the identity window in a process's AS is closed).
        if SHM_ADDR[obj] != 0 {
            crate::paging::protect(SHM_ADDR[obj], SHM_MAPSZ[obj], true, false);
            return SHM_ADDR[obj];
        }
        let (ib, isz) = crate::pci::ivshmem();
        if ib != 0 {
            // ivshmem: user-accessible, CACHEABLE (protect removes PCD/PWT GB3), RW, NX
            crate::paging::protect(ib, isz, true, false);
            SHM_ADDR[obj] = ib; SHM_MAPSZ[obj] = isz; if SHM_SIZE[obj] == 0 { SHM_SIZE[obj] = isz; }
            crate::println!("shm: the object '{}' -> ivshmem {:#x} ({} KB)",
                core::str::from_utf8(&SHM_NAME[obj][..SHM_NAME[obj].iter().position(|&c| c == 0).unwrap_or(0)]).unwrap_or("?"), ib, isz / 1024);
            return ib;
        }
        let sz = need.max(SHM_SIZE[obj]).max(4096);
        let pages = sz.div_ceil(4096);
        match crate::mem::alloc_contig(pages) {
            Some(b) => { zero_serviced(b, pages * 4096); crate::paging::protect(b, pages * 4096, true, false); SHM_ADDR[obj] = b; SHM_MAPSZ[obj] = pages * 4096;
                b }
            None => 0,
        }
    }
}
/// Zero an identity area by 2 MB, answering the neighbours' requests for a TLB flush
/// between the portions (see the identity branch of `sys_mmap`).
fn zero_serviced(base: u64, len: u64) {
    let mut off = 0u64;
    while off < len {
        let n = (len - off).min(2 << 20);
        // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate`
        // has already validated.
        unsafe { core::ptr::write_bytes((base + off) as *mut u8, 0, n as usize); }
        crate::paging::tlb_service();
        off += n;
    }
}

fn shm_stat(obj: usize, statbuf: *mut u8) -> i64 {
    // SAFETY: S2 — one field of an EXISTING row is read (the caller found the object
    // under `shm_lock()` and released it: the size of a created object does not change).
    // The write into the process's memory below is deliberately outside the lock — a
    // lazy page there faults, and a fault under a spin lock is worth avoiding.
    let sz = unsafe { SHM_SIZE[obj] };
    // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate` has
    // already validated.
    unsafe { core::ptr::write_bytes(statbuf, 0, 144); core::ptr::write_unaligned(statbuf.add(24) as *mut u32, 0o100000 | 0o600);
        core::ptr::write_unaligned(statbuf.add(48) as *mut i64, sz as i64); core::ptr::write_unaligned(statbuf.add(56) as *mut i64, 4096); }
    0
}
fn in_ivshmem(addr: u64) -> bool { let (b, s) = crate::pci::ivshmem(); b != 0 && addr >= b && addr < b + s }

/// A reset of the kernel's per-process state before a restart of the workload (supervise).
pub fn reset_process_state() {
    for i in 0..NULL_N { NULL_OPEN[i].store(false, Ordering::SeqCst); }
    crate::fs::reset_open(); crate::ext2::reset_open();
    crate::netsys::reset_all();
    crate::proc::reset_all();
    EXITED.store(false, Ordering::SeqCst);
    THREADED.store(false, Ordering::SeqCst);
    // Return the crashed instance's identity mmap regions (the mallocng arenas and so on) —
    // otherwise every restart would lose them forever (~hundreds of MB with large C++).
    let (nr, np) = crate::mem::mreg_reset_free();
    if nr > 0 { crate::println!("supervise: {} mmap regions of the instance have been freed ({} MB)", nr, np * 4 / 1024); }
}

// ── the syscall trace (talaria.strace=N: printing the first N calls — the workload's diagnostics) ──
static STRACE_LEFT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub fn strace_enable(n: u64) { STRACE_LEFT.store(n, Ordering::Relaxed); STRACE_ON.store(true, Ordering::Relaxed); }
static STRACE_ON: AtomicBool = AtomicBool::new(false);
pub fn strace_on() -> bool { STRACE_ON.load(Ordering::Relaxed) }
fn strace(f: &Frame, ret: u64) {
    let left = STRACE_LEFT.load(Ordering::Relaxed);
    if left == 0 { return; }
    STRACE_LEFT.store(left - 1, Ordering::Relaxed);
    crate::println!("[strace {}us t{}] {}({:#x}, {:#x}, {:#x}, {:#x}) = {:#x} ({})", crate::tsc::mono_ns() / 1000, crate::sched::current_id(), f.nr, f.a1, f.a2,
        f.a3, f.a4, ret, ret as i64);
}

/// The syscall dispatcher (the Linux x86-64 numbers). The returned value -> rax.
/// A dead thread does not return into ring 3.
///
/// `exit_group` marks the process's threads dead, but the one that was sitting in a syscall
/// at that moment reaches the end of the syscall and returns into user space. The scheduler
/// will not choose it any more — while it does not give up the processor either: a futex wait for a dead
/// task exits immediately, libc rechecks the predicate and calls again. The core
/// turned out to be occupied by an eternal loop of an already finished process. In the measurements that
/// looked like 94 thousand futex syscalls "out of nowhere" and got in the way of reading
/// the profile. We stop such a thread right here: its context is no longer needed by
/// anybody.
fn stop_if_dead(fr: &Frame) {
    let t = crate::sched::current_id();
    if !crate::sched::task_alive(t) {
        // Where the thread stood IN USER SPACE — for a core dump: its saved
        // context after this point is the exit path in the kernel rather than its own code.
        crate::sched::set_exit_uregs(t, crate::arch::signal::exit_uregs(fr));
        crate::sched::exit_current_code(crate::sched::exit_code_of(t));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_syscall(f: *mut Frame) -> u64 {
    // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate` has
    // already validated.
    let fr = unsafe { &mut *f };
    if SYSPROF.load(Ordering::Relaxed) && fr.nr < 512 {
        // The syscall profile is by request only (`talaria.sysprof=1`). A pair of rdtsc
        // and an atomic increment cost ~30 ns per call with a syscall's price of 100:
        // the diagnostics have no place in the hot path that they themselves measure.
        SYS_COUNT[fr.nr as usize].fetch_add(1, Ordering::Relaxed);
        let t0 = crate::tsc::rdtsc();
        LAST_SYS[crate::sched::current_id()].store((fr.nr << 48) | (fr.pc & 0xFFFF_FFFF_FFFF), Ordering::Relaxed);
        LAST_ARG0[crate::sched::current_id()].store(fr.a1, Ordering::Relaxed);
        let r = rust_syscall_inner(fr);
        SYS_TICKS[fr.nr as usize].fetch_add(crate::tsc::rdtsc().wrapping_sub(t0), Ordering::Relaxed);
        if STRACE_LEFT.load(Ordering::Relaxed) != 0 { strace(fr, r); }
        stop_if_dead(fr);
        if crate::proc::sig_pending() { return deliver_signals(fr, r); }
        return r;
    }
    let me = crate::sched::current_id();
    LAST_SYS[me].store((fr.nr << 48) | (fr.pc & 0xFFFF_FFFF_FFFF), Ordering::Relaxed);
    LAST_ARG0[me].store(fr.a1, Ordering::Relaxed);
    LAST_SYS_T0[me].store(crate::tsc::rdtsc(), Ordering::Relaxed);
    let r = rust_syscall_inner(fr);
    LAST_SYS_T1[me].store(crate::tsc::rdtsc(), Ordering::Relaxed);
    if STRACE_LEFT.load(Ordering::Relaxed) != 0 { strace(fr, r); }
    stop_if_dead(fr);
    // the delivery of the signals at the exit from a syscall (a frame on the user stack, rip -> the handler)
    if crate::proc::sig_pending() { return deliver_signals(fr, r); }
    r
}
/// The syscalls whose first argument is an fd (the translation user fd -> a handle).
fn a1_is_fd(nr: u64) -> bool {
    matches!(nr, 0 | 1 | 3 | 5 | 8 | 16 | 17 | 18 | 19 | 20 | 32 | 33 | 292 | 40 | 42 | 43 | 44 | 45 | 46 | 47 | 48 | 49 | 50 | 51 | 52 | 54 | 55
        | 72 | 73 | 74 | 75 | 77 | 81 | 91 | 93 | 217 | 221 | 232 | 233 | 277 | 281 | 285 | 288 | 299 | 307 | 286 | 287 | 67 | 295 | 296 | 327 | 328)
}
fn rust_syscall_inner(raw: &mut Frame) -> u64 {
    crate::metrics::inc_syscall_of(crate::sched::current_id());
    // The number and the argument order the PROCESS used are its architecture's; the
    // dispatcher's are the kernel's internal ones (x86-64's). `arch::abi` translates —
    // the identity on x86-64, the asm-generic table and `clone`'s tls/ctid swap on arm64
    // (docs/ARM64-PLAN.md §4.4). `raw.nr` keeps the process's number: a restarted call
    // (`SA_RESTART`) puts THAT back into the register.
    // A number the machine's table does not have is reported once, as one the dispatcher
    // does not implement is: a silent ENOSYS hides exactly what a port needs to see.
    let Some(nr) = crate::arch::abi::to_internal(raw.nr) else { return unknown(raw.nr); }; // -ENOSYS
    let mut args = [raw.a1, raw.a2, raw.a3, raw.a4, raw.a5, raw.a6];
    crate::arch::abi::normalize(nr, &mut args);
    if filter_blocks(nr) {
        SYS_DENIED.fetch_add(1, Ordering::Relaxed);
        crate::fr::record(crate::fr::SYSDENY, nr);
        crate::println!("syscall-filter: the syscall #{} IS FORBIDDEN -> EPERM", nr);
        return (-1i64) as u64; // -EPERM
    }
    // Pointer arguments must point into memory the process owns (uaccess.rs):
    // otherwise the handler would read or write kernel memory on its behalf.
    if let Some(e) = crate::uaccess::gate(nr, args) {
        return e;
    }
    // The structures and flag words whose layout is the process's architecture's (`struct
    // stat`, `epoll_event`, the open flags): into the internal form before the handler, back
    // out after it. Nothing on x86-64, whose layouts the internal ones are.
    let user_args = args;
    let mut bounce = crate::arch::abi::Bounce::uninit();
    // SAFETY: S1 — the pointer arguments have just passed `uaccess::gate`.
    unsafe { crate::arch::abi::enter(nr, &mut args, &mut bounce); }
    // the translation user fd -> an internal handle (the process's fd table)
    let mut g = raw.with_args(nr, args);
    if a1_is_fd(nr) { g.a1 = crate::proc::tr(args[0]) as u64; }
    if nr == 233 { g.a3 = crate::proc::tr(args[2]) as u64; }
    if nr == 9 && args[3] & 0x20 == 0 { g.a5 = crate::proc::tr(args[4]) as u64; }
    let r = dispatch(&g, raw);
    // SAFETY: S1 — as `enter`: the same validated pointers, the same buffer.
    let r = unsafe { crate::arch::abi::leave(nr, &user_args, &mut bounce, r) };
    // the new descriptors -> into the process's fd table
    match nr {
        // signalfd4(289) returns a new fd too
        2 | 257 | 41 | 43 | 288 | 291 | 213 | 290 | 283 | 282 | 289 | 53 if (r as i64) >= 0 => { return crate::proc::install(r as i64) as u64; }
        _ => {}
    }
    r
}
/// The dispatcher. Every `unsafe` below is S1 unless the site says otherwise: the
/// pointer arguments of THIS call were validated by `uaccess::gate` in
/// `rust_syscall_inner`, so `f.a1..f.a6` and the addresses read out of the
/// structures they point at belong to the process.
fn dispatch(f: &Frame, raw: &mut Frame) -> u64 {
    // A12: the workload's first syscall goes into the log once. It separates "ring 3 did not
    // come alive" from "it came alive but is silent": on a machine without a console those are two different diagnoses
    // and a different price of an error.
    // A12: `talaria.irr=N` — print the state of the interrupts at the first N syscalls.
    // This is the only place that IS EXECUTED while the workload runs: if
    // there is no preemption, the scheduler does not enter and the pulse is silent, while the syscalls go on.
    // A raised IRR with growing syscalls = "an interrupt IS WAITING to be accepted", that is,
    // the workload executes with the interrupts closed (IF/TPR); a non-raised one means
    // the interrupt does not reach the CPU's core.
    {
        let left = IRR_TRACE.load(Ordering::Relaxed);
        if left > 0 {
            IRR_TRACE.store(left - 1, Ordering::Relaxed);
            crate::println!("a12: the syscall {} rflags={:#x} (IF={}) IRR={} ISR={} the timer's ticks={}",
                f.nr, f.flags, crate::arch::switch::flags_irq_on(f.flags) as u8,
                crate::smp::irr_pending(crate::idt::TIMER_VECTOR as u8),
                crate::smp::isr_pending(crate::idt::TIMER_VECTOR as u8),
                crate::sched::HW_TICKS.load(Ordering::Relaxed));
        }
    }
    if !FIRST_SYSCALL.swap(true, Ordering::Relaxed) {
        crate::println!("syscall: the workload's first syscall: {} (ring 3 is alive)", f.nr);
    }
    match f.nr {
        0 => do_read(f.a1 as i64, f.a2 as *mut u8, f.a3 as usize, 0) as u64,
        45 => { // recvfrom(fd, buf, len, flags, src_addr, addrlen)
            let fd = f.a1 as i64;
            if crate::netsys::is_udp(fd) { crate::netsys::udp_recvfrom(fd, f.a2 as *mut u8, f.a3 as usize, f.a5 as *mut u8, f.a6 as *mut u32, f.a4) as u64 }
            else { do_read(fd, f.a2 as *mut u8, f.a3 as usize, f.a4) as u64 }
        }
        1 => do_write(f.a1 as i64, f.a2 as *const u8, f.a3 as usize, 0) as u64,
        44 => { // sendto(fd, buf, len, flags, dest_addr, addrlen)
            let fd = f.a1 as i64;
            if crate::netsys::is_udp(fd) { crate::netsys::udp_sendto(fd, f.a2 as *const u8, f.a3 as usize, f.a5 as *const u8) as u64 }
            else { do_write(fd, f.a2 as *const u8, f.a3 as usize, f.a4) as u64 }
        }
        46 | 47 => { // sendmsg / recvmsg (fd, msghdr*, flags): msghdr {name@0, namelen@8, iov@16, iovlen@24, control@32, controllen@40, flags@48}
            let m = f.a2 as *const u8;
            if m.is_null() { return (-14i64) as u64; }
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            let (name, iov, iovlen) = unsafe { (core::ptr::read_unaligned(m as *const u64), core::ptr::read_unaligned(m.add(16) as *const u64) as *const u64,
                core::ptr::read_unaligned(m.add(24) as *const u64) as usize) };
            let fd = f.a1 as i64;
            let mut total: i64 = 0;
            for i in 0..iovlen {
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                let base = unsafe { *iov.add(i * 2) };
                let len = unsafe { *iov.add(i * 2 + 1) } as usize;
                if len == 0 { continue; }
                let n = if f.nr == 46 {
                    if crate::netsys::is_udp(fd) { crate::netsys::udp_sendto(fd, base as *const u8, len, name as *const u8) } else { do_write(fd,
                        base as *const u8, len, f.a3) }
                } else {
                    // SAFETY: S1 — `m + 8` is the `msg_namelen` field of the `msghdr` the call
                    // passed, and `uaccess::nested` validated that structure for `recvmsg`.
                    if crate::netsys::is_udp(fd) { crate::netsys::udp_recvfrom(fd, base as *mut u8, len, name as *mut u8, unsafe { m.add(8) } as *mut u32,
                        f.a3) } else { do_read(fd, base as *mut u8, len, f.a3) }
                };
                if n < 0 { if total > 0 { break; } return n as u64; }
                total += n;
                if (n as usize) < len || f.nr == 47 { break; }
            }
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            unsafe { core::ptr::write_unaligned(m.add(40) as *mut u64, 0); } // controllen = 0
            total as u64
        }
        307 | 299 => { // sendmmsg / recvmmsg (fd, mmsghdr*, vlen, flags[, timeout]) — an mmsghdr = a msghdr(56) + msg_len u32 + pad = 64 B
            let fd = f.a1 as i64; let vlen = f.a3 as usize; let base = f.a2 as *mut u8;
            let mut done = 0usize;
            for k in 0..vlen {
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                let m = unsafe { base.add(k * 64) };
                let (name, iov, iovlen) = unsafe { (core::ptr::read_unaligned(m as *const u64),
                    core::ptr::read_unaligned(m.add(16) as *const u64) as *const u64, core::ptr::read_unaligned(m.add(24) as *const u64) as usize) };
                let mut total: i64 = 0; let mut err: i64 = 0;
                for i in 0..iovlen {
                    // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                    // dispatch.
                    let b = unsafe { *iov.add(i * 2) }; let l = unsafe { *iov.add(i * 2 + 1) } as usize;
                    if l == 0 { continue; }
                    let n = if f.nr == 307 {
                        if crate::netsys::is_udp(fd) { crate::netsys::udp_sendto_f(fd, b as *const u8, l, name as *const u8, false) } else { do_write(fd,
                            b as *const u8, l, f.a4) }
                    } else {
                        // SAFETY: S1 — as above, but for one `mmsghdr` of the array: `m` walks it
                        // with a 64-byte stride, and `uaccess::nested` validated the whole array.
                        if crate::netsys::is_udp(fd) { crate::netsys::udp_recvfrom(fd, b as *mut u8, l, name as *mut u8, unsafe { m.add(8) } as *mut u32,
                            f.a4 | 0x40) } else { do_read(fd, b as *mut u8, l, f.a4 | 0x40) }
                    };
                    if n < 0 { err = n; break; }
                    total += n;
                    if (n as usize) < l || f.nr == 299 { break; }
                }
                if err < 0 { if done == 0 { return err as u64; } break; }
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                unsafe { core::ptr::write_unaligned(m.add(56) as *mut u32, total as u32); core::ptr::write_unaligned(m.add(40) as *mut u64, 0); }
                done += 1;
            }
            if f.nr == 307 && crate::netsys::ready() { crate::netsys::poll(); } // one run of the stack per batch
            done as u64
        }
        // socket(domain, type, proto); AF_UNIX/AF_INET6 -> EAFNOSUPPORT
        41 => { if f.a1 == 1 || f.a1 == 10 { (-97i64) as u64 } else { crate::netsys::socket(f.a2) as u64 } }
        49 => crate::netsys::bind(f.a1 as i64, f.a2 as *const u8) as u64,            // bind(fd, addr, len)
        50 => crate::netsys::listen(f.a1 as i64) as u64,                             // listen(fd, backlog)
        42 => crate::netsys::connect(f.a1 as i64, f.a2 as *const u8) as u64,         // connect(fd, addr, len)
        43 => crate::netsys::accept4(f.a1 as i64, f.a2 as *mut u8, f.a3 as *mut u32, 0) as u64,
        288 => crate::netsys::accept4(f.a1 as i64, f.a2 as *mut u8, f.a3 as *mut u32, f.a4 as i64) as u64,
        48 => crate::netsys::shutdown(f.a1 as i64, f.a2 as i64) as u64,
        291 | 213 => crate::netsys::epoll_create() as u64,                           // epoll_create1 / epoll_create
        233 => crate::netsys::epoll_ctl(f.a1 as i64, f.a2 as i64, f.a3 as i64, f.a4 as *const u8) as u64,
        232 | 281 => crate::netsys::epoll_wait(f.a1 as i64, f.a2 as *mut u8, f.a3 as i64, f.a4 as i64) as u64,
        7 | 271 => { // poll(fds, nfds, timeout_ms) / ppoll(fds, nfds, timespec*, sigmask)
            let to = if f.nr == 7 { f.a3 as i64 } else { match read_timespec(f.a3) { Some((s, ns)) => s * 1000 + ns / 1_000_000, None => -1 } };
            poll_translated(f.a1 as *mut u8, f.a2 as usize, to) as u64
        }
        23 | 270 => { // select / pselect6(nfds, readfds, writefds, exceptfds, timeout)
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            let to = if f.nr == 23 { if f.a5 == 0 { -1 }
                     else { unsafe { core::ptr::read_unaligned(f.a5 as *const i64) * 1000
                                     + core::ptr::read_unaligned((f.a5 + 8) as *const i64) / 1000 } } }
                     else { match read_timespec(f.a5) { Some((s, ns)) => s * 1000 + ns / 1_000_000, None => -1 } };
            do_select(f.a1 as usize, f.a2, f.a3, f.a4, to) as u64
        }
        72 => { // fcntl(fd, cmd, arg)
            let fd = f.a1 as i64;
            if fd < 0 { return (-9i64) as u64; }
            match f.a2 {
                0 | 1030 => { // F_DUPFD / F_DUPFD_CLOEXEC: a new user fd >= arg onto the same handle
                    let h = if null_is(fd) { null_alloc() } else { fd };
                    crate::proc::install_min(h, f.a3 as usize) as u64
                }
                1 => crate::proc::fd_get_cloexec(raw.a1 as i64) as u64, // F_GETFD
                2 => crate::proc::fd_set_cloexec(raw.a1 as i64, f.a3 as i64) as u64, // F_SETFD
                3 => { // F_GETFL
                    if crate::netsys::is_sock(fd) { crate::netsys::fcntl(fd, 3, 0) as u64 }
                    else if crate::proc::pipe_is(fd) { (if crate::proc::pipe_get_nonblock(fd) { 0o4000 } else { 0 }) as u64 }
                    else if is_fs(fd) && crate::ext2::active() { (crate::ext2::fd_flags(fd) | 2) as u64 } else { 2 }
                }
                4 => { // F_SETFL
                    if crate::netsys::is_sock(fd) { crate::netsys::fcntl(fd, 4, f.a3 as i64) as u64 }
                    else if crate::proc::pipe_is(fd) { crate::proc::pipe_set_nonblock(fd, f.a3 & 0o4000 != 0); 0 }
                    else if is_fs(fd) && crate::ext2::active() { crate::ext2::fd_set_flags(fd, f.a3 as u32); 0 } else { 0 }
                }
                5 | 6 | 7 | 36 | 37 | 38 => 0, // F_GETLK/F_SETLK/F_SETLKW/the OFD locks -> ok (one host)
                // F_SETPIPE_SZ/F_GETPIPE_SZ: they MUST return THE BUFFER'S SIZE rather than "ok".
                // ClickHouse enlarges a pipe with the loop `for (; size < the desired one; size *= 2)`,
                // and a zero in the answer gave an eternal fcntl loop (the workload did not start at all).
                // Our pipe's buffer is fixed (proc::PBUF) — we return it honestly.
                1031 | 1032 => crate::proc::pipe_capacity() as u64,
                _ => 0,
            }
        }
        54 => crate::netsys::setsockopt(f.a1 as i64, f.a2 as i64, f.a3 as i64, f.a4 as *const u8) as u64,
        55 => crate::netsys::getsockopt(f.a1 as i64, f.a2 as i64, f.a3 as i64, f.a4 as *mut u8, f.a5 as *mut u32) as u64,
        51 => crate::netsys::getsockname(f.a1 as i64, f.a2 as *mut u8, f.a3 as *mut u32) as u64,
        52 => crate::netsys::getpeername(f.a1 as i64, f.a2 as *mut u8, f.a3 as *mut u32) as u64,
        3 => crate::proc::close(raw.a1 as i64) as u64, // close(ufd): drop a reference; the handle is closed at the last one
        318 => { // getrandom(buf, len, flags) — RDRAND (a fallback: a TSC mixer)
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            let dst = unsafe { core::slice::from_raw_parts_mut(f.a1 as *mut u8, f.a2 as usize) }; // (buf, buflen, flags)
            let mut i = 0;
            while i < dst.len() {
                let v = crate::tsc::rdrand64().unwrap_or_else(|| {
                    let t = crate::tsc::rdtsc(); (t ^ (t << 13) ^ (t >> 7)).wrapping_mul(0x9E37_79B9_7F4A_7C15) });
                let b = v.to_le_bytes();
                let n = (dst.len() - i).min(8);
                dst[i..i + n].copy_from_slice(&b[..n]);
                i += n;
            }
            f.a2
        }
        157 => { // prctl(option, arg2, ...)
            let t = crate::sched::current_id();
            if f.a1 == 15 && f.a2 != 0 { // PR_SET_NAME: the thread's name (pthread_setname_np)
                // The name is not a decoration of the log: the manifest lays the threads out
                // over the cores by it (`pin=rx*:dedicated`). So it is decided right here
                // whether the thread should move — and the move is made RIGHT
                // from here, from this syscall's return frame: the thread has
                // the full context in its hands, there will be no second such place.
                let mut name = [0u8; 16];
                let mut n = 0usize;
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                unsafe {
                    while n < 15 {
                        let c = core::ptr::read_volatile((f.a2 as *const u8).add(n));
                        if c == 0 { break; }
                        name[n] = c; n += 1;
                    }
                }
                crate::sched::set_comm(t, &name[..n]);
                if let Some(cpu) = crate::affinity::on_set_name(t, &name[..n]) {
                    if cpu != crate::percpu::cpu_id() && crate::sched::sched_cpu(cpu) {
                        // THE WHOLE REGISTER FILE, the return value 0 — see sched_setaffinity below.
                        let gp = crate::arch::signal::fork_regs(raw); // the REAL frame: f is the dispatcher's translated copy
                        // SAFETY: S3 — `sched::migrate_current_to`'s own contract; here it holds:
                        // the current task, on its kernel stack, with its CR3 active.
                        unsafe { crate::sched::migrate_current_to(cpu, f.pc, f.usp, f.flags, gp, fpu_area(raw)); }
                    }
                }
            }
            if f.a1 == 16 { // PR_GET_NAME -> write the name into buf (16 bytes)
                let mut name = crate::sched::comm(t);
                if name[0] == 0 { name[..8].copy_from_slice(b"talaria\0"); }
                let dst = f.a2 as *mut u8;
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                if !dst.is_null() { unsafe { for i in 0..16 { core::ptr::write(dst.add(i), name[i]); } } }
            }
            0 // the other options -> ok
        }
        39 => (crate::sched::proc_of(crate::sched::current_id()) + 1) as u64, // getpid = the process's leader + 1
        110 => { let pp = crate::sched::ppid_of(crate::sched::proc_of(crate::sched::current_id())); if pp < 0 { 0 } else { (pp + 1) as u64 } } // getppid
        112 | 111 => (crate::sched::proc_of(crate::sched::current_id()) + 1) as u64, // setsid / getpgrp -> pid
        109 | 121 => 0, // setpgid / getpgid -> ok
        61 => { // wait4(pid, wstatus, options, rusage) — wait for a child task to finish
            let want = f.a1 as i64;
            loop {
                if let Some((tid, code)) = crate::sched::reap(want) {
                    // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                    // dispatch.
                    if f.a2 != 0 { unsafe { core::ptr::write_unaligned(f.a2 as *mut i32, code); } } // the wait status (code<<8 or a signal)
                    return (tid + 1) as u64;
                }
                if f.a3 & 1 != 0 { return 0; } // WNOHANG
                if crate::proc::sig_pending() { return (-4i64) as u64; } // EINTR
                wait_window(); // yield -> the child runs (the quantum is given away rather than burnt down in a hlt)
            }
        }
        59 => { // execve(path, argv, envp) — replace the process's image with an ELF from the disk
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            unsafe { do_execve(f.a1, f.a2, f.a3) } // -> ! (it does not return on success)
        }
        21 | 269 | 439 => { // access / faccessat / faccessat2 -> is there a file/directory on the disk
            let p = if f.nr == 21 { f.a1 } else { f.a2 };
            if path_empty(p) { return (-2i64) as u64; } // ENOENT (an empty path is not the cwd)
            if f.nr != 21 && f.a1 as i64 != crate::proc::AT_FDCWD && crate::ext2::active() { return if crate::ext2::exists(crate::proc::resolve_at(f.a1 as i64,
                p)) { 0 } else { (-2i64) as u64 }; }
            if vfs_exists(p) { 0 } else { (-2i64) as u64 }
        }
        95 => 0o22, // umask -> the former mask (a stub)
        38 => 0,    // setitimer -> ok (redis's watchdog timer is optional)
        99 => { // sysinfo(struct*) -> the uptime + the memory
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            if f.a1 != 0 { unsafe {
                core::ptr::write_bytes(f.a1 as *mut u8, 0, 112);
                core::ptr::write_unaligned(f.a1 as *mut i64, (crate::tsc::mono_ns() / 1_000_000_000) as i64); // uptime
                let free = crate::mem::free_frames() * 4096;
                core::ptr::write_unaligned((f.a1 + 32) as *mut u64, free + 256 * 1024 * 1024); // totalram
                core::ptr::write_unaligned((f.a1 + 40) as *mut u64, free);  // freeram
                core::ptr::write_unaligned((f.a1 + 104) as *mut u32, 1);    // mem_unit
            }}
            0
        }
        89 | 267 => { // readlink / readlinkat — the support of /proc/self/fd/N (for musl's realpath)
            let (path_ptr, buf_ptr, bufsz) = if f.nr == 267 { (f.a2, f.a3, f.a4) } else { (f.a1, f.a2, f.a3) };
            const PFX: &[u8] = b"/proc/self/fd/";
            if path_eq_prefix(path_ptr, PFX) {
                let mut fd = 0i64; let mut i = PFX.len();
                loop {
                    // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                    // dispatch.
                    let c = unsafe { core::ptr::read_volatile((path_ptr as *const u8).add(i)) };
                    if !c.is_ascii_digit() { break; }
                    fd = fd * 10 + (c - b'0') as i64; i += 1;
                }
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                let out = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, bufsz as usize) };
                let n = crate::fs::readlink_fd(fd, out);
                if n > 0 { return n as u64; }
            }
            if path_eq(path_ptr, b"/proc/self/exe") { // -> "/<binary>" (the name of the launched binary)
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                let out = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, bufsz as usize) };
                let n = crate::fs::exe_path(out);
                if n > 0 { return n as u64; }
            }
            // a real symbolic link on ext2
            if crate::ext2::active() && !path_empty(path_ptr) {
                let p = if f.nr == 267 { crate::proc::resolve_at(f.a1 as i64, path_ptr) } else { crate::proc::resolve(path_ptr) };
                let r = crate::ext2::readlink(p, buf_ptr as *mut u8, bufsz as usize);
                if r >= 0 { return r as u64; }
                if r != -22 { return r as u64; } // an ENOENT and so on — as they are; an EINVAL (not a link) — below
            }
            // an existing ordinary file/directory -> EINVAL (not a symlink, for realpath), otherwise ENOENT
            if vfs_exists(path_ptr) { (-22i64) as u64 } else { (-2i64) as u64 }
        }
        88 | 266 => { // symlink(target, linkpath) / symlinkat(target, dirfd, linkpath)
            if !crate::ext2::active() { return (-30i64) as u64; } // EROFS
            let (tgt, link) = if f.nr == 266 { (f.a1, f.a3) } else { (f.a1, f.a2) };
            if path_empty(tgt) || path_empty(link) { return (-2i64) as u64; }
            let mut tb = [0u8; 256];
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            let n = unsafe { let mut i = 0; while i < 255 { let c = core::ptr::read_volatile((tgt as *const u8).add(i)); tb[i] = c; if c == 0 { break; } i += 1;
                } i };
            let lp = if f.nr == 266 { crate::proc::resolve_at(f.a2 as i64, link) } else { crate::proc::resolve(link) };
            crate::ext2::symlink(&tb[..n], lp) as u64
        }
        // getuid/getgid/geteuid/getegid -> 1000 (not root: PostgreSQL/MariaDB refuse
        // to work under root; the owner of the files in stat is 1000 too)
        102 | 104 | 107 | 108 => 1000,
        105 | 106 | 113 | 114 | 116 | 117 | 119 | 120 => 0, // setuid/setgid/setreuid/... -> ok
        63 => { // uname(utsname*) — 6 fields of 65 bytes (sysname/nodename/release/version/machine/domain)
            let u = f.a1 as *mut u8;
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            if !u.is_null() { unsafe {
                core::ptr::write_bytes(u, 0, 390);
                // release in the Linux format (glibc's ld.so: "FATAL: kernel too old" with a release < 3.2.0)
                for (off, s) in [(0usize, &b"Linux"[..]), (65, b"talaria"), (130, b"5.15.0-talaria"),
                                 (195, b"#1 SMP Talaria OS ring3"), (260, crate::arch::abi::UTS_MACHINE)] {
                    for (i, &b) in s.iter().enumerate() { core::ptr::write(u.add(off + i), b); }
                }
            }}
            0
        }
        186 => (crate::sched::current_id() + 1) as u64, // gettid
        19 => { // readv(fd, iov, iovcnt)
            let iov = f.a2 as *const u64;
            let mut total: i64 = 0;
            for i in 0..f.a3 as usize {
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                let base = unsafe { *iov.add(i*2) };
                let len = unsafe { *iov.add(i*2+1) } as usize;
                if len == 0 { continue; }
                let n = do_read(f.a1 as i64, base as *mut u8, len, 0);
                if n < 0 { if total > 0 { break; } return n as u64; }
                total += n;
                if (n as usize) < len { break; }
            }
            total as u64
        }
        20 => { // writev(fd, iov, iovcnt)
            let iov = f.a2 as *const u64; // the [base,len] pairs
            // A SOCKET: assemble the pieces into ONE segment.
            //
            // Previously every iovec went as a separate `write`, that is, as a separate
            // TCP segment and a separate call to the card. nginx returns an answer as a
            // writev(the header, the body) — and the client received TWO segments per one
            // answer: an extra packet, an extra recv at the client (3 syscalls per request
            // against Linux's two) and an extra network round trip in the latency.
            // Linux in this place assembles everything into one segment, and that is not
            // an optimisation but the expected behaviour of writev.
            if crate::netsys::is_sock(f.a1 as i64) && f.a3 >= 2 && f.a3 <= 16 {
                let mut total_len = 0usize;
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                for i in 0..f.a3 as usize { total_len += unsafe { *iov.add(i * 2 + 1) } as usize; }
                if total_len > 0 && total_len <= WVBUF {
                    return crate::netsys::hold(|| {
                        let cpu = crate::percpu::cpu_id().min(crate::smp::MAXCPU - 1);
                        // SAFETY: S2 — `WV`: a per-CORE scratch row (`WV[cpu]`), so no two cores
                        // share one.
                        let buf = unsafe { &mut (*core::ptr::addr_of_mut!(WV))[cpu] };
                        let mut off = 0usize;
                        for i in 0..f.a3 as usize {
                            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate`
                            // before this dispatch.
                            let base = unsafe { *iov.add(i * 2) };
                            let len = unsafe { *iov.add(i * 2 + 1) } as usize;
                            if len == 0 { continue; }
                            unsafe { core::ptr::copy_nonoverlapping(base as *const u8, buf.as_mut_ptr().add(off), len); }
                            off += len;
                        }
                        do_write(f.a1 as i64, buf.as_ptr(), off, 0) as u64
                    });
                }
            }
            let mut total: i64 = 0;
            for i in 0..f.a3 as usize {
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                let base = unsafe { *iov.add(i*2) };
                let len = unsafe { *iov.add(i*2+1) } as usize;
                if len == 0 { continue; }
                let n = do_write(f.a1 as i64, base as *const u8, len, 0);
                if n < 0 { if total > 0 { break; } return n as u64; }
                total += n;
                if (n as usize) < len { break; }
            }
            total as u64
        }
        295 | 296 | 327 | 328 => { // preadv / pwritev (/ preadv2 / pwritev2): (fd, iov, iovcnt, offset) — the ext2 files
            let fd = f.a1 as i64; let iov = f.a2 as *const u64; let mut off = f.a4;
            if !(is_fs(fd) && crate::ext2::active()) { return (-29i64) as u64; } // ESPIPE
            let mut total: i64 = 0;
            for i in 0..f.a3 as usize {
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                let base = unsafe { *iov.add(i*2) }; let len = unsafe { *iov.add(i*2+1) } as usize;
                if len == 0 { continue; }
                let n = if f.nr == 295 || f.nr == 327 { crate::ext2::pread(fd, base as *mut u8, len, off) } else { crate::ext2::pwrite(fd, base as *const u8,
                    len, off) };
                if n < 0 { if total > 0 { break; } return n as u64; }
                total += n; off += n as u64;
                if (n as usize) < len { break; }
            }
            total as u64
        }
        // SAFETY: S2 — the POSIX shm table (`SHM_*`): a row is written under the FS's own
        // serialisation, by the process that created or mapped the object.
        77 => { if shm_is(f.a1 as i64) { unsafe { SHM_SIZE[shm_obj(f.a1 as i64)] = f.a2; } 0 } else if is_fs(f.a1 as i64) && crate::ext2::active() {
            crate::ext2::ftruncate(f.a1 as i64, f.a2) as u64 } else { 0 } } // ftruncate
        76 => { if crate::ext2::active() { crate::ext2::truncate(crate::proc::resolve(f.a1), f.a2) as u64 } else { (-30i64) as u64 } } // truncate
        17 => { if is_fs(f.a1 as i64) && crate::ext2::active() { crate::ext2::pread(f.a1 as i64, f.a2 as *mut u8, f.a3 as usize, f.a4) as u64 }
            else if is_fs(f.a1 as i64) { let fd = f.a1 as i64; crate::fs::lseek(fd, f.a4 as i64, 0); crate::fs::read(fd, f.a2 as *mut u8,
            f.a3 as usize) as u64 } else { do_read(f.a1 as i64, f.a2 as *mut u8, f.a3 as usize, 0) as u64 } } // pread64
        18 => { if is_fs(f.a1 as i64) && crate::ext2::active() { crate::ext2::pwrite(f.a1 as i64, f.a2 as *const u8, f.a3 as usize, f.a4) as u64 } else {
            do_write(f.a1 as i64, f.a2 as *const u8, f.a3 as usize, 0) as u64 } } // pwrite64
        // fallocate
        285 => { if is_fs(f.a1 as i64) && crate::ext2::active() && f.a2 == 0 { crate::ext2::fallocate(f.a1 as i64, f.a3, f.a4) as u64 } else { 0 } }
        221 | 277 | 73 => 0, // fadvise64 / sync_file_range / flock -> ok
        90 | 91 | 268 | 92 | 93 | 94 | 260 | 280 | 235 | 132 => 0, // chmod/fchmod/fchmodat/chown/fchown/lchown/fchownat/utimensat/utimes/utime -> ok
        86 | 265 => (-1i64) as u64, // link/linkat -> EPERM (symlink/symlinkat are implemented above)
        29 => { let _g = shm_lock(); sysv_shmget(f.a1, f.a2, f.a3) as u64 }  // shmget(key, size, flags)
        30 => { let _g = shm_lock(); sysv_shmat(f.a1, f.a2) as u64 }         // shmat(id, addr, flags) -> the address
        31 => { let _g = shm_lock(); sysv_shmctl(f.a1, f.a2, f.a3) as u64 }  // shmctl(id, cmd, buf)
        67 => 0, // shmdt -> ok
        262 => { // newfstatat(dirfd, path, statbuf, flags)
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            if f.a4 & 0x1000 != 0 && f.a2 != 0 && unsafe { core::ptr::read_volatile(f.a2 as *const u8) } == 0 { // AT_EMPTY_PATH -> fstat(dirfd)
                let fd = crate::proc::tr(f.a1);
                return fstat_any(fd, f.a3 as *mut u8) as u64;
            }
            vfs_stat_at(f.a1 as i64, f.a2, f.a3 as *mut u8) as u64
        }
        4 => vfs_stat(f.a1, f.a2 as *mut u8) as u64,   // stat — following the link
        6 => { // lstat — about THE LINK ITSELF (otherwise `ls -l` and realpath do not see the links)
            if path_empty(f.a1) { return (-2i64) as u64; }
            if crate::ext2::active() { crate::ext2::lstat(crate::proc::resolve(f.a1), f.a2 as *mut u8) as u64 }
            else { vfs_stat(f.a1, f.a2 as *mut u8) as u64 }
        }
        332 => { // statx(dirfd, path, flags, mask, statxbuf) -> through stat
            let mut st = [0u8; 144];
            let r = if f.a3 & 0x1000 != 0 { fstat_any(crate::proc::tr(f.a1), st.as_mut_ptr()) }
                    else { vfs_stat(f.a2, st.as_mut_ptr()) };
            if r < 0 { return r as u64; }
            let x = f.a5 as *mut u8;
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            unsafe {
                core::ptr::write_bytes(x, 0, 256);
                core::ptr::write_unaligned(x as *mut u32, 0x7ff); // stx_mask: basic stats
                core::ptr::write_unaligned(x.add(4) as *mut u32, 4096); // blksize
                core::ptr::write_unaligned(x.add(16) as *mut u32, 1); // nlink
                core::ptr::write_unaligned(x.add(28) as *mut u16, core::ptr::read_unaligned(st.as_ptr().add(24) as *const u32) as u16); // mode
                core::ptr::write_unaligned(x.add(40) as *mut u64, core::ptr::read_unaligned(st.as_ptr().add(48) as *const i64) as u64); // size
                core::ptr::write_unaligned(x.add(32) as *mut u64, core::ptr::read_unaligned(st.as_ptr().add(8) as *const u64)); // ino
                core::ptr::write_unaligned(x.add(48) as *mut u64, core::ptr::read_unaligned(st.as_ptr().add(64) as *const i64) as u64); // blocks
                core::ptr::write_unaligned(x.add(112) as *mut i64, core::ptr::read_unaligned(st.as_ptr().add(88) as *const i64)); // mtime sec
            }
            0
        }
        87 | 84 | 263 => { // unlink / rmdir / unlinkat(dirfd, path, flags)
            let (p, dir) = if f.nr == 87 { (f.a1, false) } else if f.nr == 84 { (f.a1, true) } else { (f.a2, f.a3 & 0x200 != 0) };
            if shm_name_of(p).is_some() { return 0; } // shm_unlink -> ok (the objects live until a restart)
            let dfd = if f.nr == 263 { f.a1 as i64 } else { crate::proc::AT_FDCWD };
            if crate::ext2::active() { crate::ext2::unlink(crate::proc::resolve_at(dfd, p), dir) as u64 } else { 0 }
        }
        83 | 258 => { // mkdir / mkdirat
            let (p, mode) = if f.nr == 83 { (f.a1, f.a2) } else { (f.a2, f.a3) };
            let dfd = if f.nr == 258 { f.a1 as i64 } else { crate::proc::AT_FDCWD };
            if crate::ext2::active() { crate::ext2::mkdir(crate::proc::resolve_at(dfd, p), mode as u32) as u64 }
            else if crate::fs::exists(p as *const u8) { (-17i64) as u64 } else { 0 }
        }
        82 | 264 | 316 => { // rename / renameat / renameat2
            let (o, n) = if f.nr == 82 { (f.a1, f.a2) } else { (f.a2, f.a4) };
            let (od, nd) = if f.nr == 82 { (crate::proc::AT_FDCWD, crate::proc::AT_FDCWD) } else { (f.a1 as i64, f.a3 as i64) };
            if crate::ext2::active() { let op = crate::proc::resolve_at_slot(od, o, 0); let np = crate::proc::resolve_at_slot(nd, n, 1); crate::ext2::rename(op,
                np) as u64 } else { 0 }
        }
        79 => crate::proc::getcwd(f.a1, f.a2) as u64, // getcwd
        80 => { // chdir(path)
            if crate::ext2::active() { let p = crate::proc::resolve(f.a1); if crate::ext2::is_dir_path(p) { crate::proc::set_cwd(p) as u64 }
                else if crate::ext2::exists(p) { (-20i64) as u64 } else { (-2i64) as u64 } } else { 0 }
        }
        81 => 0, // fchdir -> ok (without a change)
        // getdents64
        217 => { if is_fs(f.a1 as i64) && crate::ext2::active() { crate::ext2::getdents64(f.a1 as i64, f.a2 as *mut u8, f.a3 as usize) as u64 } else { 0 } }
        // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's owner from
        // the CAS in `null_alloc_kind` until `null_free`.
        290 => { let h = null_alloc_kind(4); if h >= 0 { unsafe { let i = (h - NULL_BASE) as usize; NULL_CTR[i] = f.a1; NULL_GEN[i] = 1;
            NULL_FL[i] = ((f.a2 & 0o4000 != 0) as u8) | (((f.a2 & 1 != 0) as u8) << 1); } } h as u64 } // eventfd2(initval, flags)
        283 => { let h = null_alloc_kind(5); if h >= 0 { unsafe { let i = (h - NULL_BASE) as usize; NULL_CTR[i] = 0; NULL_IVL[i] = 0;
            NULL_FL[i] = ((f.a2 & 0o4000 != 0) as u8) | (((f.a1 == 0) as u8) << 2); } } h as u64 } // timerfd_create(clockid, flags)
        282 | 289 => { // signalfd / signalfd4(fd, mask*, sizemask, flags): the pending signals from mask as an fd
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            let mask = if f.a2 != 0 { unsafe { core::ptr::read_unaligned(f.a2 as *const u64) } } else { 0 };
            let fd = f.a1 as i64;
            // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's owner
            // from the CAS in `null_alloc_kind` until `null_free`.
            if fd >= 0 { let h = crate::proc::tr(fd as u64); if null_is(h) && unsafe { NULL_KIND[(h - NULL_BASE) as usize] } == 6 { unsafe {
                NULL_CTR[(h - NULL_BASE) as usize] = mask; } return fd as u64; } return (-22i64) as u64; }
            let h = null_alloc_kind(6); if h >= 0 { unsafe { let i = (h - NULL_BASE) as usize; NULL_CTR[i] = mask;
                NULL_FL[i] = (f.nr == 289 && f.a4 & 0o4000 != 0) as u8; } }
            h as u64
        }
        286 => { // timerfd_settime(fd, flags, new*, old*): itimerspec {interval, value}
            // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's owner
            // from the CAS in `null_alloc_kind` until `null_free`.
            let fd = f.a1 as i64; if !null_is(fd) || unsafe { NULL_KIND[(fd - NULL_BASE) as usize] } != 5 { return (-9i64) as u64; }
            let i = (fd - NULL_BASE) as usize;
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            let (is, ins, vs, vns) = unsafe { (core::ptr::read_unaligned(f.a3 as *const i64), core::ptr::read_unaligned((f.a3 + 8) as *const i64),
                core::ptr::read_unaligned((f.a3 + 16) as *const i64), core::ptr::read_unaligned((f.a3 + 24) as *const i64)) };
            let val = (vs.max(0) as u64) * 1_000_000_000 + vns.max(0) as u64;
            // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's owner
            // from the CAS in `null_alloc_kind` until `null_free`.
            unsafe {
                if f.a4 != 0 { core::ptr::write_bytes(f.a4 as *mut u8, 0, 32); }
                NULL_IVL[i] = (is.max(0) as u64) * 1_000_000_000 + ins.max(0) as u64;
                NULL_CTR[i] = if val == 0 { 0 }
                    else if f.a2 & 1 != 0 { if NULL_FL[i] & 4 != 0 { val.saturating_sub(crate::tsc::wall_ns()) + crate::tsc::mono_ns() } else { val } }
                    else { crate::tsc::mono_ns() + val };
            }
            0
        }
        // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this dispatch.
        287 => { if f.a2 != 0 { unsafe { core::ptr::write_bytes(f.a2 as *mut u8, 0, 32); } } 0 } // timerfd_gettime -> zeroes
        22 | 293 => { // pipe / pipe2(fds*, flags) -> a real pipe (a 16K ring, an EOF/HUP at the closing of the ends)
            let a = f.a1 as *mut i32;
            if a.is_null() { return (-14i64) as u64; }
            match crate::proc::pipe_create(f.nr == 293 && f.a2 & 0o4000 != 0) {
                Some((hr, hw)) => {
                    let r = crate::proc::install(hr); let w = crate::proc::install(hw);
                    if r < 0 || w < 0 { return (-24i64) as u64; }
                    // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                    // dispatch.
                    unsafe { core::ptr::write(a, r as i32); core::ptr::write(a.add(1), w as i32); }
                    0
                }
                None => (-24i64) as u64,
            }
        }
        // dup
        32 => { let h = f.a1 as i64; if h < 0 { return (-9i64) as u64; } let h = if null_is(h) { null_alloc() } else { h }; crate::proc::install(h) as u64 }
        33 | 292 => { let h = f.a1 as i64; if h < 0 { return (-9i64) as u64; } let h = if null_is(h) { null_alloc() } else { h }; crate::proc::install_at(h,
            f.a2 as i64) as u64 } // dup2 / dup3
        62 => crate::proc::kill(f.a1 as i64, f.a2) as u64,        // kill(pid, sig)
        200 => crate::proc::tkill(f.a1 as i64, f.a2) as u64,      // tkill(tid, sig)
        234 => crate::proc::tkill(f.a2 as i64, f.a3) as u64,      // tgkill(tgid, tid, sig)
        13 => crate::proc::sigaction(f.a1, f.a2, f.a3) as u64,    // rt_sigaction(sig, act, old, sigsetsize)
        14 => crate::proc::sigprocmask(f.a1, f.a2, f.a3) as u64,  // rt_sigprocmask(how, set, old, size)
        15 => sigreturn(raw),                                      // rt_sigreturn — restore the context
        // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this dispatch.
        127 => { if f.a1 != 0 { unsafe { core::ptr::write_unaligned(f.a1 as *mut u64, 0); } } 0 } // rt_sigpending -> empty
        131 => sigaltstack(f.a1, f.a2),                            // sigaltstack(new, old)
        125 => { // capget(hdr, data): there are no capabilities — zero into all three sets
            // ClickHouse asks for CAP_NET_ADMIN in order to decide whether to take the tasks'
            // statistics through netlink. It prints an ENOSYS as AN ERROR at every request;
            // an honest answer "there are no rights" sends it along the ordinary path silently.
            // The answer's size depends on the version in the header: v1 is one block (12 B),
            // v2/v3 are two (24 B). Writing 24 always would mean climbing past the buffer of a
            // program built for v1.
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            let ver = if f.a1 != 0 { unsafe { core::ptr::read_unaligned(f.a1 as *const u32) } } else { 0 };
            let n = if ver == 0x1998_0330 { 12 } else { 24 };
            if f.a2 != 0 { unsafe { core::ptr::write_bytes(f.a2 as *mut u8, 0, n); } }
            0
        }
        126 => (-1i64) as u64,                                     // capset -> EPERM (there is nothing to change)
        128 => { // rt_sigtimedwait(set*, siginfo*, timeout*, sigsetsize): wait for a signal from set (the sigwait of MariaDB's handler thread)
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            let set = if f.a1 != 0 { unsafe { core::ptr::read_unaligned(f.a1 as *const u64) } } else { 0 };
            let deadline = read_timespec(f.a3).map(|(s, ns)| crate::tsc::mono_ns() + (s.max(0) as u64) * 1_000_000_000 + ns.max(0) as u64);
            loop {
                if let Some(sig) = crate::proc::take_pending_in(set) {
                    // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                    // dispatch.
                    if f.a2 != 0 { unsafe { core::ptr::write_bytes(f.a2 as *mut u8, 0, 128); core::ptr::write_unaligned(f.a2 as *mut i32, sig as i32); } }
                    return sig;
                }
                if let Some(d) = deadline { if crate::tsc::mono_ns() >= d { return (-11i64) as u64; } } // EAGAIN
                wait_window();
            }
        }
        // SAFETY: S4 — `sti; hlt; cli` on THIS core: `rt_sigsuspend` waits for a signal, and
        // the window with interrupts open is exactly one instruction wide, so a signal that
        // arrives cannot be missed between the check above and the `hlt`.
        130 => { if crate::proc::sig_pending() { (-4i64) as u64 } else { unsafe { crate::arch::cpu::irq_enable_halt_disable(); }
            (-4i64) as u64 } } // rt_sigsuspend
        273 => 0, // set_robust_list -> ok (sigaltstack is implemented above)
        334 => ENOSYS, // rseq: not supported (glibc switches rseq off at an ENOSYS; a "success" without updating the area would break sched_getcpu)
        302 | 97 => { // prlimit64(pid, res, new, old) / getrlimit(res, rlim)
            // A LIMIT IS A PROMISE, SO IT IS TAKEN FROM WHAT THE KERNEL CAN ACTUALLY DELIVER.
            //
            // These three numbers used to be literals — NOFILE 1024, NPROC 4096, STACK 32 MB —
            // and all three were larger than the ceiling behind them: the fd table is
            // `proc::FDN` = 256 rows, the scheduler has `sched::NTASKS` slots, and the main
            // stack is 8 MB in `mode=proc` (32 only in `mode=cell`). An application that sizes
            // itself by the limit then walks into an EMFILE, an EAGAIN from clone or a guard
            // page at a quarter of the room it was promised — and it walks in there minutes
            // later, far from the syscall that lied. That is the same class as a `fsync` that
            // returns 0 without writing: the refusal is not visible in any log.
            //
            // Hence: the answer is the ceiling, and an attempt to raise it above the ceiling is
            // REFUSED the way Linux refuses it to an unprivileged process (EPERM) rather than
            // accepted with a zero. Lowering a limit for oneself is accepted and not enforced —
            // that is stated here rather than hidden, and it cannot cost the workload anything
            // it was not already able to do.
            let (res, new, out) = if f.nr == 302 { (f.a2, f.a3, f.a4) } else { (f.a1, 0, f.a2) };
            let ceiling: u64 = match res {
                3 => crate::user::stack_limit(),            // RLIMIT_STACK — what was really mapped
                6 => crate::sched::NTASKS as u64,           // RLIMIT_NPROC — the scheduler's slots
                7 => crate::proc::FDN as u64,               // RLIMIT_NOFILE — the rows of the fd table
                _ => u64::MAX,
            };
            if new != 0 {
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch (`rd_opt(3, Fixed(RLIMIT))` for prlimit64).
                let (cur, max) = unsafe {
                    (core::ptr::read_unaligned(new as *const u64), core::ptr::read_unaligned((new + 8) as *const u64))
                };
                if cur > max { return (-22i64) as u64; }      // EINVAL, as in Linux
                if max > ceiling { return (-1i64) as u64; }   // EPERM: we cannot make the ceiling higher
            }
            if out != 0 {
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                unsafe { core::ptr::write_unaligned(out as *mut u64, ceiling); core::ptr::write_unaligned((out + 8) as *mut u64, ceiling); }
            }
            0
        }
        // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this dispatch.
        98 => { if f.a2 != 0 { unsafe { core::ptr::write_bytes(f.a2 as *mut u8, 0, 144); } } 0 } // getrusage -> zeroes
        100 => 0, // times
        137 | 138 => { // statfs(path, buf) / fstatfs(fd, buf)
            // Previously the buffer was simply zeroed here and a success was returned. For
            // an application that means "there is a file system, and it has ZERO
            // free blocks": ClickHouse, honestly reserving room for
            // a part of a table, immediately fell with "Cannot reserve 4.56 MiB, not
            // enough space". A silent zero is worse than a refusal — a refusal is visible.
            let buf = f.a2; // statfs(path, buf) and fstatfs(fd, buf): the buffer is the second argument in both
            if buf == 0 { return (-14i64) as u64; } // EFAULT
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            unsafe { core::ptr::write_bytes(buf as *mut u8, 0, 120); }
            if !crate::ext2::active() { return 0; }
            let (bsize, blocks, bfree, files, ffree) = crate::ext2::statfs();
            unsafe {
                let p = buf as *mut u64;
                core::ptr::write_unaligned(p, 0xEF53);            // f_type: ext2/3/4
                core::ptr::write_unaligned(p.add(1), bsize as u64); // f_bsize
                core::ptr::write_unaligned(p.add(2), blocks as u64); // f_blocks
                core::ptr::write_unaligned(p.add(3), bfree as u64);  // f_bfree
                core::ptr::write_unaligned(p.add(4), bfree as u64);  // f_bavail (we do not allocate a root reserve)
                core::ptr::write_unaligned(p.add(5), files as u64);  // f_files
                core::ptr::write_unaligned(p.add(6), ffree as u64);  // f_ffree
                core::ptr::write_unaligned(p.add(8), 255);           // f_namelen
                core::ptr::write_unaligned(p.add(9), bsize as u64);  // f_frsize
            }
            0
        }
        24 => { // sched_yield: give up the quantum to the BSP scheduler's rotation. It was an sti;hlt —
                // the caller EXTINGUISHED the core until the next interrupt (up to 1 ms of a tick + a vmexit):
                // the spin phases with a yield (so_5's combined_lock and the like) degraded into ticks.
                // An AP is run-to-completion: there is nobody to yield to — a no-op.
            // On a core of the common pool a yield MUST give up the processor if there is
            // somebody to run there. Previously it was a no-op on all the cores except the control
            // plane — for the sake of run-to-completion on a dedicated core. But for a thread
            // that moved to an AP by a steal or by the layout that turns into an eternal loop:
            // `while (the condition) sched_yield();` holds the core, while the one that must
            // fulfil the condition stands next to it and does not get the processor. The condition
            // is preserved: if one is alone on the core, we do not yield (a dedicated core has nobody
            // to yield to by construction, run-to-completion is untouched).
            // IF THERE IS NOBODY TO YIELD TO, WE DO NOT YIELD, on the control plane too.
            // Previously on core 0 there was an unconditional entry into the scheduler, and that was
            // the most expensive no-op in the system: an `int 0x41`, a save/restore of
            // the FPU, a walk over the queue — for the sake of returning into the same task.
            // With ClickHouse a pipeline's executor waits for tasks with a spin with a yield:
            // 4282 calls over a run of thirty-six queries, that is, more
            // than the futex waits. On Linux a `sched_yield` with an empty queue costs
            // a hundred nanoseconds, with us it cost 5.8 us — and the difference fell straight
            // into the request's path.
            if THREADED.load(Ordering::Relaxed) {
                let cpu = crate::percpu::cpu_id();
                let me = crate::sched::current_id();
                if crate::sched::cpu_has_other_runnable(cpu, me) { crate::sched::yield_origin(crate::sched::YO_SYSYIELD); crate::sched::yield_now(); }
            }
            0
        }
        144 | 141 | 142 | 148 | 149 | 151 | 152 => 0, // sched_setscheduler/setpriority/getpriority... mlock/munlock/mlockall/munlockall -> ok
        324 => { // membarrier(cmd, flags, cpu_id)
            // Previously there was an unconditional zero here — "done", without a single
            // action. That is exactly the kind of answer that must not be given: a runtime
            // asks for a barrier because its OWN one is lowered on the fast path, and
            // a quiet success means that the order of the writes between the cores is
            // provided by nobody. There is a real barrier in the kernel (an IPI + a wait for the
            // acknowledgements: the iret on the target serialises, its write buffer is drained).
            match f.a1 {
                0 => (1 | 2 | 4 | 8 | 16 | 32 | 64) as u64, // QUERY: what we can do
                4 | 16 | 64 => 0,                           // the registration — there is nothing to do
                1 | 2 | 8 | 32 => { crate::sync::membarrier(); 0 }
                _ => (-22i64) as u64,                       // EINVAL
            }
        }
        28 => { // madvise(addr, len, advice)
            // MADV_DONTNEED (4) on anonymous private memory is NOT a hint:
            // Linux must give the frames away and return ZEROES at the next access.
            // That is how the runtimes (the JVM, jemalloc, glibc) "uncommit" pages; a silent
            // "ok" would leave the old data there — a quiet corruption of the heap instead of zeroes.
            if f.a3 == 4 && f.a2 > 0 && crate::sched::task_cr3(crate::sched::current_id()) != 0
                && f.a1 >= 0x1_0000_0000 {
                crate::paging::relazy_pages(f.a1 & !0xFFF, f.a2.div_ceil(4096));
            }
            0
        }
        4601 => crate::sync::mode() as u64, // tsync_kmode(): the kernel wait's mode (0 hlt / 1 mwait / 2 mwaitx / 3 waitpkg)
        4602 => { // upark(addr, val, deadline_abs_mono_ns|0, kind, state_ptr) — PLAN-SYNC SPSC v2:
                  // kind1 = wait while (i32)(*addr - val) < 0 (the hysteresis: a re-arm of the monitor IN THE KERNEL,
                  // without a syscall at every change); state: 1 = an mwait on the word (a store wakes,
                  // the waker does NOT need a wake syscall), 2 = parked (a futex wake is needed).
            let addr = f.a1 as *const u32;
            if f.a1 == 0 { return (-14i64) as u64; }
            let deadline = if f.a3 != 0 { Some(f.a3) } else { None };
            crate::sync::wait_on_ex(addr, f.a2 as u32, deadline, f.a4 & 7, f.a5, f.a6) as u64
        }
        4600 => { // swtchto(tid) — PLAN-SYNC stage 4: give the remainder of the slice to the task tid
                  // (a directed switch past the rotation; the heritage of Google's switchto/UMCG)
            let t = (f.a1 as usize).wrapping_sub(1);
            if t >= crate::sched::NTASKS || !crate::sched::task_alive(t) { return (-3i64) as u64; } // ESRCH
            if crate::percpu::cpu_id() != 0 || crate::sched::cpu_of(t) != 0 { return 0; } // the target/we are on an AP — a no-op
            crate::sched::yield_origin(crate::sched::YO_SWTCH); crate::sched::yield_to(t);
            0
        }
        203 => { // sched_setaffinity(pid, len, mask*) — PLAN-SYNC: a migration of ONE'S OWN thread onto an AP
            let t = crate::sched::current_id();
            if f.a2 < 8 || f.a3 == 0 { return (-22i64) as u64; }
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            let mask = unsafe { core::ptr::read_unaligned(f.a3 as *const u64) };
            if mask == 0 { return (-22i64) as u64; }
            if f.a1 != 0 && f.a1 != (t + 1) as u64 { return 0; } // a foreign thread: a PoC — a no-op
            // Remember THE REQUESTED mask. Without that `sched_getaffinity` returned
            // all the online cores regardless of the affinity, and the workload laid out its thread
            // pool not by the set on which it was asked to work.
            // An empty intersection with the online ones is an EINVAL, as in Linux: a silent
            // agreement would mean that the thread works not where it was asked to.
            if !crate::affinity::set_affinity(t, mask) {
                if !AFF_WARNED.swap(true, Ordering::SeqCst) {
                    crate::println!("talaria: sched_setaffinity: the core {} is not among the online ones ({:#x}) — EINVAL",
                        mask.trailing_zeros(), crate::percpu::ONLINE.load(Ordering::SeqCst));
                }
                return (-22i64) as u64;
            }
            let cur_cpu = crate::percpu::cpu_id();
            if mask & (1u64 << cur_cpu) != 0 { return 0; } // it is already where it was asked to be
            let want = (mask & crate::percpu::ONLINE.load(Ordering::SeqCst)).trailing_zeros() as usize;
            if cur_cpu != 0 || want == 0 { return 0; }     // a move only from the BSP (a PoC)
            // The context of the return from THIS syscall (the return value 0) moves onto the AP
            // WITH THE WHOLE REGISTER FILE. A syscall preserves every register but the return
            // value (x86 also rcx/r11, which sysret takes), and compiled code relies on it: an
            // inline `svc` keeps its temporaries in x9..x18 and lr across the call. Only the
            // callee-saved ones were carried, and a ClickHouse thread moved by its name
            // (`prctl(PR_SET_NAME)` above) came back with x0..x18 and lr zero and returned to
            // address 0 — arm64, 6 runs of 6 with the stealing push off; x86 lost rdi..r10 the same way.
            let gp = crate::arch::signal::fork_regs(raw); // the REAL frame: f is the dispatcher's translated copy
            // IMPORTANT: fpu_area comes from the REAL frame (raw) rather than from the copy g (dispatch
            // passes a copy for the fd translation; the address arithmetic over it is garbage)
            // SAFETY: S3 — `sched::migrate_current_to`'s own contract; here it holds: the current
            // task, on its kernel stack, with its CR3 active.
            unsafe { crate::sched::migrate_current_to(want, f.pc, f.usp, f.flags, gp, fpu_area(raw)); }
        }
        // NUMA policies: a process's memory comes from one node (`affinity::my_node`), so a
        // policy is accepted and changes nothing, and the policy reported is the default. arm64's
        // ClickHouse asks `get_mempolicy` at start-up (an ENOSYS it tolerated, an unknown syscall
        // the gates refuse).
        237 | 238 => 0, // mbind, set_mempolicy
        239 => { // get_mempolicy(mode*, nodemask*, maxnode, addr, flags)
            // SAFETY: S1 — the pointer arguments were validated by `uaccess::gate` (4 and 8 bytes).
            unsafe {
                if f.a1 != 0 { core::ptr::write_unaligned(f.a1 as *mut i32, 0); } // MPOL_DEFAULT
                if f.a2 != 0 && f.a3 != 0 { core::ptr::write_bytes(f.a2 as *mut u8, 0, (f.a3.div_ceil(8) as usize).min(8)); }
            }
            0
        }
        204 => { // sched_getaffinity(pid, len, mask*) — the cores where the thread IS ALLOWED to live
            // This is what the workload counts nproc/hardware_concurrency by and starts
            // pools of workers by, so foreign DEDICATED cores must not get here:
            // otherwise the application would itself settle workers on a core it asked
            // to be given to its own latency-critical thread (see affinity::allowed_mask).
            if f.a2 >= 8 && f.a3 != 0 {
                let m = crate::affinity::allowed_mask(crate::sched::current_id());
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                unsafe { core::ptr::write_bytes(f.a3 as *mut u8, 0, f.a2 as usize); core::ptr::write_unaligned(f.a3 as *mut u64, m); }
            }
            8
        }
        309 => { // getcpu(cpu*, node*, tcache) — the LOGICAL core number (A10)
            // It is needed both by the workload (musl's/glibc's sched_getcpu, NUMA-aware pools) and by
            // the tests: without it "a thread is on its own core" is checked only by
            // the indirect metric of the speedup, which is easy to deceive.
            let c = crate::percpu::cpu_id() as u32;
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            if f.a1 != 0 { unsafe { core::ptr::write_unaligned(f.a1 as *mut u32, c); } }
            if f.a2 != 0 {
                // The node from the SRAT (255 = there is no table -> the machine has one, we return 0).
                let nd = crate::affinity::node_of(c as usize);
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                unsafe { core::ptr::write_unaligned(f.a2 as *mut u32, if nd == 255 { 0 } else { nd as u32 }); }
            }
            0
        }
        145 => 0,   // sched_getscheduler -> SCHED_OTHER
        146 => 99,  // sched_get_priority_max
        147 => 1,   // sched_get_priority_min
        10 => { // mprotect(addr,len,prot): PROT_READ=1 WRITE=2 EXEC=4 — we really change the PTE's rights
            let (addr, len, prot) = (f.a1, f.a2, f.a3);
            // We touch only what is NOT the kernel's image (the boundary is taken from
            // the linkage rather than from the constant 32 MB: with a build with a high base the user code
            // lies exactly in the low addresses).
            if len > 0 && addr >= 0x20_0000 && (addr >= crate::mem::kernel_hi() || addr + len <= crate::mem::kernel_lo()) {
                if prot == 0 { // PROT_NONE — non-present + the PNONE mark (munmap will free the frame)
                    crate::paging::protect_none(addr, len);
                } else {
                    crate::paging::protect(addr, len, prot & 2 != 0, prot & 4 != 0);
                }
                crate::fr::record(crate::fr::MPROTECT, addr);
            }
            0
        }
        60 => { // exit — the end of A THREAD (or of the process, if it is single-threaded)
            if THREADED.load(Ordering::SeqCst) {
                let p = crate::sched::proc_of(crate::sched::current_id());
                if crate::sched::proc_task_count(p) <= 1 { let c = ((f.a1 & 0xff) << 8) as i32; crate::sched::exit_all(c); crate::proc::exit_cleanup(p);
                    crate::sched::queue_dead_space(crate::sched::task_cr3(p)); crate::sched::exit_current_code(c); }
                crate::sched::exit_current_code(((f.a1 & 0xff) << 8) as i32);
            }
            EXIT_CODE.store(f.a1, Ordering::SeqCst); EXITED.store(true, Ordering::SeqCst);
            crate::println!("[syscall] exit(the code={}) — the binary has finished in ring 3", f.a1);
            crate::supervise::process_exited(f.a1 as i32) // -> !
        }
        231 => { // exit_group — the end of THE PROCESS (all the threads)
            crate::println!("[syscall] exit_group(the code={}) — the process has finished", f.a1);
            EXIT_CODE.store(f.a1, Ordering::SeqCst); EXITED.store(true, Ordering::SeqCst);
            if THREADED.load(Ordering::SeqCst) {
                let c = ((f.a1 & 0xff) << 8) as i32;
                // First all the process's threads are dead AND have stopped (having run to the end what
                // they started in the kernel), then the cleanup: the files, the journal and the address
                // space must not go out from under a thread in the middle of a syscall.
                crate::sched::exit_all(c); // all the threads of THIS process
                crate::sched::drain_proc(1000);
                crate::ext2::flush_all(); // the process's dirty blocks go to the disk (like a clever "sync at exit")
                crate::proc::exit_cleanup(crate::sched::proc_of(crate::sched::current_id()));
                crate::sched::queue_dead_space(crate::sched::task_cr3(crate::sched::current_id())); // the AS will be freed by the scheduler
                // Under the supervisor control will be returned to it by the BSP ("all the tasks have
                // finished") rather than by this thread: it may stand on any AP.
                if crate::supervise::RECOVERY_ARMED.load(Ordering::SeqCst) { crate::supervise::exit_pending(f.a1 as i32); }
                crate::sched::exit_current_code(c); // the scheduler will pick up the other processes / the summary
            }
            crate::supervise::process_exited(f.a1 as i32); // single-threaded: under the supervisor -> a longjmp(3); otherwise a hlt
        }
        158 => { // arch_prctl(code, addr)
            if f.a1 == 0x1002 { // ARCH_SET_FS -> IA32_FS_BASE (TLS)
                // SAFETY: S4 — `ARCH_SET_FS` writes the FS base of THIS core, which is the
                // current task's TLS pointer; the scheduler saves and restores it per task.
                unsafe { set_tls_base(f.a2); }
                return 0;
            }
            // SAFETY: S1 + S4 — the FS base of this core is read and stored into the argument's
            // 8 bytes, which `uaccess::gate` validated for `arch_prctl`.
            if f.a1 == 0x1003 { if f.a2 != 0 { unsafe { core::ptr::write_unaligned(f.a2 as *mut u64, tls_base()); } } return 0; } // ARCH_GET_FS
            if f.a1 == 0x1001 { return (-22i64) as u64; } // ARCH_SET_GS: the kernel's per-CPU GS (percpu.rs) — it is forbidden to user code
            if f.a1 >= 0x3001 { return (-22i64) as u64; } // ARCH_CET_* (glibc 2.35's ld.so): there is no CET -> EINVAL
            0
        }
        218 => { crate::sched::set_ctid(crate::sched::current_id(), f.a1); (crate::sched::current_id() + 1) as u64 } // set_tid_address
        12 => { // brk(addr)
            let cur = crate::sched::current_id();
            let pbase = crate::sched::heap_base(cur);
            if pbase != 0 { // the process's private heap (a per-process brk)
                let top = crate::sched::heap_top(cur);
                if f.a1 == 0 { return crate::sched::heap_brk(cur); }
                if f.a1 >= pbase && f.a1 <= top { crate::sched::set_heap_brk(cur, f.a1); f.a1 }
                else { crate::sched::heap_brk(cur) }
            } else { // the common heap (the single/threaded path — without changes)
                let base = HEAP_BASE.load(Ordering::SeqCst);
                let top = base + HEAP_FRAMES * 4096;
                if f.a1 == 0 { return HEAP_BRK.load(Ordering::SeqCst); }
                if f.a1 >= base && f.a1 <= top { HEAP_BRK.store(f.a1, Ordering::SeqCst); if f.a1 > HEAP_HIGH.load(Ordering::SeqCst) { HEAP_HIGH.store(f.a1,
                    Ordering::SeqCst); } f.a1 }
                else { HEAP_BRK.load(Ordering::SeqCst) }
            }
        }
        9 => { // mmap(addr,len,prot,flags,fd,off) — anonymously from the allocator, we map it present+the rights
            if f.a4 & 0x20 == 0 && shm_is(f.a5 as i64) { // a MAP_SHARED of a /dev/shm object -> the region's base (+the offset)
                // The region is allocated and PUBLISHED into the row: a neighbour mapping the
                // same object must either wait for the address or see the finished one.
                let b = { let _g = shm_lock(); shm_region(shm_obj(f.a5 as i64), f.a2 + f.a6) };
                return if b == 0 { (-12i64) as u64 } else { b + f.a6 };
            }
            if f.a4 & 0x40000 != 0 { return (-12i64) as u64; } // MAP_HUGETLB -> ENOMEM (PG's huge_pages=try will fall back)
            let file_backed = f.a5 as i64 >= 0 && f.a4 & 0x20 == 0;
            let pages = f.a2.div_ceil(4096);
            let in_proc = crate::sched::task_cr3(crate::sched::current_id()) != 0;
            let mut shared = f.a4 & 0x01 != 0; // MAP_SHARED
            // MAP_FIXED, or MAP_FIXED_NOREPLACE alone (0x100000) — Linux's "exactly here or
            // EEXIST" needs no MAP_FIXED beside it. Read as MAP_FIXED only, a NOREPLACE probe
            // fell to the bump region and came back at another address: HotSpot gave the
            // address back and tried the next — 64 candidates on aarch64, where the compressed
            // class space must sit at an encodable base and has no "anywhere" fallback, and
            // Temurin 21 died with "Could not allocate compressed class space" (x86's heap has
            // that fallback, so the probes there failed silently).
            let fixed = f.a4 & (0x10 | 0x10_0000) != 0;
            // A file MAP_SHARED FOR READING ONLY that did not fit into the block cache
            // is given as a private copy. The reason: a shared mapping with us is the pages
            // of the ext2 cache (8 MB), while the JVM maps `lib/modules` (136 MB) that way and got
            // an ENODEV. For a mapping without PROT_WRITE a copy is indistinguishable from a shared map:
            // one cannot write into the file through it, while we have no other writer (one
            // workload). With PROT_WRITE the substitution would be a silent loss of the writes —
            // there it is still an honest refusal.
            if file_backed && shared && f.a3 & 2 == 0 && crate::ext2::active()
                && is_fs(f.a5 as i64) && !crate::ext2::mmap_fits(f.a2 as usize) { shared = false; }
            // §8.5: a file MAP_SHARED — we map THE PAGES OF ext2's BLOCK CACHE
            // (the same bytes that read/write see; the write-back is by the D bit in
            // msync/munmap/fsync).
            // §2.2/§3.3: the pages are mapped ONE BY ONE, so a contiguous window
            // of slots is needed neither in the process's AS nor in the kernel's identity AS — a file scattered
            // over the disk is mapped in the same way as a contiguous one.
            if file_backed && shared {
                if !(is_fs(f.a5 as i64) && crate::ext2::active()) { return (-19i64) as u64; }
                let prepared = crate::ext2::mmap_prepare(f.a5 as i64, f.a6, f.a2 as usize);
                if prepared.is_none() {
                    // the cache did not give the pages: for reading only it is a private copy (see above), for writing it is a refusal
                    if f.a3 & 2 != 0 { return (-19i64) as u64; }
                    shared = false;
                }
                if let Some(pages) = prepared {
                let vaddr = if in_proc {
                    let v = if fixed { f.a1 & !0xFFF } else { crate::proc::mmap_alloc(f.a2) };
                    let cr3 = crate::paging::current_cr3() & !0xFFF;
                    for p in 0..pages {
                        let phys = crate::ext2::mmap_page(f.a5 as i64, f.a6, p);
                        if phys == 0 { return (-19i64) as u64; }
                        crate::paging::map_leaf(cr3, v + (p * 4096) as u64, phys, f.a3 & 2 != 0, true, false);
                    }
                    v
                } else {
                    // §3.3: the kernel's identity AS. Previously a contiguous window of the cache's
                    // slots was required here (vaddr == phys) and at a discontinuity an
                    // ENODEV was returned. Now we map the cache's pages ONE BY ONE into a separate
                    // window outside the identity — the cache's layout no longer matters.
                    let cr3 = crate::paging::current_cr3() & !0xFFF;
                    let v = crate::paging::fmap_alloc(f.a2);
                    let mut mapped = 0usize;
                    while mapped < pages {
                        let phys = crate::ext2::mmap_page(f.a5 as i64, f.a6, mapped);
                        if phys == 0 { break; }
                        crate::paging::map_leaf(cr3, v + (mapped * 4096) as u64, phys, f.a3 & 2 != 0, true, false);
                        mapped += 1;
                    }
                    crate::paging::flush_tlb_now();
                    if mapped != pages { crate::ext2::mmap_unprepare(f.a5 as i64, f.a6, pages); return (-19i64) as u64; }
                    v
                };
                crate::ext2::mmap_register(vaddr, f.a6, pages, f.a5 as i64);
                return vaddr;
                }
            }
            if file_backed && !(is_fs(f.a5 as i64) && crate::ext2::active()) { return (-19i64) as u64; }
            if in_proc && (fixed || file_backed || !shared) {
                // Process mode: ALL the private mappings go by a virtual address (not identity):
                // MAP_FIXED (ld.so puts a .so's segments over a reservation), a file MAP_PRIVATE (a copy of the file
                // into fresh frames) and an anonymous MAP_PRIVATE (the bump region 0x6000_0000_0000). Otherwise after a fork
                // a COW copy would land at an identity vaddr != phys, while the kernel writes into frames at their identity address
                // (the zeroing at an allocation, a COW copy, the kernel stacks) — it would get into a foreign page.
                // A MAP_SHARED stays identity (shared through the kernel's 2M leaves).
                if pages == 0 { return (-22i64) as u64; }
                let vaddr = if fixed { f.a1 & !0xFFF } else { crate::proc::mmap_alloc(f.a2) };
                if vaddr < 0x1_0000_0000 && fixed { return (-22i64) as u64; } // we do not recut the identity area
                // ...nor its gigabytes above 4 GB (the high area, the NUMA areas, the 1G MMIO
                // windows): the kernel reaches those frames at their identity address in THIS AS too.
                if fixed && crate::paging::ident_leaf_overlap(vaddr, pages) { return (-22i64) as u64; }
                // MAP_FIXED_NOREPLACE (0x100000): "put it exactly here, but overwrite NOTHING".
                // The JVM probes the addresses for its heap exactly that way (the kernel 5.15 in uname -> the flag is on)
                // and moves to the next candidate by an EEXIST. While the flag was ignored,
                // an unsuccessful probe SILENTLY overwrote what was already mapped — a corruption of the heap/the class archive.
                if fixed && f.a4 & 0x10_0000 != 0 && crate::paging::any_mapped(vaddr, pages) { return (-17i64) as u64; }
                if file_backed {
                    // the file's content is needed NOW — we allocate the frames at once
                    if !crate::paging::map_fresh_pages(vaddr, pages) { return (-12i64) as u64; }
                    let n = crate::ext2::pread(f.a5 as i64, vaddr as *mut u8, f.a2 as usize, f.a6);
                    if n < 0 { return n as u64; }
                    crate::paging::protect(vaddr, pages * 4096, f.a3 & 2 != 0, f.a3 & 4 != 0); // the final rights (PRIV is preserved)
                } else {
                    // anonymous memory is A RESERVATION: a frame will appear at the first access
                    // (demand-zero, paging::lazy_fault). That way the reservation of the JVM's heap/code cache
                    // and of jemalloc's arenas costs addresses rather than physical memory.
                    if !crate::paging::map_lazy_pages(vaddr, pages, f.a3 & 2 != 0, f.a3 & 4 != 0, f.a3 == 0) { return (-12i64) as u64; }
                }
                return vaddr;
            }
            let got = crate::mem::alloc_contig(pages); // identity: a MAP_SHARED in a process or any mmap in identity mode
            match got {
                Some(b) => {
                    // An anonymous mmap MUST be zeroed (Linux's semantics; musl's mallocng
                    // relies on zeroes in fresh pages) — the frames from the free list carry garbage.
                    // THE ZEROING GOES IN PORTIONS, WITH THE SERVICING OF THE TLB MAIL. The syscalls go with
                    // IF=0; an 8.75 ms memset (PostgreSQL's segment) is 8.75 ms during
                    // which THIS core does not acknowledge foreign flushes, and all the neighbours'
                    // munmaps stand on it for up to the 10 ms timeout.
                    zero_serviced(b, pages * 4096);
                    // present+US, writable by PROT_WRITE, NX if !PROT_EXEC (W^X). The mapping is
                    // explicit -> munmap can remove it (otherwise a UAF on the identity 2M).
                    crate::paging::protect(b, pages * 4096, f.a3 & 2 != 0, f.a3 & 4 != 0);
                    crate::mem::mreg_add(b, pages); // the accounting: supervise will free it at a restart of the instance
                    b
                }
                None => (-12i64) as u64, // ENOMEM
            }
        }
        25 => { // mremap(old, old_size, new_size, flags, new_addr)
            // IT RETURNED ENOMEM, AND THAT COST THE SCALING. Without `mremap`
            // `realloc` in libc degenerates into "allocate — copy —
            // free", while every freeing is a `munmap`, that is, a TLB flush
            // on ALL the cores. With ClickHouse on a read of a table that gave 1906
            // calls of `munmap` over 15.2 million pages and 4090 TLB flushes with
            // four threads against 134/674 thou./488 with one — and it was exactly there that
            // the speedup by cores was lost (2.4x against Linux's 3.4x), while
            // a pure multi-threaded computation scales with us on a par.
            let (old_a, old_sz, new_sz, flags) = (f.a1 & !0xFFF, f.a2, f.a3, f.a4);
            const MAYMOVE: u64 = 1;
            if old_a == 0 || old_sz == 0 || new_sz == 0 { return (-22i64) as u64; }
            if crate::sched::task_cr3(crate::sched::current_id()) == 0 { return (-12i64) as u64; }
            let op = old_sz.div_ceil(4096);
            let np = new_sz.div_ceil(4096);
            MREMAP_CALLS.fetch_add(1, Ordering::Relaxed);
            if np == op { return old_a; }
            if np < op { // a truncation: remove the tail, the address does not change
                crate::paging::unmap_proc(old_a + np * 4096, op - np);
                MREMAP_INPLACE.fetch_add(1, Ordering::Relaxed);
                return old_a;
            }
            let (w, x) = crate::paging::range_rights(old_a, op);
            // Grow it IN PLACE if it is empty right after the mapping.
            if crate::paging::range_free(old_a + op * 4096, np - op) {
                if crate::paging::map_lazy_pages(old_a + op * 4096, np - op, w, x, false) {
                    MREMAP_INPLACE.fetch_add(1, Ordering::Relaxed);
                    return old_a;
                }
            }
            if flags & MAYMOVE == 0 { MREMAP_FAIL.fetch_add(1, Ordering::Relaxed); return (-12i64) as u64; }
            // A move: the new address comes from the same bump allocator, the tables' entries
            // ARE MOVED (the frames are the same, there is no copy), the tail is grown lazily.
            let dst = crate::proc::mmap_alloc(np * 4096);
            if dst == 0 { MREMAP_FAIL.fetch_add(1, Ordering::Relaxed); return (-12i64) as u64; }
            if !crate::paging::move_mapping(old_a, dst, op) { MREMAP_FAIL.fetch_add(1, Ordering::Relaxed); return (-12i64) as u64; }
            crate::paging::map_lazy_pages(dst + op * 4096, np - op, w, x, false);
            MREMAP_MOVED.fetch_add(1, Ordering::Relaxed);
            dst
        }

        11 => { // munmap(addr, len) — remove the mapping (a guard: non-present) + return it into the free list
            if in_ivshmem(f.a1) { return 0; } // the host's shared memory: we do not touch it
            // §8.5: a file MAP_SHARED — flush the changes and release the cache's pins
            // (the pages belong to the ext2 cache, they are not returned into the free list)
            if crate::ext2::munmap(f.a1 & !0xFFF, f.a2 as usize) { return 0; }
            if f.a1 != 0 && f.a2 != 0 && crate::sched::task_cr3(crate::sched::current_id()) != 0 {
                // process mode: the pages may be COW-shared with the parent/the children — by the refcount
                crate::paging::unmap_proc(f.a1 & !0xFFF, f.a2.div_ceil(4096));
                return 0;
            }
            if f.a1 != 0 && f.a2 != 0 {
                let pages = f.a2.div_ceil(4096);
                // Remove ring 3's access, PRESERVING the kernel's identity: an access from ring 3
                // is still a #PF (there is no UAF), while a frame that went into the free list
                // stays addressable by the kernel — both in this AS and in the ones copied from it.
                crate::paging::unmap_ident(f.a1 & !0xFFF, pages);
                crate::mem::free_contig(f.a1, pages);
                crate::mem::mreg_del(f.a1 & !0xFFF, pages); // the region was given away by itself — remove it from the restart accounting
            }
            0
        }
        16 => { // ioctl(fd, req, arg)
            let fd = f.a1 as i64;
            if crate::netsys::is_sock(fd) && f.a2 == 0x5421 { // FIONBIO
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                let on = unsafe { core::ptr::read_unaligned(f.a3 as *const i32) } != 0;
                crate::netsys::fcntl(fd, 4, if on { 0x800 } else { 0 }); return 0;
            }
            (-25i64) as u64 // ENOTTY (stdout is not a tty -> full buffering)
        }
        2 => vfs_open(f.a1, f.a2, f.a3) as u64,   // open(path, flags, mode)
        257 => vfs_open_at(f.a1 as i64, f.a2, f.a3, f.a4) as u64, // openat(dirfd, path, flags, mode)
        // SAFETY: S2 — the pseudo-fd table (`NULL_*`): a row belongs to the descriptor's owner from
        // the CAS in `null_alloc_kind` until `null_free`.
        8 => { if is_fs(f.a1 as i64) { if crate::ext2::active() { crate::ext2::lseek(f.a1 as i64, f.a2 as i64, f.a3 as i64) as u64 } else {
            crate::fs::lseek(f.a1 as i64, f.a2 as i64, f.a3 as i64) as u64 } } else if null_is(f.a1 as i64) { let i = (f.a1 as i64 - NULL_BASE) as usize;
            if unsafe { NULL_KIND[i] } == 7 { unsafe { NULL_CTR[i] = match f.a3 { 0 => f.a2, 1 => NULL_CTR[i].wrapping_add(f.a2), _ => NULL_CTR[i] };
            NULL_CTR[i] } } else { 0 } } else { (-29i64) as u64 } } // lseek (ESPIPE)
        5 => fstat_any(f.a1 as i64, f.a2 as *mut u8) as u64, // fstat
        74 | 75 => { if crate::ext2::active() { crate::ext2::fsync(f.a1 as i64) as u64 } else { 0 } } // fsync / fdatasync -> write-back + virtio flush
        162 | 306 => crate::ext2::sync_all() as u64, // sync / syncfs — an EIO if the device refused
        26 => crate::ext2::msync(f.a1, f.a2 as usize) as u64, // msync(addr,len,flags) — §8.5
        57 => { // fork — A PROCESS with a full copy of the address space
            if !THREADED.load(Ordering::SeqCst) { return (-38i64) as u64; } // only under the scheduler (ENOSYS)
            let child_cr3 = crate::paging::fork_cow_space(crate::paging::current_cr3()); // a COW clone
            let parent_user_rsp = raw.usp; // the user's rsp at the entry
            let kstack = crate::sched::alloc_kstack_in(child_cr3).unwrap(); // the guard in the CHILD's space
            // SAFETY: S3 — `sched::spawn_process_regs`'s own contract; here it holds: the current
            // task, on its kernel stack, with its CR3 active.
            let idx = unsafe { crate::sched::spawn_process_regs(f.pc, parent_user_rsp, f.flags, kstack, child_cr3, fork_regs(f)) };
            // The task table's ceiling is AN HONEST REFUSAL (as with a clone thread below).
            // Previously fork went past the check and wrote past the boundary of the kernel's arrays.
            if idx == usize::MAX {
                crate::sched::release_kstack(kstack);
                crate::sched::queue_dead_space(child_cr3); // the COW clone will be cleaned up by the housekeeping
                return (-11i64) as u64; // EAGAIN
            }
            crate::sched::set_kstack_pages(idx, crate::sched::KSTACK_PAGES); // the kernel stack will return into the free list at a reuse_slot/reset
            // Carry the process's private heap over to the child (the COW region is copied by
            // fork_cow_space; we inherit the parent's base/top/brk, otherwise the child would go to the common heap).
            let p = crate::sched::current_id();
            let pbase = crate::sched::heap_base(p);
            if pbase != 0 {
                crate::sched::set_heap(idx, pbase, crate::sched::heap_top(p));
                crate::sched::set_heap_brk(idx, crate::sched::heap_brk(p));
            }
            crate::proc::fork_into(crate::sched::proc_of(p), idx); crate::sched::set_ppid(idx, crate::sched::proc_of(p) as i32);
            // SAFETY: S4 — the FS base of this core IS the forking thread's TLS pointer; the
            // child inherits it.
            crate::sched::set_fsbase(idx, unsafe { tls_base() }); // the parent's TLS
            crate::sched::set_fpu(idx, fpu_area(raw)); // the parent's FPU/SSE state
            crate::fr::record(crate::fr::USER, f.pc); // the child starts with rax=0 at the return point
            crate::sched::publish(idx); // the task is assembled entirely — now it may be scheduled
            (idx + 1) as u64 // to the parent -> the child's pid
        }
        56 => { // clone(flags, child_stack, ptid, ctid, tls)
            let flags = f.a1;
            if flags & 0x0000_0100 == 0 { // there is no CLONE_VM -> this is a fork (musl's fork = a clone(SIGCHLD))
                if !THREADED.load(Ordering::SeqCst) { return (-38i64) as u64; }
                let child_cr3 = crate::paging::fork_cow_space(crate::paging::current_cr3());
                let cstack = if f.a2 != 0 { f.a2 } else { raw.usp }; // child_stack or the parent's rsp
                let kstack = crate::sched::alloc_kstack_in(child_cr3).unwrap(); // the guard in the CHILD's space
                // SAFETY: S3 — `sched::spawn_process_regs`'s own contract; here it holds: the
                // current task, on its kernel stack, with its CR3 active.
                let idx = unsafe { crate::sched::spawn_process_regs(f.pc, cstack, f.flags, kstack, child_cr3, fork_regs(f)) };
                if idx == usize::MAX {   // the task table's ceiling — an EAGAIN, see fork(57)
                    crate::sched::release_kstack(kstack);
                    crate::sched::queue_dead_space(child_cr3);
                    return (-11i64) as u64;
                }
                crate::sched::set_kstack_pages(idx, crate::sched::KSTACK_PAGES); // as with fork(57): the stack will return at a reuse_slot/reset
                let p = crate::sched::current_id();
                if crate::sched::heap_base(p) != 0 {
                    crate::sched::set_heap(idx, crate::sched::heap_base(p), crate::sched::heap_top(p));
                    crate::sched::set_heap_brk(idx, crate::sched::heap_brk(p));
                }
                crate::proc::fork_into(crate::sched::proc_of(p), idx); crate::sched::set_ppid(idx, crate::sched::proc_of(p) as i32);
                // SAFETY: S4 — as above: the forking thread's TLS pointer is this core's FS base.
                crate::sched::set_fsbase(idx, unsafe { tls_base() }); // the parent's TLS
                crate::sched::set_fpu(idx, fpu_area(raw));
                let tid = (idx + 1) as i32;
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                if flags & 0x0100_0000 != 0 && f.a4 != 0 { unsafe { core::ptr::write(f.a4 as *mut i32, 0); } } // CHILD_SETTID -> 0 in the child (its AS)
                crate::fr::record(crate::fr::USER, f.pc);
                crate::sched::publish(idx);
                return tid as u64;
            }
            // Without the scheduler (a single-threaded run_disk) a thread is created but will not get a quantum —
            // services like redis (its bio threads) are content with a successful clone. We do not refuse.
            let cstack = f.a2; let ptid = f.a3; let ctid = f.a4; let tls = f.a5; let func = f.a6;
            let kstack = match crate::sched::alloc_kstack() { Some(k) => k, None => return (-11i64) as u64 };
            // SAFETY: S4 — `clone` without CLONE_SETTLS gives the child the parent's TLS, which
            // is this core's FS base right now.
            let fsbase = if flags & 0x0008_0000 != 0 { tls } else { unsafe { tls_base() } };
            let gp = crate::arch::signal::clone_regs(f, func); // the child's file: the return value 0, the function where its libc looks
            // SAFETY: S3 — `sched::spawn_user_ctx`'s own contract; here it holds: the current task,
            // on its kernel stack, with its CR3 active.
            let idx = unsafe { crate::sched::spawn_user_ctx(f.pc, cstack, f.flags, fsbase, kstack, gp) };
            // WHAT MUST BE RETURNED IS THE WHOLE STACK AND FROM ITS BASE. `alloc_kstack` returns
            // THE TOP (`base + KSTACK_PAGES*4096`), while here FOUR pages were freed
            // at the address "the top minus 16 KB" — that is, a foreign
            // piece in the middle of a live stack, while the stack itself (16 pages) leaked.
            if idx == usize::MAX {
                crate::sched::warn_no_tasks();
                crate::sched::release_kstack(kstack);
                return (-11i64) as u64; // EAGAIN: the task limit
            }
            crate::proc::thread_inherit_sigmask(crate::sched::current_id(), idx); // the new thread inherits the creator's signal mask (Linux)
            crate::sched::set_kstack_pages(idx, crate::sched::KSTACK_PAGES);
            // A thread SHARES the parent's address space -> it inherits its CR3
            // (otherwise in process mode the thread would run under a foreign/zero AS -> a crash).
            crate::sched::set_task_cr3(idx, crate::sched::task_cr3(crate::sched::current_id()));
            let tid = (idx + 1) as i32;
            // SAFETY: S3 — `sched::set_ctid`'s own contract; here it holds: the current task, on its
            // kernel stack, with its CR3 active.
            unsafe {
                if flags & 0x0010_0000 != 0 && ptid != 0 { core::ptr::write(ptid as *mut i32, tid); } // PARENT_SETTID
                if flags & 0x0100_0000 != 0 && ctid != 0 { core::ptr::write(ctid as *mut i32, tid); } // CHILD_SETTID
            }
            if flags & 0x0020_0000 != 0 { crate::sched::set_ctid(idx, ctid); } // CHILD_CLEARTID -> clear it at the exit
            // The spreading over the cores: the thread has never executed, its context is
            // entirely in RSP[idx] — a "migration" here costs one write into CPU_OF
            // and a ring of the target core's doorbell. The thread has no name yet; if it
            // names itself, a rule will move it by address (the prctl above).
            crate::affinity::place_new_thread(idx, crate::sched::current_id());
            // Only HERE does the thread become visible to the schedulers: before that moment it
            // might have had no frame, CR3 or core set — while a tick on a neighbouring core already counted it
            // as ready and drove off by a garbage frame (a #GP on the iretq in the scheduler's stub).
            crate::sched::publish(idx);
            tid as u64
        }
        202 => { // futex — PLAN-SYNC: a WAIT sleeps in the kernel with an mwait on the user word (a write wakes it
                 // without an IPI), a WAKE is a real one: WOKEN + a directed switch / an IPI reschedule / a kick of an AP.
            let uaddr = f.a1 as *const u32; let op = f.a2 & 0x7f; let val = f.a3 as u32;
            if (op as usize) < 16 { FUTEX_OPS[op as usize].fetch_add(1, Ordering::Relaxed); }
            let leader = crate::sched::proc_of(crate::sched::current_id());
            if op == 0 || op == 9 { // FUTEX_WAIT / FUTEX_WAIT_BITSET
                let deadline = match read_timespec(f.a4) {
                    Some((s, ns)) => {
                        let t = (s.max(0) as u64) * 1_000_000_000 + ns.max(0) as u64;
                        // BITSET: absolute (REALTIME with the flag 0x100)
                        if op == 9 { Some(if f.a2 & 0x100 != 0 { t.saturating_sub(crate::tsc::wall_ns()) + crate::tsc::mono_ns() } else { t }) }
                        else { Some(crate::tsc::mono_ns() + t) } // WAIT: relative
                    }
                    None => None,
                };
                return crate::sync::wait_on(uaddr, val, deadline) as u64;
            }
            if op == 1 || op == 10 { // FUTEX_WAKE
                return crate::sync::wake_addr(f.a1, f.a3 as usize, leader, true) as u64;
            }
            if op == 5 { // FUTEX_WAKE_OP(uaddr, val, val2, uaddr2, val3): an op over *uaddr2 + a wake of both words
                let enc = f.a6 as u32; let oparg = ((enc >> 12) & 0xfff) as i32; let oparg = if (enc >> 28) & 8 != 0 { 1 << oparg } else { oparg };
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                let u2 = f.a5 as *mut i32;
                if u2 as usize & 3 != 0 { return (-22i64) as u64; } // a futex word is aligned (EINVAL, as Linux)
                // The op is ATOMIC on the user word, as Linux's: another thread updating it
                // with its own atomics on another core must not lose its update (a plain
                // read-then-write did) — and the wake below must see the new value published.
                // SAFETY: S1 — validated by `uaccess::gate`, aligned (checked above).
                let w = unsafe { core::sync::atomic::AtomicI32::from_ptr(u2) };
                let op4 = (enc >> 28) & 7;
                let old = w.try_update(Ordering::SeqCst, Ordering::SeqCst, |old| Some(match op4 { 0 => oparg, 1 => old.wrapping_add(oparg),
                    2 => old | oparg, 3 => old & !oparg, 4 => old ^ oparg, _ => old })).unwrap_or_else(|v| v);
                let mut n = crate::sync::wake_addr(f.a1, f.a3 as usize, leader, false);
                let cmparg = (enc & 0xfff) as i32;
                let cond = match (enc >> 24) & 0xf { 0 => old == cmparg, 1 => old != cmparg, 2 => old < cmparg, 3 => old <= cmparg, 4 => old > cmparg,
                    _ => old >= cmparg };
                if cond { n += crate::sync::wake_addr(f.a5, f.a4 as usize, leader, false); }
                return n as u64;
            }
            if op == 3 || op == 4 { // FUTEX_REQUEUE / CMP_REQUEUE: we wake everybody (a spurious wake
                // is allowed by the futex ABI — the waiters recheck the predicate and lie down on the mutex themselves)
                //
                // A real requeue here WAS TRIED and removed. Our waiter sleeps
                // not "in a queue by an address" but with a monitor (an mwait) on A PARTICULAR word,
                // and its local copy of the address does not change from a write into the registry. A waker
                // at the new address computes `store_woke` — "a write into the word has already woken
                // the monitor" — by the NEW word, while the monitor stands on the old one: a push is not
                // sent, the waiter does not wake up. It is caught at once (the gate tsync stalls at
                // the very first condvar broadcast). In order to requeue for real, the waiter
                // must be woken and rearmed on the new word — that is not a saving but an exchange of
                // one crush for another, and without a measurement in favour of requeueing there is no point.
                // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
                // dispatch.
                if op == 4 && unsafe { core::ptr::read_volatile(uaddr) } != f.a6 as u32 { return (-11i64) as u64; } // EAGAIN
                let n = f.a3.saturating_add(f.a4).min(u32::MAX as u64) as usize; // nwake + nrequeue
                return crate::sync::wake_addr(f.a1, n, leader, true) as u64;
            }
            crate::println!("[futex] the op {:#x} is not supported (t{})", f.a2, crate::sched::current_id());
            0
        }
        228 => { // clock_gettime(clockid, timespec*)
            let ns = match f.a1 { 0 | 5 | 11 => crate::tsc::wall_ns(), _ => crate::tsc::mono_ns() }; // REALTIME(+COARSE,TAI) / MONOTONIC & co
            write_timespec(f.a2, ns); 0
        }
        96 => { // gettimeofday(timeval*, tz)
            // SAFETY: S1 — the pointer argument was validated by `uaccess::gate` before this
            // dispatch.
            if f.a1 != 0 { let ns = crate::tsc::wall_ns(); unsafe { core::ptr::write_unaligned(f.a1 as *mut i64, (ns / 1_000_000_000) as i64);
                core::ptr::write_unaligned((f.a1 + 8) as *mut i64, ((ns % 1_000_000_000) / 1000) as i64); } }
            0
        }
        201 => { let s = crate::tsc::wall_ns() / 1_000_000_000; if f.a1 != 0 { unsafe { core::ptr::write_unaligned(f.a1 as *mut i64, s as i64); } } s } // time
        229 => { write_timespec(f.a2, 1); 0 } // clock_getres -> 1 ns
        35 | 230 => { // nanosleep(req, rem) / clock_nanosleep(clockid, flags, req, rem)
            let (req, abs) = if f.nr == 35 { (f.a1, false) } else { (f.a3, f.a2 & 1 != 0) };
            if let Some((s, ns)) = read_timespec(req) {
                let t = (s.max(0) as u64) * 1_000_000_000 + ns.max(0) as u64;
                let until = if abs { if f.a1 == 0 { t.saturating_sub(crate::tsc::wall_ns()) + crate::tsc::mono_ns() } else { t } }
                            else { crate::tsc::mono_ns() + t };
                return sleep_until_mono(until) as u64;
            }
            0
        }
        // SAFETY: S4 — `pause`: as `rt_sigsuspend` above, the interrupt window is one
        // instruction wide, so a signal cannot slip between the check and the `hlt`.
        34 => { loop { if crate::proc::sig_pending() { return (-4i64) as u64; } unsafe { crate::arch::cpu::irq_enable_halt_disable(); }
            } } // pause — until a signal
        1000 => { // talaria_fastmap -> the base of the shared region (ring 3's access is explicit, in this AS)
            let r = crate::fastpath::region();
            if r != 0 { crate::paging::protect(r, (crate::fastpath::REGION_SZ as u64 + 4095) & !4095, true, false); }
            r
        }
        1001 => 0, // a nop syscall (a micro-benchmark of the price of ring3->ring0->ring3)
        1002 => crate::tsc::calibrate_cached(), // give the TSC Hz to the application
        _ => unknown(f.nr),
    }
}


// ── the signals: the delivery at the exit from a syscall and rt_sigreturn ─────────
// The LAYOUT of the frame (siginfo, ucontext, gregs, the FPU area) and the register names are
// the architecture's, in `arch::signal`; here is the policy — the same on x86-64 and arm64.
use crate::arch::signal::{fork_regs, fpu_area, RET_SLOT, SIGFRAME, SIGFRAME_ALIGN, SIGINFO_SIZE, UC_SIGMASK, UC_STACK};
/// The signals whose default action is "terminate AND WRITE A DUMP"
/// (Linux: SIGQUIT/ILL/ABRT/FPE/SEGV/BUS/SYS/TRAP/XCPU/XFSZ). For the rest
/// (for example SIGTERM, SIGKILL, SIGINT) no dump is written — the process was killed from the outside,
/// and its memory is not evidence.
fn fatal_dumps(sig: u64) -> bool {
    matches!(sig, 3 | 4 | 5 | 6 | 7 | 8 | 11 | 24 | 25 | 31)
}

/// Deliver one pending signal to the current task: SIG_DFL — ignore/terminate, SIG_IGN —
/// skip, otherwise build a frame (the restorer, the siginfo, the ucontext) on the user stack and
/// redirect rip into the handler (rdi=sig, rsi=&siginfo, rdx=&uc). It returns rax.
fn deliver_signals(f: &mut Frame, ret: u64) -> u64 {
    loop {
        let sig = match crate::proc::take_pending() { Some(s) => s, None => return ret };
        let (h, flags, restorer, mask) = crate::proc::handler_of(sig);
        if strace_on() {
            crate::println!("[signal] t{} accepted {} (the handler {:#x})", crate::sched::current_id(), sig, h);
        }
        if h == 1 { continue; } // SIG_IGN
        if h == 0 { // SIG_DFL
            match sig { 17 | 23 | 28 | 18 | 20 | 21 | 22 => continue, // CHLD/URG/WINCH/CONT/TSTP/TTIN/TTOU -> ignore
                _ => {
                    let p = crate::sched::proc_of(crate::sched::current_id());
                    crate::println!("[signal] the process pid={} was finished by the signal {}", p + 1, sig);
                    // A CORE DUMP HERE TOO, rather than only on the exception's path (idt.rs).
                    // A workload that has set its own handler of a fatal signal
                    // dies NOT through a #PF: the handler prints its own, removes
                    // itself and re-raises the signal — the process reaches here, and
                    // there was no dump exactly in the case for whose sake it is
                    // needed (that is how the analysis of MariaDB's crash stayed silent). The frame here is
                    // the point of the re-raise rather than of the original error; the memory,
                    // the list of threads and the kernel's stack it carries in full, while the stack of the
                    // workload itself is by that moment already in the log from its own handler.
                    // As at a crash: the sisters stop BEFORE the dump is written, and
                    // the cleanup goes after they stop (see `idt`, `exit_group`).
                    if fatal_dumps(sig) { crate::coredump::snapshot_threads(); }
                    crate::sched::exit_all(sig as i32);
                    crate::sched::drain_proc(1000);
                    if fatal_dumps(sig) {
                        let ef = crate::arch::signal::excframe_of_syscall(f);
                        crate::coredump::dump_process(&ef, sig as u32);
                    }
                    crate::proc::exit_cleanup(p);
                    crate::sched::queue_dead_space(crate::sched::task_cr3(p));
                    crate::sched::exit_current_code(sig as i32);
                }
            }
        }
        // the frame goes on the USER stack below the red zone (the kernel executes the syscall on its own kernel stack; a write into
        // a COW page of the stack will give a ring-0 #PF -> a cow_fault by CR0.WP)
        let cur = crate::sched::current_id();
        let usp = if flags & 0x0800_0000 != 0 && altstack_top(cur) != 0 { altstack_top(cur) } else { f.usp };
        let base = (usp - 128 - SIGFRAME) & !(SIGFRAME_ALIGN - 1);
        let sp = base - RET_SLOT;
        if !crate::paging::user_writable(sp, SIGFRAME + RET_SLOT) {
            crate::println!("[signal] there is no room for the frame of the signal {} at t{} (rsp={:#x}) — it is skipped", sig, cur, usp);
            continue;
        }
        let si = base; let uc = base + SIGINFO_SIZE;
        // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate`
        // has already validated.
        unsafe {
            core::ptr::write_bytes(base as *mut u8, 0, SIGFRAME as usize);
            core::ptr::write_unaligned(si as *mut i32, sig as i32);            // si_signo
            core::ptr::write_unaligned((si + 8) as *mut i32, 0);               // si_code SI_USER
            let t = crate::sched::current_id();
            // SA_RESTART: an interrupted (-EINTR) restartable syscall is repeated after the handler
            // (the return lands on the `syscall` instruction itself, the number back in the result register)
            // The list is in the INTERNAL numbers; the frame holds the process's own (arm64's
            // `wait4` is 260, and its 61 is `getdents64` — read raw, a `waitpid` interrupted
            // by SIGCHLD came back as EINTR under SA_RESTART).
            let restart = ret as i64 == -4 && flags & 0x1000_0000 != 0
                && matches!(crate::arch::abi::to_internal(f.nr), Some(0 | 1 | 17 | 18 | 19 | 20 | 43 | 288 | 61 | 44 | 45 | 46 | 47 | 42));
            let old_mask = crate::proc::sigmask_of(t);
            crate::arch::signal::write_syscall_context(uc, f, ret, usp, restart, old_mask);
            core::ptr::write_unaligned((uc + UC_SIGMASK) as *mut u64, old_mask); // uc_sigmask
            let mut nm = old_mask | mask; if flags & 0x4000_0000 == 0 { nm |= 1u64 << (sig - 1); } // !SA_NODEFER
            crate::proc::set_sigmask(t, nm);
            // The FPU/SSE/AVX state of the interrupted code -> into the frame (uc_mcontext.fpregs)
            crate::arch::signal::write_fpu(base, uc, fpu_area(f));
        }
        // SAFETY: S1 — `sp` lies in the frame `user_writable` admitted above.
        unsafe { crate::arch::signal::enter_handler_syscall(f, sig, si, uc, h, sp, restorer); }
        return ret;
    }
}
// ── sigaltstack: a separate stack for a handler (SA_ONSTACK) ────────────────────
// It is needed exactly where the handler has NOTHING to work on: an overflow of a thread's stack.
// The JVM sets it for every thread and catches a StackOverflowError on it.
static mut ALT_SP: [u64; crate::sched::NTASKS] = [0; crate::sched::NTASKS];
static mut ALT_SIZE: [u64; crate::sched::NTASKS] = [0; crate::sched::NTASKS];
static mut ALT_ON: [bool; crate::sched::NTASKS] = [false; crate::sched::NTASKS];
/// sigaltstack(new, old): ss_sp(0) ss_flags(8) ss_size(16).
fn sigaltstack(new: u64, old: u64) -> u64 {
    let t = crate::sched::current_id();
    // SAFETY: S2 — the alternate signal stack (`ALT_*`): indexed by the task, written only by that
    // task.
    unsafe {
        if old != 0 {
            core::ptr::write_unaligned(old as *mut u64, ALT_SP[t]);
            core::ptr::write_unaligned((old + 8) as *mut i32, if ALT_ON[t] { 1 } else { if ALT_SP[t] == 0 { 2 } else { 0 } }); // SS_ONSTACK / SS_DISABLE
            core::ptr::write_unaligned((old + 16) as *mut u64, ALT_SIZE[t]);
        }
        if new != 0 {
            if ALT_ON[t] { return (-16i64) as u64; } // EPERM: it must not be changed while we work on it
            let sp = core::ptr::read_unaligned(new as *const u64);
            let fl = core::ptr::read_unaligned((new + 8) as *const i32);
            let sz = core::ptr::read_unaligned((new + 16) as *const u64);
            if fl & 2 != 0 { ALT_SP[t] = 0; ALT_SIZE[t] = 0; } // SS_DISABLE
            else { if sz < 2048 { return (-12i64) as u64; } ALT_SP[t] = sp; ALT_SIZE[t] = sz; }
        }
    }
    0
}
/// The top of a thread's alt stack (0 — it is not set/we are already on it).
fn altstack_top(t: usize) -> u64 {
    // SAFETY: S2 — the alternate signal stack (`ALT_*`): indexed by the task, written only by that
    // task.
    unsafe { if ALT_SP[t] == 0 || ALT_ON[t] { 0 } else { ALT_SP[t] + ALT_SIZE[t] } }
}

/// A fatal signal to a process: there was nowhere to deliver it (there is no handler or there is no room
/// for a frame on the thread's stack), and the supervisor does not catch this error. Linux in
/// such a situation kills the process — and we must do the same rather than
/// stop the machine: an error of the workload in ring 3 must not cost the whole OS.
pub fn kill_current_process(sig: u64) -> ! {
    let p = crate::sched::proc_of(crate::sched::current_id());
    crate::println!("[signal] the process pid={} was killed by the signal {} (there was nowhere to deliver it)", p + 1, sig);
    crate::sched::exit_all(sig as i32);
    crate::sched::drain_proc(1000);
    crate::proc::exit_cleanup(p);
    crate::sched::queue_dead_space(crate::sched::task_cr3(p));
    crate::sched::exit_current_code(sig as i32);
}

/// A synchronous signal from a CPU exception (#PF/#GP/#UD/#DE...) — build the same
/// frame as `deliver_signals`, but from THE EXCEPTION's frame and with real
/// si_addr/err/trapno/cr2. true — the frame is built, the exception's frame is redirected
/// into the handler (the caller does an iretq); false — there is no handler, this is
/// a real crash (a crash dump).
/// `talaria.sigtrace=N` — print the first N SYNCHRONOUS signals to the workload.
///
/// A synchronous signal is the kernel's report that the workload did something
/// inadmissible, and usually it is lawful (the JVM catches a SIGSEGV at every safepoint
/// poll). But when it comes WHERE IT IS NOT EXPECTED, the workload dies
/// silently: the handler is its own, and nothing gets outside. Then one needs
/// a look from the other side — what the kernel saw in the page tables at the moment of the fault.
/// How many more signals to describe in detail (`talaria.sigtrace=N`).
pub static SIGTRACE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many more FATAL exceptions to print IN ONE LINE, even when the
/// workload has a handler of its own.
///
/// Why not by a flag. The path "an exception -> the workload has a handler" was
/// MUTE: the application fell, the signal's frame was built, control went into
/// the handler — and not a line was left in the kernel's log. From the outside that looks
/// like "it fell by itself": with MariaDB an evening went into the analysis, while exactly
/// one line with the rip and the address was needed. Four are enough not to flood the log with a loop
/// of crashes.
static FATALTRACE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(4);

fn sigtrace(f: &crate::idt::ExcFrame, sig: u64, cr2: u64, t: usize, delivered: bool) {
    use core::sync::atomic::Ordering::Relaxed;
    let left = SIGTRACE.load(Relaxed);
    if left == 0 { return; }
    SIGTRACE.store(left - 1, Relaxed);
    let nm = crate::sched::comm(t); let nl = crate::sched::comm_len(t);
    let (present, rw, us, _) = crate::paging::query(cr2);
    let (_pf, no_page, write, _ifetch) = crate::arch::signal::fault_kind(f);
    crate::println!("[signal] the signal {} to the thread t{} \"{}\" on the core {}: the address {:#x}, the rip {:#x}, the err {:#x} ({}, {}) -> {}",
        sig, t, core::str::from_utf8(&nm[..nl]).unwrap_or("?"), crate::percpu::cpu_id(),
        cr2, crate::arch::signal::exc_pc(f), crate::arch::signal::fault_err(f),
        if no_page { "there is no page" } else { "a violation of the rights" },
        if write { "a write" } else { "a read" },
        if delivered { "to the handler" } else { "FATALLY" });
    // What the kernel sees in the tables RIGHT NOW. A divergence with err is the main evidence:
    // "there is no page" at the processor and present with us mean a stale TLB.
    crate::println!("[signal]   the page tables: present={} rw={} user={} the kind={}",
        present, rw, us,
        match crate::paging::page_kind(cr2) { 0 => "none", 1 => "PROT_NONE", 2 => "a lazy reservation", _ => "an ordinary one" });
}

pub fn deliver_exception_signal(f: &mut crate::idt::ExcFrame, sig: u64, cr2: u64, fpu: u64) -> bool {
    let t = crate::sched::current_id();
    sigtrace(f, sig, cr2, t, true);
    if fatal_dumps(sig) {
        use core::sync::atomic::Ordering::Relaxed;
        let left = FATALTRACE.load(Relaxed);
        if left > 0 {
            FATALTRACE.store(left - 1, Relaxed);
            // query returns (present, rw, us, EXECUTABLE) — the fourth one is NX-removed.
            let (present, _rw, _us, exec) = crate::paging::query(cr2);
            let (_pf, no_page, write, ifetch) = crate::arch::signal::fault_kind(f);
            crate::println!("[fault] the signal {} to the thread t{}: the rip {:#x}, the address {:#x}, the err {:#x} ({}{}{}) -> to the workload's handler",
                sig, t, crate::arch::signal::exc_pc(f), cr2, crate::arch::signal::fault_err(f),
                if no_page { "there is no page" } else { "a violation of the rights" },
                if ifetch { ", an instruction fetch" } else if write { ", a write" } else { ", a read" },
                if present && !exec && ifetch { ", the page is NOT EXECUTABLE" } else { "" });
            crate::println!("[fault] the core {}, the task was running on c{}", crate::percpu::cpu_id(), crate::sched::cpu_of(t));
            crate::sched::dump_push_log();
        }
    }
    let (h, flags, restorer, mask) = crate::proc::handler_of(sig);
    if h == 0 || h == 1 { return false; }                       // SIG_DFL/SIG_IGN — the former behaviour
    if restorer == 0 { return false; }                          // without a restorer there is no return from the handler
    if crate::proc::sigmask_of(t) & (1u64 << (sig - 1)) != 0 { return false; } // it is blocked — that is fatal (as in Linux)
    let on_alt = flags & 0x0800_0000 != 0 && altstack_top(t) != 0; // SA_ONSTACK
    let user_sp = crate::arch::signal::exc_sp(f);
    let mut usp = if on_alt { altstack_top(t) } else { user_sp };
    let mut base = (usp - 128 - SIGFRAME) & !(SIGFRAME_ALIGN - 1);
    // The frame MUST LAND. If there is no room on the thread's stack (the typical case is
    // a stack overflow: below rsp the guard page begins), we try
    // the alternative stack even if the handler did not ask for SA_ONSTACK: there is nowhere
    // to deliver the signal — the process will die anyway, but the kernel must not fall.
    if !crate::paging::user_writable(base - RET_SLOT, SIGFRAME + RET_SLOT) {
        let alt = altstack_top(t);
        if alt == 0 || !crate::paging::user_writable((alt - 128 - SIGFRAME) & !0xF, SIGFRAME + RET_SLOT) {
            // What exactly is under the stack pointer: the workload's own guard page (which means
            // the thread ran into its own limit) or the end of the mapping — those are different illnesses.
            let nm = crate::sched::comm(t); let nl = crate::sched::comm_len(t);
            crate::println!("[signal] there is no room for the frame of the signal {} at the thread \"{}\" (t{}, rsp={:#x}, the alt stack={:#x}, the page at \
                rsp: {}) — the signal is fatal",
                sig, core::str::from_utf8(&nm[..nl]).unwrap_or("?"), t, user_sp, alt,
                match crate::paging::page_kind(user_sp) { 0 => "none at all", 1 => "PROT_NONE (the workload's guard)", 2 => "a lazy reservation", _ => "an \
                    ordinary one" });
            return false;
        }
        usp = alt;
        base = (usp - 128 - SIGFRAME) & !(SIGFRAME_ALIGN - 1);
    }
    let sp = base - RET_SLOT;
    let si = base; let uc = base + SIGINFO_SIZE;
    // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate` has
    // already validated.
    unsafe {
        core::ptr::write_bytes(base as *mut u8, 0, SIGFRAME as usize);
        core::ptr::write_unaligned(si as *mut i32, sig as i32);              // si_signo
        // si_code: for a #PF it is SEGV_MAPERR (there is no page) / SEGV_ACCERR (there are no rights)
        let (si_code, addr) = crate::arch::signal::exception_siginfo(f, sig, cr2);
        core::ptr::write_unaligned((si + 8) as *mut i32, si_code);
        core::ptr::write_unaligned((si + 16) as *mut u64, addr);             // si_addr
        let old_mask = crate::proc::sigmask_of(t);
        crate::arch::signal::write_exception_context(uc, f, cr2, old_mask);  // gregs + ERR/TRAPNO/CR2
        core::ptr::write_unaligned((uc + UC_SIGMASK) as *mut u64, old_mask); // uc_sigmask
        // uc_stack (ss_sp/ss_flags/ss_size) — the JVM's handler reads it
        core::ptr::write_unaligned((uc + UC_STACK) as *mut u64, ALT_SP[t]);
        core::ptr::write_unaligned((uc + UC_STACK + 8) as *mut i32, if on_alt { 1 } else { 0 });
        core::ptr::write_unaligned((uc + UC_STACK + 16) as *mut u64, ALT_SIZE[t]);
        let mut nm = old_mask | mask;
        if flags & 0x4000_0000 == 0 { nm |= 1u64 << (sig - 1); } // !SA_NODEFER
        crate::proc::set_sigmask(t, nm);
        // The full FPU/AVX state of the interrupted code (uc_mcontext.fpregs)
        crate::arch::signal::write_fpu(base, uc, fpu);
        if on_alt { ALT_ON[t] = true; }
    }
    // SAFETY: S1 — `sp` lies in the frame `user_writable` admitted above.
    unsafe { crate::arch::signal::enter_handler_exception(f, sig, si, uc, h, sp, restorer); }
    true
}

/// rt_sigreturn: the restorer calls syscall(15) with the user sp at the frame's base (x86:
/// after its `ret` popped the return slot; arm64: unchanged). The mask and the
/// alternate-stack flag are ours; the registers and the FPU state go back through
/// `arch::signal::sigreturn` — on x86-64 THROUGH AN IRETQ rather than a sysret (the reasons —
/// rcx/r11 and the JVM's JIT — are told there), on arm64 by rewriting this very frame.
fn sigreturn(f: &mut Frame) -> u64 {
    let base = f.usp;
    let uc = base + SIGINFO_SIZE;
    let t = crate::sched::current_id();
    // SAFETY: S2 — the alternate signal stack (`ALT_*`): indexed by the task, written only by that
    // task.
    unsafe { ALT_ON[t] = false; } // we have left the alt stack
    // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate` has
    // already validated.
    unsafe {
        crate::proc::set_sigmask(t, core::ptr::read_unaligned((uc + UC_SIGMASK) as *const u64));
        crate::arch::signal::sigreturn(f, uc, crate::sched::kstack_top(t))
    }
}
/// A poll with the translation of the fds user->handle in place (and back).
fn poll_translated(fds: *mut u8, nfds: usize, to: i64) -> i64 {
    if fds.is_null() { return if nfds == 0 { crate::netsys::poll_fds(fds, 0, to) } else { -14 }; }
    let mut saved = [0i32; 64];
    let n = nfds.min(64);
    // SAFETY: S1 — the caller hands in an address of the process's memory that `uaccess::gate` has
    // already validated.
    unsafe {
        for k in 0..n { let p = fds.add(k * 8) as *mut i32; saved[k] = core::ptr::read_unaligned(p); if saved[k] >= 0 { core::ptr::write_unaligned(p,
            crate::proc::tr(saved[k] as u64) as i32); } }
        let r = crate::netsys::poll_fds(fds, n, to);
        for k in 0..n { core::ptr::write_unaligned(fds.add(k * 8) as *mut i32, saved[k]); }
        r
    }
}
/// select/pselect: the fd_set by bits; the readiness through the epoll masks.
fn do_select(nfds: usize, rs: u64, ws: u64, es: u64, to_ms: i64) -> i64 {
    let t0 = crate::tsc::mono_ns();
    let deadline = if to_ms >= 0 { Some(t0 + (to_ms as u64) * 1_000_000) } else { None };
    loop {
        if crate::netsys::ready() { crate::netsys::poll(); }
        let mut n = 0i64;
        let mut outr = [0u64; 16]; let mut outw = [0u64; 16];
        for ufd in 0..nfds.min(1024) {
            let w = ufd / 64; let b = 1u64 << (ufd % 64);
            // SAFETY: S1 — the caller hands in an address of the process's memory that
            // `uaccess::gate` has already validated.
            let wr = rs != 0 && unsafe { core::ptr::read_unaligned((rs as *const u64).add(w)) } & b != 0;
            let ww = ws != 0 && unsafe { core::ptr::read_unaligned((ws as *const u64).add(w)) } & b != 0;
            if !wr && !ww { continue; }
            let h = crate::proc::tr(ufd as u64);
            let m = crate::netsys::readiness_any(h);
            if wr && m & (1 | 0x10 | 8) != 0 { outr[w] |= b; n += 1; }
            if ww && m & (4 | 8) != 0 { outw[w] |= b; n += 1; }
        }
        let done = n > 0 || deadline.is_some_and(|d| crate::tsc::mono_ns() >= d);
        if done {
            let words = nfds.min(1024).div_ceil(64);
            // SAFETY: S1 — the caller hands in an address of the process's memory that
            // `uaccess::gate` has already validated.
            unsafe {
                if rs != 0 { for w in 0..words { core::ptr::write_unaligned((rs as *mut u64).add(w), outr[w]); } }
                if ws != 0 { for w in 0..words { core::ptr::write_unaligned((ws as *mut u64).add(w), outw[w]); } }
                if es != 0 { for w in 0..words { core::ptr::write_unaligned((es as *mut u64).add(w), 0); } }
            }
            return n;
        }
        if crate::proc::sig_pending() { return -4; }
        wait_window();
    }
}
// ── System V shm (PostgreSQL's interlock segment): a region of identity memory, shared through a fork ──
const SV_N: usize = 8;
static mut SV_KEY: [u64; SV_N] = [0; SV_N];
static mut SV_ADDR: [u64; SV_N] = [0; SV_N];
static mut SV_SIZE: [u64; SV_N] = [0; SV_N];
fn sysv_shmget(key: u64, size: u64, flags: u64) -> i64 {
    // SAFETY: S2 — the SysV shm table (`SV_*`). THE CALLER HOLDS `shm_lock()`, for the
    // same reason as the POSIX one: `shmget(IPC_CREAT)` is a find-or-create.
    unsafe {
        // IPC_EXCL -> EEXIST
        for i in 0..SV_N { if SV_ADDR[i] != 0 && SV_KEY[i] == key && key != 0 { if flags & 0o2000 != 0 { return -17; } return i as i64 + 1; } }
        if flags & 0o1000 == 0 { return -2; } // there is no IPC_CREAT -> ENOENT
        shm_gate_delay();                    // the same window as in `shm_lookup`
        for i in 0..SV_N {
            if SV_ADDR[i] == 0 {
                let pages = size.max(4096).div_ceil(4096);
                let b = match crate::mem::alloc_contig(pages) { Some(b) => b, None => return -12 };
                core::ptr::write_bytes(b as *mut u8, 0, (pages * 4096) as usize);
                crate::paging::protect(b, pages * 4096, true, false);
                SV_KEY[i] = key; SV_ADDR[i] = b; SV_SIZE[i] = pages * 4096;
                return i as i64 + 1;
            }
        }
        -28
    }
}
fn sysv_shmat(id: u64, _addr: u64) -> i64 {
    let i = id.wrapping_sub(1) as usize;
    // SAFETY: S2 — the SysV shm table (`SV_*`). THE CALLER HOLDS `shm_lock()`: the row is
    // read and its region is handed to this address space, while a neighbour may be
    // creating a segment in a free row right now.
    unsafe { if i >= SV_N || SV_ADDR[i] == 0 { return -22; } crate::paging::protect(SV_ADDR[i], SV_SIZE[i], true, false); SV_ADDR[i] as i64 }
}
fn sysv_shmctl(id: u64, cmd: u64, buf: u64) -> i64 {
    let i = id.wrapping_sub(1) as usize;
    // SAFETY: S2 — the SysV shm table (`SV_*`). THE CALLER HOLDS `shm_lock()`: `IPC_RMID`
    // FREES a row, and a creator scanning for a free one must not see it half-freed.
    // S1 for the `IPC_STAT` buffer — a pointer argument `uaccess::gate` validated.
    unsafe {
        if i >= SV_N || SV_ADDR[i] == 0 { return -22; }
        match cmd {
            2 => { if buf != 0 { core::ptr::write_bytes(buf as *mut u8, 0, 112); core::ptr::write_unaligned((buf + 48) as *mut u64, SV_SIZE[i]);
                core::ptr::write_unaligned((buf + 72) as *mut i32, 1); core::ptr::write_unaligned((buf + 80) as *mut u64, 1); } 0 }
                // IPC_STAT: shm_segsz, cpid, nattch
            0 => { SV_ADDR[i] = 0; SV_KEY[i] = 0; 0 } // IPC_RMID (we do not return the memory: the processes may hold it)
            _ => 0,
        }
    }
}

// `syscall_entry` — the SYSCALL stub — is `arch::syscall_entry` (x86-64 asm).
