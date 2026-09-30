//! The Linux arm64 syscall numbers -> the kernel's internal numbers (docs/ARM64-PLAN.md
//! §4.4). The dispatcher (`syscall.rs`) and the pointer gate (`uaccess.rs`) match on
//! x86-64's numbers, which are therefore the INTERNAL numbering; an arm64 process uses
//! the asm-generic table (`openat` = 56, `clone` = 220, `mmap` = 222 ...), and the SVC
//! entry translates with `to_internal` before dispatching. Twenty-nine calls the x86
//! dispatcher implements have no arm64 number at all (`open`, `stat`, `poll`, `fork`,
//! `pipe`, `dup2`, `epoll_create`, `epoll_wait`, `arch_prctl` ...): an arm64 libc never
//! issues them, so they are simply absent from the map.
//!
//! The table was derived from the two toolchains' headers (`asm/unistd_64.h` on the host,
//! `asm-generic/unistd.h` from the aarch64 sysroot) for every number `syscall.rs` has a
//! match arm for, and is checked by the host tests below — including that no two arm64
//! numbers land on one internal number. Talaria's own calls (1000+, 4600+) are the same
//! number on both architectures.

/// (the arm64 number, the internal number), sorted by the arm64 number.
const MAP: [(u16, u16); 160] = [
    ( 17,  79), // getcwd
    ( 19, 290), // eventfd2
    ( 20, 291), // epoll_create1
    ( 21, 233), // epoll_ctl
    ( 22, 281), // epoll_pwait
    ( 23,  32), // dup
    ( 24, 292), // dup3
    ( 25,  72), // fcntl
    ( 29,  16), // ioctl
    ( 32,  73), // flock
    ( 34, 258), // mkdirat
    ( 35, 263), // unlinkat
    ( 36, 266), // symlinkat
    ( 37, 265), // linkat
    ( 38, 264), // renameat
    ( 43, 137), // statfs
    ( 44, 138), // fstatfs
    ( 45,  76), // truncate
    ( 46,  77), // ftruncate
    ( 47, 285), // fallocate
    ( 48, 269), // faccessat
    ( 49,  80), // chdir
    ( 50,  81), // fchdir
    ( 52,  91), // fchmod
    ( 53, 268), // fchmodat
    ( 54, 260), // fchownat
    ( 55,  93), // fchown
    ( 56, 257), // openat
    ( 57,   3), // close
    ( 59, 293), // pipe2
    ( 61, 217), // getdents64
    ( 62,   8), // lseek
    ( 63,   0), // read
    ( 64,   1), // write
    ( 65,  19), // readv
    ( 66,  20), // writev
    ( 67,  17), // pread64
    ( 68,  18), // pwrite64
    ( 69, 295), // preadv
    ( 70, 296), // pwritev
    ( 72, 270), // pselect6
    ( 73, 271), // ppoll
    ( 74, 289), // signalfd4
    ( 78, 267), // readlinkat
    ( 79, 262), // newfstatat
    ( 80,   5), // fstat
    ( 81, 162), // sync
    ( 82,  74), // fsync
    ( 83,  75), // fdatasync
    ( 84, 277), // sync_file_range
    ( 85, 283), // timerfd_create
    ( 86, 286), // timerfd_settime
    ( 87, 287), // timerfd_gettime
    ( 88, 280), // utimensat
    ( 90, 125), // capget
    ( 91, 126), // capset
    ( 93,  60), // exit
    ( 94, 231), // exit_group
    ( 96, 218), // set_tid_address
    ( 98, 202), // futex
    ( 99, 273), // set_robust_list
    (101,  35), // nanosleep
    (102,  36), // getitimer
    (103,  38), // setitimer
    (113, 228), // clock_gettime
    (114, 229), // clock_getres
    (115, 230), // clock_nanosleep
    (118, 142), // sched_setparam
    (119, 144), // sched_setscheduler
    (120, 145), // sched_getscheduler
    (122, 203), // sched_setaffinity
    (123, 204), // sched_getaffinity
    (124,  24), // sched_yield
    (125, 146), // sched_get_priority_max
    (126, 147), // sched_get_priority_min
    (127, 148), // sched_rr_get_interval
    (129,  62), // kill
    (130, 200), // tkill
    (131, 234), // tgkill
    (132, 131), // sigaltstack
    (133, 130), // rt_sigsuspend
    (134,  13), // rt_sigaction
    (135,  14), // rt_sigprocmask
    (136, 127), // rt_sigpending
    (137, 128), // rt_sigtimedwait
    (139,  15), // rt_sigreturn
    (140, 141), // setpriority
    (143, 114), // setregid
    (144, 106), // setgid
    (145, 113), // setreuid
    (146, 105), // setuid
    (147, 117), // setresuid
    (149, 119), // setresgid
    (150, 120), // getresgid
    (153, 100), // times
    (154, 109), // setpgid
    (155, 121), // getpgid
    (157, 112), // setsid
    (159, 116), // setgroups
    (160,  63), // uname
    (163,  97), // getrlimit
    (165,  98), // getrusage
    (166,  95), // umask
    (167, 157), // prctl
    (168, 309), // getcpu
    (169,  96), // gettimeofday
    (172,  39), // getpid
    (173, 110), // getppid
    (174, 102), // getuid
    (175, 107), // geteuid
    (176, 104), // getgid
    (177, 108), // getegid
    (178, 186), // gettid
    (179,  99), // sysinfo
    (190,  64), // semget
    (194,  29), // shmget
    (195,  31), // shmctl
    (196,  30), // shmat
    (197,  67), // shmdt
    (198,  41), // socket
    (200,  49), // bind
    (201,  50), // listen
    (202,  43), // accept
    (203,  42), // connect
    (204,  51), // getsockname
    (205,  52), // getpeername
    (206,  44), // sendto
    (207,  45), // recvfrom
    (208,  54), // setsockopt
    (209,  55), // getsockopt
    (210,  48), // shutdown
    (211,  46), // sendmsg
    (212,  47), // recvmsg
    (214,  12), // brk
    (215,  11), // munmap
    (216,  25), // mremap
    (220,  56), // clone
    (221,  59), // execve
    (222,   9), // mmap
    (223, 221), // fadvise64
    (226,  10), // mprotect
    (227,  26), // msync
    (228, 149), // mlock
    (230, 151), // mlockall
    (231, 152), // munlockall
    (233,  28), // madvise
    (242, 288), // accept4
    (243, 299), // recvmmsg
    (260,  61), // wait4
    (261, 302), // prlimit64
    (267, 306), // syncfs
    (269, 307), // sendmmsg
    (276, 316), // renameat2
    (278, 318), // getrandom
    (283, 324), // membarrier
    (286, 327), // preadv2
    (287, 328), // pwritev2
    (291, 332), // statx
    (293, 334), // rseq
    (439, 439), // faccessat2
];

/// Talaria's own calls: the same numbers on every architecture (`syscall.rs`: 1000+ the
/// cell's and the process model's, 4600+ the parking `upark` family).
fn is_private(nr: u64) -> bool { (1000..1100).contains(&nr) || (4600..4700).contains(&nr) }

/// The internal (x86-64) number of an arm64 syscall number, or `None` for a number the
/// kernel does not implement.
pub fn to_internal(nr: u64) -> Option<u64> {
    if is_private(nr) { return Some(nr); }
    let key = nr as u16;
    MAP.binary_search_by_key(&key, |&(a, _)| a).ok().map(|i| MAP[i].1 as u64)
}

/// Bring the six arguments into the internal (x86-64) order. One call differs: arm64's
/// `clone(flags, stack, parent_tid, TLS, child_tid)` has the last two the other way
/// round from x86-64's `clone(flags, stack, parent_tid, child_tid, tls)` (glibc's and
/// musl's `clone.S` for aarch64 say so) — a swap that, missed, gives a new thread a
/// garbage TLS and a `child_tid` written into it. `nr` is the INTERNAL number.
#[inline]
pub fn normalize(nr: u64, args: &mut [u64; 6]) {
    if nr == 56 { args.swap(3, 4); }
}

// ── the structures and flag values that differ (docs/ARM64-PLAN.md §4.4) ─────────────
//
// The handlers write and read x86-64's layouts — they are the kernel's internal ones, as
// the numbers are. Three things an arm64 libc sees differently, and each is translated at
// the dispatcher's door rather than in the dozen places that fill them:
//
// * `struct stat`: 128 bytes with `st_mode` before a 32-bit `st_nlink` (x86-64: 144 bytes,
//   a 64-bit `st_nlink` first). An untranslated `fstat` does not only show a wrong mode —
//   it writes 16 bytes PAST the caller's structure (the first arm64 `sigtest` returned from
//   `main` into address 0: the saved x29/x30 lay right after its `struct stat`);
// * `struct epoll_event`: 16 bytes with `data` at 8 (x86-64 packs it into 12, `data` at 4);
// * `O_DIRECTORY`/`O_NOFOLLOW`/`O_DIRECT`/`O_LARGEFILE`: the same four bits, in two pairs
//   that change places.

/// `struct stat` sizes: the internal (x86-64) one and arm64's.
pub const STAT_INTERNAL: usize = 144;
pub const STAT_SIZE: usize = 128;
/// `struct epoll_event` sizes likewise.
pub const EPOLL_EVENT_INTERNAL: usize = 12;
pub const EPOLL_EVENT_SIZE: usize = 16;

/// The kernel-side buffer a call's structure is bounced through (`enter`/`leave`).
pub struct Bounce { buf: core::mem::MaybeUninit<[u8; STAT_INTERNAL]> }
impl Bounce {
    #[inline(always)]
    pub fn uninit() -> Self { Bounce { buf: core::mem::MaybeUninit::uninit() } }
    #[inline(always)]
    fn ptr(&mut self) -> *mut u8 { self.buf.as_mut_ptr() as *mut u8 }
}

/// An x86-64 `struct stat` as arm64's.
pub fn stat_from_internal(x: &[u8; STAT_INTERNAL]) -> [u8; STAT_SIZE] {
    let mut a = [0u8; STAT_SIZE];
    a[0..16].copy_from_slice(&x[0..16]);        // st_dev, st_ino
    a[16..20].copy_from_slice(&x[24..28]);      // st_mode
    a[20..24].copy_from_slice(&x[16..20]);      // st_nlink: the low 32 bits of x86's 64
    a[24..32].copy_from_slice(&x[28..36]);      // st_uid, st_gid
    a[32..40].copy_from_slice(&x[40..48]);      // st_rdev
    a[48..56].copy_from_slice(&x[48..56]);      // st_size
    a[56..60].copy_from_slice(&x[56..60]);      // st_blksize: 32 bits here
    a[64..120].copy_from_slice(&x[64..120]);    // st_blocks, the three timestamps
    a
}

/// arm64's `struct epoll_event` as the packed internal one, and back.
pub fn epoll_event_to_internal(a: &[u8; EPOLL_EVENT_SIZE]) -> [u8; EPOLL_EVENT_INTERNAL] {
    let mut x = [0u8; EPOLL_EVENT_INTERNAL];
    x[0..4].copy_from_slice(&a[0..4]);
    x[4..12].copy_from_slice(&a[8..16]);
    x
}
pub fn epoll_event_from_internal(x: &[u8; EPOLL_EVENT_INTERNAL]) -> [u8; EPOLL_EVENT_SIZE] {
    let mut a = [0u8; EPOLL_EVENT_SIZE];
    a[0..4].copy_from_slice(&x[0..4]);
    a[8..16].copy_from_slice(&x[4..12]);
    a
}

/// The open flags between the two numberings. arm64: `O_DIRECTORY` 0x4000, `O_NOFOLLOW`
/// 0x8000, `O_DIRECT` 0x10000, `O_LARGEFILE` 0x20000; x86-64: `O_DIRECT` 0x4000,
/// `O_LARGEFILE` 0x8000, `O_DIRECTORY` 0x10000, `O_NOFOLLOW` 0x20000 — the pairs at bits
/// 14-15 and 16-17 change places, so the same function goes both ways.
#[inline]
pub fn oflags(f: u64) -> u64 { (f & !0x3C000) | ((f & 0xC000) << 2) | ((f & 0x3_0000) >> 2) }

/// Before the dispatch: the flag words into the internal numbering, and a structure the
/// handler READS rebuilt in `b` with the pointer argument moved onto it. A structure the
/// handler WRITES gets `b` as its destination — `leave` carries it out. `nr` is internal,
/// the pointers have passed `uaccess::gate`.
/// # Safety
/// The pointer arguments must be the process's own, readable for the structure's size.
#[inline]
pub unsafe fn enter(nr: u64, args: &mut [u64; 6], b: &mut Bounce) {
    match nr {
        257 => args[2] = oflags(args[2]),                              // openat(dirfd, path, flags, mode)
        293 => args[1] = oflags(args[1]),                              // pipe2(fds, flags): O_DIRECT
        72 if args[1] == 4 => args[2] = oflags(args[2]),               // fcntl(F_SETFL)
        5 => args[1] = b.ptr() as u64,                                 // fstat(fd, st)
        262 => args[2] = b.ptr() as u64,                               // newfstatat(dirfd, path, st, flags)
        233 if args[3] != 0 => {                                       // epoll_ctl(ep, op, fd, event)
            let a = core::ptr::read_unaligned(args[3] as *const [u8; EPOLL_EVENT_SIZE]);
            core::ptr::write_unaligned(b.ptr() as *mut [u8; EPOLL_EVENT_INTERNAL], epoll_event_to_internal(&a));
            args[3] = b.ptr() as u64;
        }
        _ => {}
    }
}

/// After the dispatch: what the handler wrote, in the process's layout, at the process's
/// pointer (`user` are the arguments as they were before `enter`); the result, translated
/// where it is a flag word.
/// # Safety
/// As `enter`; `b` is the buffer `enter` was given for this call.
#[inline]
pub unsafe fn leave(nr: u64, user: &[u64; 6], b: &mut Bounce, r: u64) -> u64 {
    match nr {
        5 | 262 if r == 0 => {
            let dst = if nr == 5 { user[1] } else { user[2] };
            let x = core::ptr::read_unaligned(b.ptr() as *const [u8; STAT_INTERNAL]);
            core::ptr::write_unaligned(dst as *mut [u8; STAT_SIZE], stat_from_internal(&x));
        }
        // epoll_pwait(ep, events, max, ...): `r` packed events lie at the start of an array
        // sized for `max` 16-byte ones — spread them out from the last one down.
        281 if (r as i64) > 0 => {
            let base = user[1] as *mut u8;
            for i in (0..r as usize).rev() {
                let x = core::ptr::read_unaligned(base.add(i * EPOLL_EVENT_INTERNAL) as *const [u8; EPOLL_EVENT_INTERNAL]);
                core::ptr::write_unaligned(base.add(i * EPOLL_EVENT_SIZE) as *mut [u8; EPOLL_EVENT_SIZE], epoll_event_from_internal(&x));
            }
        }
        72 if user[1] == 3 && (r as i64) >= 0 => return oflags(r),     // fcntl(F_GETFL)
        _ => {}
    }
    r
}

/// `utsname.machine`.
pub const UTS_MACHINE: &[u8] = b"aarch64";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_lands_in_arm64s_fields() {
        let mut x = [0u8; STAT_INTERNAL];
        x[0..8].copy_from_slice(&0x1122u64.to_le_bytes());        // st_dev
        x[8..16].copy_from_slice(&77u64.to_le_bytes());           // st_ino
        x[16..24].copy_from_slice(&3u64.to_le_bytes());           // st_nlink
        x[24..28].copy_from_slice(&0o40755u32.to_le_bytes());     // st_mode
        x[28..32].copy_from_slice(&1000u32.to_le_bytes());        // st_uid
        x[32..36].copy_from_slice(&1001u32.to_le_bytes());        // st_gid
        x[40..48].copy_from_slice(&0x0501u64.to_le_bytes());      // st_rdev
        x[48..56].copy_from_slice(&123456789u64.to_le_bytes());   // st_size
        x[56..64].copy_from_slice(&4096u64.to_le_bytes());        // st_blksize
        x[64..72].copy_from_slice(&88u64.to_le_bytes());          // st_blocks
        x[72..80].copy_from_slice(&1700000000u64.to_le_bytes());  // st_atime
        x[104..112].copy_from_slice(&1700000002u64.to_le_bytes());// st_ctime
        let a = stat_from_internal(&x);
        let u32at = |o: usize| u32::from_le_bytes(a[o..o + 4].try_into().unwrap());
        let u64at = |o: usize| u64::from_le_bytes(a[o..o + 8].try_into().unwrap());
        assert_eq!((u64at(0), u64at(8)), (0x1122, 77));
        assert_eq!((u32at(16), u32at(20), u32at(24), u32at(28)), (0o40755, 3, 1000, 1001));
        assert_eq!((u64at(32), u64at(40), u64at(48)), (0x0501, 0, 123456789));
        assert_eq!((u32at(56), u32at(60), u64at(64)), (4096, 0, 88));
        assert_eq!((u64at(72), u64at(104), u64at(120)), (1700000000, 1700000002, 0));
    }

    #[test]
    fn an_epoll_event_goes_there_and_back() {
        let mut a = [0u8; EPOLL_EVENT_SIZE];
        a[0..4].copy_from_slice(&0x8000_0001u32.to_le_bytes());   // EPOLLIN | EPOLLET
        a[4..8].copy_from_slice(&0xDEADu32.to_le_bytes());        // the padding: must not travel
        a[8..16].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        let x = epoll_event_to_internal(&a);
        assert_eq!(u32::from_le_bytes(x[0..4].try_into().unwrap()), 0x8000_0001);
        assert_eq!(u64::from_le_bytes(x[4..12].try_into().unwrap()), 0x1122_3344_5566_7788);
        let back = epoll_event_from_internal(&x);
        assert_eq!(&back[0..4], &a[0..4]);
        assert_eq!(&back[4..8], &[0u8; 4]);
        assert_eq!(&back[8..16], &a[8..16]);
    }

    #[test]
    fn a_packed_array_of_events_spreads_in_place() {
        // what `leave` does for epoll_pwait: three packed events at the start of a 4-slot array
        let mut buf = [0u8; 4 * EPOLL_EVENT_SIZE];
        for i in 0..3u64 {
            let o = i as usize * EPOLL_EVENT_INTERNAL;
            buf[o..o + 4].copy_from_slice(&(i as u32 + 1).to_le_bytes());
            buf[o + 4..o + 12].copy_from_slice(&(0xA0 + i).to_le_bytes());
        }
        let user = [0, buf.as_mut_ptr() as u64, 4, 0, 0, 0];
        let mut b = Bounce::uninit();
        assert_eq!(unsafe { leave(281, &user, &mut b, 3) }, 3);
        for i in 0..3usize {
            let o = i * EPOLL_EVENT_SIZE;
            assert_eq!(u32::from_le_bytes(buf[o..o + 4].try_into().unwrap()), i as u32 + 1);
            assert_eq!(u64::from_le_bytes(buf[o + 8..o + 16].try_into().unwrap()), 0xA0 + i as u64);
        }
    }

    #[test]
    fn the_open_flags_change_places_in_pairs_and_the_rest_stays() {
        assert_eq!(oflags(0x4000), 0x10000);   // O_DIRECTORY
        assert_eq!(oflags(0x8000), 0x20000);   // O_NOFOLLOW
        assert_eq!(oflags(0x10000), 0x4000);   // O_DIRECT
        assert_eq!(oflags(0x20000), 0x8000);   // O_LARGEFILE
        let rest = 0o2 | 0o100 | 0o1000 | 0o2000 | 0o4000 | 0x80000; // RDWR CREAT TRUNC APPEND NONBLOCK CLOEXEC
        assert_eq!(oflags(rest | 0x4000), rest | 0x10000);
        for f in [0u64, 0x4000, 0x3C000, 0x24000, rest | 0x18000] { assert_eq!(oflags(oflags(f)), f); }
    }

    #[test]
    fn fstat_is_bounced_and_carried_out() {
        let mut user_st = [0xEEu8; STAT_SIZE + 16];              // 16 guard bytes after the structure
        let mut args = [3, user_st.as_mut_ptr() as u64, 0, 0, 0, 0];
        let user = args;
        let mut b = Bounce::uninit();
        unsafe {
            enter(5, &mut args, &mut b);
            assert_ne!(args[1], user[1]);
            let mut x = [0u8; STAT_INTERNAL];
            x[24..28].copy_from_slice(&0o100644u32.to_le_bytes());
            x[48..56].copy_from_slice(&10u64.to_le_bytes());
            core::ptr::write_unaligned(args[1] as *mut [u8; STAT_INTERNAL], x);  // the handler
            assert_eq!(leave(5, &user, &mut b, 0), 0);
        }
        assert_eq!(u32::from_le_bytes(user_st[16..20].try_into().unwrap()), 0o100644);
        assert_eq!(u64::from_le_bytes(user_st[48..56].try_into().unwrap()), 10);
        assert_eq!(&user_st[STAT_SIZE..], &[0xEEu8; 16]);       // nothing past the 128 bytes
    }

    #[test]
    fn the_calls_a_libc_issues_land_on_the_right_handler() {
        // (arm64, x86-64) for the calls every musl binary makes on its first breath
        for (a, x) in [(63, 0), (64, 1), (57, 3), (56, 257), (222, 9), (215, 11), (226, 10), (214, 12),
                       (220, 56), (98, 202), (94, 231), (93, 60), (139, 15), (134, 13), (135, 14),
                       (79, 262), (80, 5), (96, 218), (99, 273), (160, 63), (113, 228), (261, 302)] {
            assert_eq!(to_internal(a), Some(x), "arm64 {a}");
        }
    }

    #[test]
    fn a_number_arm64_does_not_have_is_not_reachable_and_the_private_ones_pass() {
        assert_eq!(to_internal(999), None);   // below the private range, above every asm-generic number
        assert_eq!(to_internal(2), None);     // io_submit: not implemented
        assert_eq!(to_internal(u64::MAX), None);
        assert_eq!(to_internal(1100), None);
        assert_eq!(to_internal(1000), Some(1000));
        assert_eq!(to_internal(4602), Some(4602));
    }

    #[test]
    fn the_table_is_sorted_and_one_to_one() {
        for w in MAP.windows(2) { assert!(w[0].0 < w[1].0, "{:?}", w); }
        let mut internal: Vec<u16> = MAP.iter().map(|&(_, x)| x).collect();
        internal.sort_unstable();
        internal.dedup();
        assert_eq!(internal.len(), MAP.len());
    }

    #[test]
    fn clone_has_its_tls_and_child_tid_swapped_and_nothing_else_moves() {
        let mut a = [1, 2, 3, 4, 5, 6];
        normalize(56, &mut a);
        assert_eq!(a, [1, 2, 3, 5, 4, 6]);
        let mut b = [1, 2, 3, 4, 5, 6];
        normalize(9, &mut b);
        assert_eq!(b, [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn the_legacy_calls_are_absent_on_purpose() {
        // x86's `open`(2), `stat`(4), `fork`(57), `pipe`(22), `epoll_wait`(232) have no arm64
        // number; the numbers that LOOK like them on arm64 mean other calls.
        assert_ne!(to_internal(2), Some(2));   // arm64 2 = io_submit (not implemented)
        assert_ne!(to_internal(57), Some(57)); // arm64 57 = close
        assert_eq!(to_internal(57), Some(3));
    }
}
