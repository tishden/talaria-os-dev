//! The self-tests of the Linux ABI in ring 3: ELF, C++, signals, fork/exec, threads.
//!
//! The name of the mode is compared AS A WHOLE (a `match` on the value of `talaria.test=`),
//! so a name that is a prefix of a foreign name can no longer start a foreign mode
//! — see `crate::cmdline`.

use core::sync::atomic::Ordering;

/// Run the self-test `name`, if it is one of this set.
/// Returns `true` if the name was recognised (the body may well not have returned — many
/// modes end with `suite::park()`).
// Many gates do not return at all (`suite::park()` or the start of a workload),
// so in the sets where ALL the modes do that, the trailing `true` is unreachable.
#[allow(unused_variables, unreachable_code)]
pub fn run(name: &str, cmdline: &str) -> bool {
    match name {
        "musl" => {
            static MHELLO: &[u8] = user_elf!("mhello");
            println!("talaria: a musl static ELF, {} bytes — the full Linux ABI (TLS, auxv)", MHELLO.len());
            crate::user::run(MHELLO);
        }
        "user" => {
            static HELLO: &[u8] = user_elf!("hello.elf");
            println!("talaria: a built-in ELF, {} bytes — the Linux syscall ABI", HELLO.len());
            crate::user::run(HELLO);
        }
        "procdisk" => {
            // A1.3: two disk workloads, each in a private AS (identical vaddrs of the stack/heap).
            if crate::fs::init() {
                println!("talaria: two processes FROM THE DISK in separate ASes (A1.3: the run_disk class is isolated)");
                unsafe { crate::user::run_processes_disk(&[b"mhello\0", b"mhello\0"]); }
            }
        }
        "cpp3" => {
            static CP3: &[u8] = user_elf!("cpptest3");
            println!("talaria: the C++ iostream test, {} bytes (cout/map/sort)", CP3.len());
            crate::user::run(CP3);
        }
        "cpp2" => {
            static CP2: &[u8] = user_elf!("cpptest2");
            println!("talaria: the C++ threads test, {} bytes (std::thread/mutex/chrono)", CP2.len());
            unsafe { crate::user::run_threaded(CP2); }
        }
        "cppdtor" => {
            // RELEASE-1.0 §1.3: a clean C++ exit — global destructors that go into the futex path
            // (a thread+mutex+condvar AFTER the return from main). The historical #DE on exit (v0.40) does not
            // reproduce (it was closed by the lapic_enable invariants + the futex fixes of v0.51+) — the test holds it.
            static CPD: &[u8] = user_elf!("cppdtor");
            println!("talaria: cppdtor — global destructors with a futex path on exit");
            unsafe { crate::user::run_threaded(CPD); }
        }
        "stop" => {
            // talaria.stop=N: the process /sigtest sleep waits for a SIGTERM and exits cleanly (the handler prints a line).
            if crate::fs::init() { unsafe { crate::user::run_process_disk(b"/sigtest", &[b"sigtest", b"sleep"], &[b"HOME=/"]); } }
            crate::suite::park()
        }
        "pwrbtnv" => {
            // The second boot after the button: the file is in place AND the journal is clean (start 0 —
            // without a replay: the sync before S5 made a checkpoint).
            if crate::fs::init() {
                let mut buf = [0u8; 64];
                let h = crate::ext2::open(b"/pwrbtn.txt", 0, 0);
                let n = if h >= 0 { let n = crate::ext2::read(h, buf.as_mut_ptr(), buf.len()); crate::ext2::close(h); n } else { -1 };
                let want: &[u8] = b"pwrbtn: dirty data reached disk\n";
                if n == want.len() as i64 && &buf[..want.len()] == want { println!("pwrbtn: PERSISTED OK"); }
                else { println!("pwrbtn: FAIL — the file is not on the disk after the power button (open={}, read {})", h, n); }
            }
            crate::suite::park()
        }
        "pwrbtn" | "pwrbtnign" => {
            // The power button (stop-instances): the file is written NOT under an fsync and lies in
            // the block cache; the process waits for a SIGTERM. `pwrbtn` exits by it cleanly
            // (the path "the workload has left -> sync -> S5"), `pwrbtnign` ignores it
            // (the path "the deadline has passed -> a sync from the tick -> S5"). From the outside the gate presses
            // the button through QMP and reads the file from the image: without a sync it is not there.
            if crate::fs::init() {
                let h = crate::ext2::open(b"/pwrbtn.txt", 0o1101, 0o644); // O_WRONLY|O_CREAT|O_TRUNC
                let m: &[u8] = b"pwrbtn: dirty data reached disk\n";
                if h >= 0 { crate::ext2::write(h, m.as_ptr(), m.len()); crate::ext2::close(h); }
                println!("pwrbtn: the file was written without an fsync (dirty blocks {}), I am waiting for the button", crate::ext2::dirty_count());
                let arg: &[u8] = if name == "pwrbtnign" { b"ignore" } else { b"sleep" };
                unsafe { crate::user::run_process_disk(b"/sigtest", &[b"sigtest", arg], &[b"HOME=/"]); }
            }
            crate::suite::park()
        }
        "mbench" => {
            crate::smp::start_from_cmdline(cmdline); // talaria.smp=N works here too
            // The primitives of a hot loop (ALU, the caches, the memory, a TLB miss, a syscall) —
            // with the same binary that is run on Linux: the gap with it is localised
            // by measurement rather than by enumerating hypotheses.
            if crate::fs::init() { unsafe { crate::user::run_process_disk(b"/mbench", &[b"mbench"], &[b"HOME=/"]); } }
            crate::suite::park()
        }
        "fsread" => {
            crate::smp::start_from_cmdline(cmdline);
            // THE SPEED OF READING A FILE — with the same binary that goes to Linux.
            // The gate watches one property: reading in 4 KB pieces must not
            // be a multiple of times slower than reading in megabyte pieces. Otherwise it
            // means that the read-ahead window is again computed from the REQUEST'S SIZE
            // rather than from the behaviour — and every block costs a separate access to
            // the device (it was exactly like that: 169 MB/s against 1680).
            if crate::fs::init() {
                unsafe { crate::user::run_process_disk(b"/fsread", &[b"fsread", b"/big.dat", b"1024", b"96"], &[b"HOME=/"]); }
            }
            crate::suite::park()
        }
        "fputest" => {
            crate::smp::start_from_cmdline(cmdline); // talaria.smp=N works here too
            // The state of the FPU/AVX: a syscall, a signal handler and pre-emption must not
            // spoil the YMM registers (the kernel saves them with an XSAVE — both in the scheduler
            // and in the signal's frame).
            if crate::fs::init() { unsafe { crate::user::run_process_disk(b"/fputest", &[b"fputest"], &[b"HOME=/"]); } }
            crate::suite::park()
        }
        "execmt" => {
            crate::smp::start_from_cmdline(cmdline); // talaria.smp=N works here too
            // A multi-threaded execve: three threads (one sleeps on a futex), then an exec onto itself.
            // What is checked is both that the sisters are taken off the cores and that the new image lives.
            if crate::fs::init() { unsafe { crate::user::run_process_disk(b"/execmt", &[b"execmt"], &[b"HOME=/"]); } }
            crate::suite::park()
        }
        "sigtesthi" => {
            // The same binary linked at 0x4000_0000 (x86's GB1 of RAM): under strict the image, its
            // forks and its shared memory live where the kernel's identity used to be.
            if crate::fs::init() { unsafe { crate::user::run_process_disk(b"/sigtest-hi", &[b"sigtest"], &[b"HOME=/"]); } }
            crate::suite::park()
        }
        "sigtestlo" => {
            // The same binary linked at 0x20_0000 — on x86 the kernel image's own load address
            // (stage 7.4): the image runs at its high alias, a strict process space has none of
            // its identity, so the process's image, forks and shared memory live there.
            if crate::fs::init() { unsafe { crate::user::run_process_disk(b"/sigtest-lo", &[b"sigtest"], &[b"HOME=/"]); } }
            crate::suite::park()
        }
        "sigtest" => {
            // The process model: fd tables, a pipe, fork+SIGCHLD, kill/handlers, cwd, shm — a musl test from the ext2 disk.
            if crate::fs::init() { unsafe { crate::user::run_process_disk(b"/sigtest", &[b"sigtest"], &[b"HOME=/"]); } }
            crate::suite::park()
        }
        "dyn" => {
            // A dynamic ELF: PIE + PT_INTERP (ld-musl), a .so through a file mmap/MAP_FIXED, dlopen.
            if crate::fs::init() { unsafe { crate::user::run_process_disk(b"/hello", &[b"/hello", b"arg1"], &[b"HOME=/", b"LD_LIBRARY_PATH=/lib"]); } }
            crate::suite::park()
        }
        "exec" => {
            static MHELLO: &[u8] = user_elf!("mhello");
            println!("talaria: an exec of a musl binary, {} bytes, in the process's private address space", MHELLO.len());
            unsafe { crate::user::exec(MHELLO); }
        }
        "efault" => {
            // Pointer arguments the process does not own -> EFAULT, kernel intact
            // (uaccess.rs). In its own address space, as every workload runs.
            static EF: &[u8] = user_elf!("efault");
            println!("talaria: efault — syscalls with pointers into the kernel, unmapped, read-only");
            unsafe { crate::user::run_processes(&[EF]); }
        }
        "forkp" => {
            static FT: &[u8] = user_elf!("forktest.elf");
            println!("talaria: a fork of a process — the parent and the child in separate ASes");
            unsafe { crate::user::run_processes(&[FT]); } // the fork inside will add a second task
        }
        "procs" => {
            static HELLO: &[u8] = user_elf!("hello.elf");
            static MHELLO: &[u8] = user_elf!("mhello");
            println!("talaria: two processes in separate address spaces under the scheduler");
            unsafe { crate::user::run_processes(&[HELLO, MHELLO]); }
        }
        "fork" => {
            // The memory of a fork: the clone of an AS copies the private pages; a write in the child is not
            // visible to the parent (copy semantics).
            println!("talaria: the test of the cloning of an address space (the memory of a fork)");
            let orig = crate::paging::current_cr3();
            let pp = crate::mem::alloc_frame().unwrap();
            let parent = crate::paging::new_address_space();
            const V: u64 = 0x1_0000_0000;
            crate::paging::map_in(parent, V, pp, true, false);
            let (pval, cval1, cval2, pval2) = unsafe {
                crate::paging::switch(parent); core::ptr::write_volatile(crate::mem::vptr::<u64>(V), 0x1111);
                let child = crate::paging::fork_address_space(parent); // the clone AFTER the write of 0x1111
                crate::paging::switch(child);  let cval1 = core::ptr::read_volatile(crate::mem::vptr::<u64>(V)); // we expect 0x1111
                core::ptr::write_volatile(crate::mem::vptr::<u64>(V), 0x2222); // change it in the child
                let cval2 = core::ptr::read_volatile(crate::mem::vptr::<u64>(V));
                crate::paging::switch(parent); let pval2 = core::ptr::read_volatile(crate::mem::vptr::<u64>(V)); // we expect still 0x1111
                crate::paging::switch(orig);
                (0x1111u64, cval1, cval2, pval2)
            };
            println!("vmspace-fork: the parent={:#x} the child(after the clone)={:#x} the child(after the write)={:#x} the parent(after)={:#x}",
                pval, cval1, cval2, pval2);
            if cval1 == 0x1111 && cval2 == 0x2222 && pval2 == 0x1111 {
                println!("fork: THE MEMORY WAS CLONED (the child inherited 0x1111, the child's write is not visible to the parent)");
            } else {
                println!("fork: FAILURE");
            }
            crate::suite::park()
        }
        "cpp" => {
            static CP: &[u8] = user_elf!("cpptest");
            println!("talaria: the C++ test, {} bytes (static-init, STL, exceptions)", CP.len());
            crate::user::run(CP);
        }
        "pthreads" => {
            static PT: &[u8] = user_elf!("pthreads");
            println!("talaria: the pthreads test, {} bytes (clone+futex+mutex+join)", PT.len());
            unsafe { crate::user::run_threaded(PT); }
        }
        // Four threads create shared memory AT THE SAME INSTANT: one name, own names,
        // one SysV key. Without the lock of the tables (`syscall::shm_lock`) two threads
        // that miss the lookup both take a free row, and the halves of a workload map
        // DIFFERENT memory while nothing crashes. The programme also prints how many
        // times a creator really had to wait — a race that did not happen proves nothing.
        "shmrace" => {
            // The APs must be UP, otherwise the four threads share the BSP and the race
            // cannot happen at all: the syscall path runs with interrupts masked (FMASK), so
            // a creator inside the section is not preempted and nobody contends with it.
            // That is also the answer to "is this reachable on one core" — it is not.
            crate::smp::start_from_cmdline(cmdline);
            static SR: &[u8] = user_elf!("shmrace");
            println!("talaria: the shmrace test, {} bytes (shm_open/shmget from four threads at once)", SR.len());
            unsafe { crate::user::run_threaded(SR); }
        }
        // A pipe and an eventfd with their ends on two cores at once: the bytes of the pipe
        // checked one by one at the reader, the eventfd's increments summed exactly. Both used
        // to be one counter that each end read, changed and wrote back with plain stores.
        "piperace" => {
            crate::smp::start_from_cmdline(cmdline);
            static PR: &[u8] = user_elf!("piperace");
            println!("talaria: the piperace test, {} bytes (a pipe and an eventfd across cores)", PR.len());
            unsafe { crate::user::run_threaded(PR); }
        }
        // arm64: MIDR/MPIDR/ID_AA64* read from EL0 — the emulation HWCAP_CPUID promises
        // (`arch::aarch64::trap::emulate_id_read`).
        #[cfg(target_arch = "aarch64")]
        "idregs" => {
            static IR: &[u8] = user_elf!("idregs");
            println!("talaria: the idregs test, {} bytes (the ID registers from EL0)", IR.len());
            unsafe { crate::user::run_processes(&[IR]); }
        }
        // A syscall that moves the thread to another core (`sched_setaffinity` from the boot
        // core) returns with EVERY register the thread had, not only the callee-saved ones.
        "migregs" => {
            crate::smp::start_from_cmdline(cmdline);
            static MR: &[u8] = user_elf!("migregs");
            println!("talaria: the migregs test, {} bytes (the registers across a move to another core)", MR.len());
            unsafe { crate::user::run_threaded(MR); }
        }
        // Two waiters in epoll_wait on sets that are not only sockets, one of them parked by
        // `talaria.netmix=`: a pipe's write, an eventfd's and a signal must wake the parked one
        // at once, not at the park's ceiling (`netsys::mix_kick`).
        "mixwait" => {
            crate::smp::start_from_cmdline(cmdline);
            static MW: &[u8] = user_elf!("mixwait");
            println!("talaria: the mixwait test, {} bytes (mixed epoll sets parked, woken by their events)", MW.len());
            unsafe { crate::user::run_threaded(MW); }
        }
        "threads" => {
            unsafe {
                let kmain = crate::mem::alloc_contig(4).unwrap() + 4 * 4096;
                crate::user::setup_thread_mode(kmain);
                // a page of ring3 code: inc qword [rdi]; jmp loop  (48 FF 07 EB FB)
                let code = crate::mem::alloc_frame().unwrap();
                let cp = crate::mem::kptr::<u8>(code);
                for (i, b) in [0x48u8, 0xFF, 0x07, 0xEB, 0xFB].iter().enumerate() { core::ptr::write(cp.add(i), *b); }
                // the page of counters (2 x u64)
                let ctr = crate::mem::alloc_frame().unwrap();
                core::ptr::write(crate::mem::kptr::<u64>(ctr), 0); // DMAP: ring 3 (thread mode, the identity) uses `code`/`ctr` by these physical addresses
                core::ptr::write(crate::mem::kptr::<u64>(ctr + 8), 0);
                CTR_BASE.store(ctr, Ordering::SeqCst);
                // the stacks: 2 user + 2 kernel
                let us0 = crate::mem::alloc_frame().unwrap() + 4096;
                let us1 = crate::mem::alloc_frame().unwrap() + 4096;
                let ks0 = crate::mem::alloc_frame().unwrap() + 4096;
                let ks1 = crate::mem::alloc_frame().unwrap() + 4096;
                println!("talaria: 2 ring-3 threads (an inc loop) + a ring0 printer, pre-emption by the timer");
                crate::sched::spawn(task_thread_printer, crate::mem::alloc_frame().unwrap() + 4096); // task 0 (ring0)
                crate::sched::spawn_user(code, us0, ks0, ctr, 0);       // task 1 (ring3) -> counter[0]
                crate::sched::spawn_user(code, us1, ks1, ctr + 8, 0);   // task 2 (ring3) -> counter[1]
                crate::smp::lapic_enable();
                crate::smp::lapic_timer_init(crate::smp::SLICE_NS);
                crate::sched::install();
                crate::sched::start();   // no irq_enable before it: see runmode's ipi path
            }
        }
        "sysbench" => {
            static SB: &[u8] = user_elf!("sysbench");
            println!("talaria: the microbenchmark of a syscall's cost, {} bytes", SB.len());
            crate::user::run(SB);
        }
        _ => return false,
    }
    true
}

/// A ring 0 observer for the `threads` gate: two ring 3 threads each spin a counter in
/// shared memory, this task prints both. Both grow -> the pre-emption works.
/// The address of the counters is placed by the gate itself (the buffer lives in the thread's address space).
static CTR_BASE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

extern "C" fn task_thread_printer() -> ! {
    use core::sync::atomic::Ordering;
    let base = CTR_BASE.load(Ordering::SeqCst);
    let mut last = 0u64;
    loop {
        for _ in 0..80_000_000u64 { core::hint::spin_loop(); }
        let c0 = unsafe { core::ptr::read_volatile(crate::mem::kptr::<u64>(base)) };
        let c1 = unsafe { core::ptr::read_volatile(crate::mem::kptr::<u64>(base + 8)) };
        println!("talaria: the ring3 threads -> the counters [{}] [{}] (both growing = the pre-emption works)", c0, c1);
        if c0 > 0 && c1 > 0 && last > 0 { /* proven */ }
        last = c0;
    }
}
