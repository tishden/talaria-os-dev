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

#[cfg(test)]
mod tests {
    use super::*;

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
