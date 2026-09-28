//! The self-checks of the file system and the disk: ext2, the journal, the htree, the block cache.
//!
//! The mode's name is compared IN FULL (a `match` by the value of `talaria.test=`),
//! so a name that is a prefix of a foreign name can no longer start a foreign mode
//! — see `crate::cmdline`.

use core::sync::atomic::Ordering;

/// Run the self-check `name` if it is from this set.
/// It returns `true` if the name was recognised (the body may not have returned — many
/// modes end with a `suite::park()`).
// Many gates do not return at all (a `suite::park()` or a launch of a workload),
// so in the sets where ALL the modes do that, the `true` at the end is unreachable.
#[allow(unused_variables, unreachable_code)]
pub fn run(name: &str, cmdline: &str) -> bool {
    match name {
        "fs" => {
            if crate::fs::init() {
                let fd = crate::fs::open(b"/hello.txt\0".as_ptr());
                println!("talaria: open(/hello.txt) -> fd={}", fd);
                if fd >= 0 {
                    let mut buf = [0u8; 256];
                    let n = crate::fs::read(fd, buf.as_mut_ptr(), buf.len());
                    println!("talaria: {} bytes were read from the disk:", n);
                    if n > 0 {
                        let sl = &buf[..n as usize];
                        for &b in sl { crate::serial::putb(b); }
                    }
                    crate::fs::close(fd);
                }
            }
        }
        "diskexec" => {
            if crate::fs::init() {
                static mut ELFBUF: [u8; 262144] = [0; 262144];
                let buf: &mut [u8] = unsafe { &mut *core::ptr::addr_of_mut!(ELFBUF) };
                let n = crate::fs::read_all(b"mhello\0", buf);
                println!("talaria: mhello has been loaded from the disk ({} bytes) — I execute it in ring 3", n);
                if n > 0 { crate::user::run(&buf[..n]); }
            }
        }
        "mapshared" => {
            // §8.5: a file MAP_SHARED — a process in its own AS (the block cache's pages
            // are mapped into the user AS through a map_leaf). A mode=cell workload comes to
            // this same path after §8.6 (it has its own address space too).
            // §3.3: with talaria.cell=identity the same workload goes into the kernel's identity AS —
            // the path where a file mapping previously required A CONTIGUOUS window
            // of the cache's slots (vaddr == phys) and returned an ENODEV on a scattered file.
            if crate::fs::init() && crate::ext2::active() {
                if crate::user::CELL_IDENTITY.load(Ordering::Relaxed) {
                    unsafe { crate::user::run_disk_threaded(b"/mapshared\0".as_ptr(), &[b"mapshared"], &[b"HOME=/"]); }
                }
                unsafe { crate::user::run_process_disk(b"/mapshared", &[b"mapshared"], &[b"HOME=/"]); }
            } else { println!("mapshared: ext2 is not mounted"); }
            crate::suite::park()
        }
        "fsbench" => {
            // §8.7: the price of an fsync. We write a file in blocks, measure the fsync itself and look at
            // how many REAL accesses to the disk it turned into (the ranges and the commands).
            if crate::fs::init() && crate::ext2::active() {
                let hz = crate::tsc::calibrate_cached();
                let mut buf = [0u8; 4096];
                for i in 0..4096 { buf[i] = (i * 7) as u8; }
                let nblocks: usize = 512;
                let h = crate::ext2::open(b"/fsbench.dat", 0o100 | 0o1 | 0o1000, 0o644); // O_CREAT|O_WRONLY|O_TRUNC
                if h < 0 { println!("fsbench: open fail {}", h); crate::suite::park() }
                let a_before = crate::ext2::alloc_scans();
                let t0 = crate::tsc::rdtsc();
                for _ in 0..nblocks { crate::ext2::write(h, buf.as_ptr(), buf.len()); }
                let t_wr = crate::tsc::rdtsc() - t0;
                let a_wr = crate::ext2::alloc_scans() - a_before;
                let dirty = crate::ext2::dirty_count();
                            let (w0, r0, f0) = crate::ext2::wr_stats(); let c0 = crate::fs::blk_cmds();
                let t1 = crate::tsc::rdtsc();
                crate::ext2::fsync(h);
                let t_fs = crate::tsc::rdtsc() - t1;
                let (w1, r1, f1) = crate::ext2::wr_stats(); let c1 = crate::fs::blk_cmds();
                // a second fsync without changes must be almost free
                let t2 = crate::tsc::rdtsc(); crate::ext2::fsync(h); let t_fs2 = crate::tsc::rdtsc() - t2;
                crate::ext2::close(h);
                let (jc, jb, _) = crate::ext2::jrn_stats();
                println!("fsbench: {} blocks of 4K: the write {} us, the fsync {} us (there were {} dirty), a repeated fsync {} us",
                    nblocks, t_wr * 1_000_000 / hz, t_fs * 1_000_000 / hz, dirty, t_fs2 * 1_000_000 / hz);
                println!("fsbench: the disk: the blocks {} the ranges {} the barriers {} the commands {} | the journal: the transactions {} the blocks {} the \
                    submissions {}",
                    w1 - w0, r1 - r0, f1 - f0, c1 - c0, jc, jb, crate::ext2::jrn_io());
                // §2.4: the trips into the bitmap per nblocks blocks of data must be
                // about nblocks/PREA_RUN (it used to be one per EVERY block).
                println!("fsbench: the allocation: {} trips into the bitmap per {} blocks of data", a_wr, nblocks);
                // THE QUEUE'S DEPTH. A one here would mean a queue of depth
                // one: a command is submitted — we wait for it — we submit the next one. For the sake of
                // this number being greater than one (and of a barrier not occupying
                // the queue entirely) the separation "submit"/"wait for one's own"
                // was made — with virtio through a table of slots, with NVMe through the CID.
                use core::sync::atomic::Ordering::Relaxed;
                println!("fsbench: the queue: {} commands submitted, the greatest depth {}, the queue was full {} times",
                    crate::fs::IO_SUBMITS.load(Relaxed), crate::fs::IO_DEPTH_MAX.load(Relaxed),
                    crate::fs::IO_QFULL.load(Relaxed));
                let ok = dirty >= nblocks && (r1 - r0) < (w1 - w0).max(1) && t_fs2 * 20 < t_fs.max(1)
                    && a_wr * 4 < nblocks as u64;
                println!("fsbench: {}", if ok { "ALL OK" } else { "FAILED" });
            } else { println!("fsbench: ext2 is not mounted"); }
            crate::suite::park()
        }
        "journal1" | "journalbig1" => {
            // §8.4 phase 1: create a file, do an fsync (the transaction is committed) and HANG
            // without doing a checkpoint — the metadata exist ONLY in the journal.
            // The harness kills QEMU here; phase 2 must recover the file.
            // §3.2: journalbig1 — the file is KNOWINGLY larger than the journal (1024 blocks in the image):
            // in the journal=data mode the transaction must be cut into parts.
            let nblk = if crate::cmdline::test_is(cmdline, "journalbig1") { 1500 } else { 8 };
            if crate::fs::init() && crate::ext2::active() {
                if !crate::ext2::jrn_active() { println!("journal1: there is no journal — the test is meaningless"); crate::suite::park() }
                let h = crate::ext2::open(b"/jtest.dat", 0o100 | 0o1 | 0o1000, 0o644);
                let mut buf = [0u8; 4096];
                for i in 0..4096 { buf[i] = b'J'; }
                for _ in 0..nblk { crate::ext2::write(h, buf.as_ptr(), buf.len()); }
                crate::ext2::fsync(h);
                let (jc, jb, dirty) = crate::ext2::jrn_stats();
                println!("journal1: the fsync has been done — the transactions {} the blocks {} checkpoint_pending={} (now we imitate a crash)", jc, jb, dirty);
                println!("journal1: COMMITTED");
            } else { println!("journal1: ext2 is not mounted"); }
            crate::suite::park()
        }
        "journal2" | "journalbig2" => {
            // §8.4 phase 2: the mounting must REPLAY the journal and return the file.
            let nblk: u64 = if crate::cmdline::test_is(cmdline, "journalbig2") { 1500 } else { 8 };
            if crate::fs::init() && crate::ext2::active() {
                let h = crate::ext2::open(b"/jtest.dat", 0, 0);
                let sz = if h >= 0 { crate::ext2::fd_size(h) } else { 0 };
                let mut buf = [0u8; 64];
                let n = crate::ext2::read_all(b"/jtest.dat", &mut buf);
                let ok = sz == nblk * 4096 && n > 0 && buf[0] == b'J' && buf[63] == b'J';
                if h >= 0 { crate::ext2::close(h); }
                println!("journal2: /jtest.dat the size {} the first bytes {} -> {}", sz, buf[0] as char,
                    if ok { "RECOVERED OK" } else { "FAILED" });
            } else { println!("journal2: ext2 is not mounted"); }
            crate::suite::park()
        }
        "htree" => {
            // §3.1: an indexed directory (dir_index) — the image is assembled by the host
            // (mkfs -O dir_index + an e2fsck -fD builds the htree), /big has 3000 files.
            // We check: (1) the search by the index finds ALL the names and does not find foreign ones,
            // (2) with talaria.ext2=dxcheck it coincides with the walk on every name,
            // (3) the creation of another 3000 files splits the leaves, and after that both
            // the old and the new ones are found. The integrity of the record is checked by e2fsck on the host.
            if crate::fs::init() && crate::ext2::active() {
                let mut ok = true;
                let mut name = [0u8; 32];
                // "/big/file_<i>.txt" without an alloc
                let mkname = |buf: &mut [u8; 32], pfx: &[u8], i: u32| -> usize {
                    let mut n = 0;
                    for &c in pfx { buf[n] = c; n += 1; }
                    let mut d = [0u8; 10]; let mut k = 0; let mut v = i;
                    if v == 0 { d[0] = b'0'; k = 1; }
                    while v > 0 { d[k] = b'0' + (v % 10) as u8; v /= 10; k += 1; }
                    while k > 0 { k -= 1; buf[n] = d[k]; n += 1; }
                    for &c in b".txt" { buf[n] = c; n += 1; }
                    buf[n] = 0; n
                };
                let (rd0, _) = crate::ext2::stats();
                let t0 = crate::tsc::rdtsc();
                let mut found = 0u32;
                for i in 1..=3000u32 {
                    let n = mkname(&mut name, b"/big/file_", i);
                    if crate::ext2::exists(&name[..n]) { found += 1; }
                }
                let t_look = crate::tsc::rdtsc() - t0;
                let (rd1, _) = crate::ext2::stats();
                if found != 3000 { println!("htree: FAIL {} names out of 3000 were found", found); ok = false; }
                let mut ghosts = 0u32;
                for i in 90000..90200u32 {
                    let n = mkname(&mut name, b"/big/file_", i);
                    if crate::ext2::exists(&name[..n]) { ghosts += 1; }
                }
                if ghosts != 0 { println!("htree: FAIL {} NON-existent names were found", ghosts); ok = false; }
                let (db, lb) = crate::ext2::dx_blocks();
                let hz = crate::tsc::calibrate_cached();
                println!("htree: 3000 searches in {} us; the directory's blocks looked through: by the index {}, by the walk {}; THE READS FROM THE DISK {}",
                    t_look * 1_000_000 / hz, db, lb, rd1 - rd0);
                // the directory's growth: 3000 new names (the leaves' splits). With
                // talaria.ext2=dxlimit=N a node "overflows" early — then we check
                // THE DEINDEXING PATH: the directory must stay working even after EXT2_INDEX_FL
                // is removed (further on it is linear).
                let lim = crate::ext2::dxlimit_set();
                let pfx: &[u8] = if lim { b"/big/lim_" } else { b"/big/new_" };
                let mut made = 0u32;
                for i in 1..=3000u32 {
                    let n = mkname(&mut name, pfx, i);
                    let h = crate::ext2::open(&name[..n], 0o100 | 0o1, 0o644);
                    if h >= 0 { crate::ext2::close(h); made += 1; }
                }
                if made != 3000 { println!("htree: FAIL {} out of 3000 were created", made); ok = false; }
                let mut back = 0u32;
                for i in 1..=3000u32 {
                    let n = mkname(&mut name, pfx, i);
                    if crate::ext2::exists(&name[..n]) { back += 1; }
                    let n = mkname(&mut name, b"/big/file_", i);
                    if crate::ext2::exists(&name[..n]) { back += 1; }
                }
                if back != 6000 { println!("htree: FAIL after the growth {} out of 6000 were found", back); ok = false; }
                // A deletion from an indexed directory: the entries leave their
                // leaves, the index stays valid, the other names are found.
                let mut gone = 0u32;
                for i in (1..=3000u32).step_by(6) {
                    let n = mkname(&mut name, b"/big/file_", i);
                    if crate::ext2::unlink(&name[..n], false) == 0 && !crate::ext2::exists(&name[..n]) { gone += 1; }
                }
                let mut left = 0u32;
                for i in (2..=3000u32).step_by(6) {
                    let n = mkname(&mut name, b"/big/file_", i);
                    if crate::ext2::exists(&name[..n]) { left += 1; }
                }
                println!("htree: {} names have been deleted, the neighbours by the hash are in place: {}", gone, left);
                if gone != 500 || left != 500 { println!("htree: FAIL a deletion from an indexed directory"); ok = false; }
                // A5: the index FROM SCRATCH. A fresh directory is linear; when it outgrows
                // the first block, the kernel must build an htree itself (previously an index
                // appeared only from foreign tools — an e2fsck -D).
                let mut fresh_ok = true;
                if crate::ext2::mkdir(b"/fresh", 0o755) == 0 {
                    let mut made = 0u32;
                    for i in 1..=600u32 {
                        let n = mkname(&mut name, b"/fresh/f_", i);
                        let h = crate::ext2::open(&name[..n], 0o100 | 0o1, 0o644);
                        if h >= 0 { crate::ext2::close(h); made += 1; }
                    }
                    let mut back = 0u32;
                    for i in 1..=600u32 {
                        let n = mkname(&mut name, b"/fresh/f_", i);
                        if crate::ext2::exists(&name[..n]) { back += 1; }
                    }
                    let depth = crate::ext2::dx_depth(b"/fresh");
                    println!("htree: the fresh directory /fresh: {} created, {} found, the index {}",
                        made, back, match depth { Some(d) => d as i32, None => -1 });
                    if made != 600 || back != 600 { fresh_ok = false; }
                    // depth == None means "there is no index" — while it must appear
                    if depth.is_none() { println!("htree: FAIL an index from scratch was not built"); fresh_ok = false; }
                } else { println!("htree: FAIL /fresh was not created"); fresh_ok = false; }
                if !fresh_ok { ok = false; }
                let (probes, splits, degr, noidx, miss) = crate::ext2::dx_stats();
                let (lv, nsp, made_ix) = crate::ext2::dx_growth();
                println!("htree: the descents {}, the leaves' splits {}, deindexed {}, the index-not-parsed ones {}, the divergences with the walk {}",
                    probes, splits, degr, noidx, miss);
                println!("htree: the tree's growth: the levels added {}, the node's splits {}, indexed from scratch {}, the depth of /big = {}",
                    lv, nsp, made_ix, match crate::ext2::dx_depth(b"/big") { Some(d) => d as i32, None => -1 });
                if miss != 0 { ok = false; }
                if splits == 0 { println!("htree: FAIL the directory's growth did not lead to a single split"); ok = false; }
                if made_ix == 0 { println!("htree: FAIL not a single directory has been indexed from scratch"); ok = false; }
                if lim {
                    // The node is squeezed artificially: the tree must EITHER grow deeper
                    // OR (when the root is full too) be deindexed — and in both
                    // cases the directory stays working. What exactly happened
                    // is decided by the value of dxlimit; test.sh checks that by the line above.
                    if lv == 0 && degr == 0 { println!("htree: FAIL with a dxlimit the tree neither grew nor was deindexed"); ok = false; }
                } else if degr != 0 || noidx != 0 {
                    println!("htree: FAIL the directory lost its index for no reason"); ok = false;
                }
                crate::ext2::flush_all();
                println!("htree: {}", if ok { "ALL OK" } else { "FAILED" });
            } else { println!("htree: ext2 is not mounted"); }
            crate::suite::park()
        }
        "ext2v" => {
            // The persistence of ext2 across a reboot: /t/hello.txt (the selftest) and /t/app.log (sigtest, O_APPEND,
            // it went to the disk at the exit_group from the write-back cache) must be readable in the next boot.
            if crate::fs::init() && crate::ext2::active() {
                let mut buf = [0u8; 64];
                let n = crate::ext2::read_all(b"/t/hello.txt", &mut buf);
                let ok1 = n == 24 && &buf[..24] == b"hello from talaria ext2\n";
                let h = crate::ext2::open(b"/t/app.log", 0, 0); let sz = if h >= 0 { crate::ext2::fd_size(h) } else { 0 }; if h >= 0 { crate::ext2::close(h); }
                println!("ext2v: hello.txt {} ({} bytes), app.log {} bytes -> {}", if ok1 { "ok" } else { "MISMATCH" }, n, sz, if ok1 && sz == 6 { "PERSISTED \
                    OK" } else { "FAILED" });
            } else { println!("ext2v: ext2 is not mounted"); }
            crate::suite::park()
        }
        "symlink" => {
            // The symbolic links of ext2: a read through a link, a link in the middle of a path,
            // readlink, stat/lstat, a creation by the kernel, an ELOOP.
            if crate::fs::init() && crate::ext2::active() {
                let ok = crate::ext2::symlink_selftest();
                println!("symlink-test: {}", if ok { "ALL OK" } else { "FAILED" });
            } else { println!("symlink-test: ext2 is not mounted"); }
            crate::suite::park()
        }
        "ext2" => {
            // An RW ext2 on virtio-blk: create/write/read/pread/ftruncate/rename/unlink/mkdir/rmdir/getdents64.
            if crate::fs::init() && crate::ext2::active() {
                let ok = crate::ext2::selftest();
                println!("ext2-test: {}", if ok { "ALL OK" } else { "FAILED" });
            } else { println!("ext2-test: ext2 is not mounted"); }
            crate::trace::mark("ext2-test finished");
            crate::suite::park()
        }
        "shell" => {
            // The operator's shell in the kernel: the parsing of the paths, ls/cat/write/cp/rm on top of
            // a live ext2 — what an operator corrects a node with without ring 3 and without busybox.
            if crate::fs::init() { crate::shell::selftest(); } else { println!("shell-test: the FS is not mounted"); }
            crate::suite::park()
        }
        "edit" => {
            // The console's editor: the loading, the edits, an undo, a search, a write and — most importantly —
            // A REFUSAL to write a file that did not fit into the buffer.
            if crate::fs::init() && crate::ext2::active() { crate::edit::selftest(); } else { println!("edit-test: ext2 is not mounted"); }
            crate::suite::park()
        }
        "fsbigrace" => {
            crate::smp::start_from_cmdline(cmdline);
            // A REPRODUCTION OF A CLOSED CLASS: four threads spin a "piece's" life
            // cycle — a directory of files, a rename, a deletion from under a read. It
            // used to be green on one core and broken on two or more, because the
            // syscall's path buffer was shared (see the gate's assert in `test.sh`:
            // "a syscall's path belongs to ITS OWN task"). It is in the regression and
            // it stays there: this is the shape that catches the class coming back.
            if crate::fs::init() { unsafe { crate::user::run_process_disk(b"/fsbig", &[b"fsbig", b"race"], &[b"HOME=/"]); } }
            crate::suite::park()
        }
        "devlockdiag" => {
            // The device's lock DOES NOT GIVE UP silently. Previously after a second of waiting it
            // returned an "empty" guard, and the write went without the lock (on gp3 a barrier is sometimes
            // 1.7-1.9 s — it fired under an honest load). The gate: the BSP holds
            // the lock for 1.5 s, an AP asks for it; ONE diagnosis into the console is expected with
            // the name of the holder core, a take after the release (not earlier) and zero
            // bypasses — a bypass is allowed only to a panic.
            use core::sync::atomic::{AtomicU64, Ordering::SeqCst};
            static START: AtomicU64 = AtomicU64::new(0);
            static GOT: AtomicU64 = AtomicU64::new(0);
            fn ap(_id: usize) -> ! {
                while START.load(SeqCst) == 0 { core::hint::spin_loop(); }
                let g = crate::fs::dev_lock();
                let held = g.held();
                GOT.store(crate::tsc::rdtsc() | 1, SeqCst);
                if !held { crate::println!("DEVLOCK: FAIL — the AP got a guard without the lock (a bypass)"); }
                drop(g);
                loop { unsafe { core::arch::asm!("hlt") }; }
            }
            if !crate::fs::init() { println!("DEVLOCK: FAIL — there is no disk"); crate::suite::park() }
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(3, SeqCst);
            crate::smp::AP_HOOK.store((ap as fn(usize) -> !) as usize as u64, SeqCst);
            if !crate::smp::start_cpu(1) { println!("DEVLOCK: FAIL — the AP did not come up (a -smp 2 is needed)"); crate::suite::park() }
            let hz = crate::tsc::hz().max(1);
            let hold = hz + hz / 2; // 1.5 s: a diagnosis after a second, but not a give-up
            let g = crate::fs::dev_lock();
            let t0 = crate::tsc::rdtsc();
            START.store(1, SeqCst);
            while crate::tsc::rdtsc().wrapping_sub(t0) < hold { crate::paging::tlb_service(); core::hint::spin_loop(); }
            let early = GOT.load(SeqCst) != 0; // it took it while we were holding — there is no lock
            let t_rel = crate::tsc::rdtsc();
            drop(g);
            let mut spins = 0u64;
            while GOT.load(SeqCst) == 0 { core::hint::spin_loop(); spins += 1; if spins > 20_000_000_000 { break; } }
            let got = GOT.load(SeqCst);
            let after_ms = got.wrapping_sub(t_rel) / (hz / 1000);
            let timeouts = crate::fs::DEV_TIMEOUT.load(SeqCst);
            let bypass = crate::fs::DEV_BYPASS.load(SeqCst);
            println!("DEVLOCK: we held it for {} ms, the AP took it {} ms after the release, the diagnoses {}, the bypasses {}, ahead of time: {}",
                hold / (hz / 1000), after_ms, timeouts, bypass, early);
            if got != 0 && !early && timeouts == 1 && bypass == 0 && after_ms < 100 { println!("DEVLOCK: OK"); }
            else { println!("DEVLOCK: FAIL"); }
            crate::suite::park()
        }
        "ringlockdiag" => {
            // The RING's lock (the submission into the virtqueue) is the same store as devlockdiag:
            // previously after a second of waiting a guard without the lock was returned, and its
            // Drop released A FOREIGN lock. The gate: the BSP holds the ring for 1.5 s, the AP does
            // a blk_read; a diagnosis after a second is expected, the read completes
            // AFTER the release and successfully, the bypasses are zero.
            use core::sync::atomic::{AtomicU64, Ordering::SeqCst};
            static START: AtomicU64 = AtomicU64::new(0);
            static GOT: AtomicU64 = AtomicU64::new(0);
            static OK: AtomicU64 = AtomicU64::new(0);
            fn ap(_id: usize) -> ! {
                while START.load(SeqCst) == 0 { core::hint::spin_loop(); }
                let mut buf = [0u8; 512];
                let ok = crate::fs::blk_read(0, &mut buf);
                OK.store(ok as u64, SeqCst);
                GOT.store(crate::tsc::rdtsc() | 1, SeqCst);
                loop { unsafe { core::arch::asm!("hlt") }; }
            }
            if !crate::fs::init() { println!("RINGLOCK: FAIL — there is no disk"); crate::suite::park() }
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(3, SeqCst);
            crate::smp::AP_HOOK.store((ap as fn(usize) -> !) as usize as u64, SeqCst);
            if !crate::smp::start_cpu(1) { println!("RINGLOCK: FAIL — the AP did not come up (a -smp 2 is needed)"); crate::suite::park() }
            let hz = crate::tsc::hz().max(1);
            let hold = hz + hz / 2;
            let g = crate::fs::ring_hold_for_test();
            let t0 = crate::tsc::rdtsc();
            START.store(1, SeqCst);
            while crate::tsc::rdtsc().wrapping_sub(t0) < hold { crate::paging::tlb_service(); core::hint::spin_loop(); }
            let early = GOT.load(SeqCst) != 0;
            let t_rel = crate::tsc::rdtsc();
            drop(g);
            let mut spins = 0u64;
            while GOT.load(SeqCst) == 0 { core::hint::spin_loop(); spins += 1; if spins > 20_000_000_000 { break; } }
            let got = GOT.load(SeqCst);
            let after_ms = got.wrapping_sub(t_rel) / (hz / 1000);
            let timeouts = crate::fs::RING_TIMEOUT.load(SeqCst);
            let bypass = crate::fs::RING_BYPASS.load(SeqCst);
            println!("RINGLOCK: we held it for {} ms, the AP's read completed {} ms after the release (the success {}), the diagnoses {}, the bypasses {}, \
                ahead of time: {}",
                hold / (hz / 1000), after_ms, OK.load(SeqCst) == 1, timeouts, bypass, early);
            if got != 0 && !early && OK.load(SeqCst) == 1 && timeouts == 1 && bypass == 0 && after_ms < 100 { println!("RINGLOCK: OK"); }
            else { println!("RINGLOCK: FAIL"); }
            crate::suite::park()
        }
        "netlockdiag" => {
            // The STACK's lock waits as long as necessary (falling asleep in the middle of a syscall is not allowed),
            // but not silently: a second of waiting is a defect, and the diagnosis must name
            // the holder while the machine is alive. The gate: the BSP holds the lock for 1.5 s through
            // `netsys::hold`, the AP asks for the same one; ONE diagnosis with the holder's core
            // and depth is expected and an entry AFTER the release.
            use core::sync::atomic::{AtomicU64, Ordering::SeqCst};
            static START: AtomicU64 = AtomicU64::new(0);
            static GOT: AtomicU64 = AtomicU64::new(0);
            fn ap(_id: usize) -> ! {
                while START.load(SeqCst) == 0 { core::hint::spin_loop(); }
                crate::netsys::hold(|| GOT.store(crate::tsc::rdtsc() | 1, SeqCst));
                loop { unsafe { core::arch::asm!("hlt") }; }
            }
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(3, SeqCst);
            crate::smp::AP_HOOK.store((ap as fn(usize) -> !) as usize as u64, SeqCst);
            if !crate::smp::start_cpu(1) { println!("NETLOCK: FAIL — the AP did not come up (a -smp 2 is needed)"); crate::suite::park() }
            let hz = crate::tsc::hz().max(1);
            let hold = hz + hz / 2;
            let (early, t_rel) = crate::netsys::hold(|| {
                let t0 = crate::tsc::rdtsc();
                START.store(1, SeqCst);
                while crate::tsc::rdtsc().wrapping_sub(t0) < hold { crate::paging::tlb_service(); core::hint::spin_loop(); }
                (GOT.load(SeqCst) != 0, crate::tsc::rdtsc())
            });
            let mut spins = 0u64;
            while GOT.load(SeqCst) == 0 { core::hint::spin_loop(); spins += 1; if spins > 20_000_000_000 { break; } }
            let got = GOT.load(SeqCst);
            let after_ms = got.wrapping_sub(t_rel) / (hz / 1000);
            let long = crate::netsys::NET_LONGWAIT.load(SeqCst);
            println!("NETLOCK: we held it for {} ms, the AP entered {} ms after the release, the diagnoses {}, ahead of time: {}",
                hold / (hz / 1000), after_ms, long, early);
            if got != 0 && !early && long == 1 && after_ms < 100 { println!("NETLOCK: OK"); } else { println!("NETLOCK: FAIL"); }
            crate::suite::park()
        }
        "ioerr" => {
            // A device refusal reaches fsync/sync as an EIO, the FS goes read-only.
            if !crate::fs::init() { println!("ioerr: FAIL — there is no disk"); crate::suite::park() }
            if !crate::ext2::active() { println!("ioerr: FAIL — an ext2 volume is needed (disk_ext2.img)"); crate::suite::park() }
            crate::ext2::ioerr_selftest();
            crate::suite::park()
        }
        "jrnwaitdiag" => {
            // The journal is busy with a foreign transaction for 1.5 s: an fsync/sync from an AP must
            // WAIT (a diagnosis after the threshold) rather than go bypassing the journal
            // (previously it was a give-up after ~1 s and an "it is written" without the metadata).
            //
            // SESSION 39: and the waiter must also say WHAT the holder stood on.
            // Session 38's instrument gave 238 us per call for this phase with THREE
            // events over 120 s — one stop drags on for hundreds of milliseconds, and
            // that is the mechanics of p99.9; "the journal was busy" does not become a change.
            // So the holder here CHANGES its phase halfway (first
            // "the descriptor+the copies", then "the commit"), and the gate requires that
            // the wait be broken down between the two roughly equally rather than fall
            // into one: a sampling that does not notice a change of phase
            // would show exactly one — and would lie convincingly.
            use core::sync::atomic::{AtomicU64, Ordering::SeqCst};
            static START: AtomicU64 = AtomicU64::new(0);
            static GOT: AtomicU64 = AtomicU64::new(0);
            static RC: AtomicU64 = AtomicU64::new(u64::MAX);
            static BYP: AtomicU64 = AtomicU64::new(u64::MAX);
            fn ap(_id: usize) -> ! {
                while START.load(SeqCst) == 0 { core::hint::spin_loop(); }
                let (rc, byp) = crate::ext2::jrn_sync_for_test();
                RC.store(rc as u64, SeqCst); BYP.store(byp, SeqCst);
                GOT.store(crate::tsc::rdtsc() | 1, SeqCst);
                loop { unsafe { core::arch::asm!("hlt") }; }
            }
            if !crate::fs::init() { println!("JRNWAIT: FAIL — there is no disk"); crate::suite::park() }
            if !crate::ext2::jrn_active() { println!("JRNWAIT: FAIL — a volume without a journal (disk_ext2.img is needed)"); crate::suite::park() }
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(3, SeqCst);
            crate::smp::AP_HOOK.store((ap as fn(usize) -> !) as usize as u64, SeqCst);
            if !crate::smp::start_cpu(1) { println!("JRNWAIT: FAIL — the AP did not come up (a -smp 2 is needed)"); crate::suite::park() }
            let hz = crate::tsc::hz().max(1);
            let hold = hz + hz / 2;
            let byp0 = crate::ext2::JRN_BYPASS.load(SeqCst);
            crate::ext2::jrn_busy_for_test(true);
            crate::ext2::jrn_stage_for_test(3);          // "the descriptor+the copies"
            let t0 = crate::tsc::rdtsc();
            START.store(1, SeqCst);
            let mut switched = false;
            while crate::tsc::rdtsc().wrapping_sub(t0) < hold {
                if !switched && crate::tsc::rdtsc().wrapping_sub(t0) > hold / 2 {
                    crate::ext2::jrn_stage_for_test(5);  // «commit»
                    switched = true;
                }
                crate::paging::tlb_service(); core::hint::spin_loop();
            }
            let early = GOT.load(SeqCst) != 0;
            let t_rel = crate::tsc::rdtsc();
            crate::ext2::jrn_busy_for_test(false);
            let mut spins = 0u64;
            while GOT.load(SeqCst) == 0 { core::hint::spin_loop(); spins += 1; if spins > 20_000_000_000 { break; } }
            let got = GOT.load(SeqCst);
            let after_ms = got.wrapping_sub(t_rel) / (hz / 1000);
            let long = crate::ext2::JRN_LONGWAIT.load(SeqCst);
            let byp = BYP.load(SeqCst).wrapping_sub(byp0);
            let rc = RC.load(SeqCst) as i64;
            println!("JRNWAIT: we held it for {} ms, the sync from the AP completed {} ms after the release (the code {}), the diagnoses {}, the journal's \
                bypasses {}, ahead of time: {}",
                hold / (hz / 1000), after_ms, rc, long, byp, early);
            // THE WAIT IS BROKEN DOWN BY THE HOLDER'S PHASE rather than dumped into one.
            let msd = (hz / 1000).max(1);
            let (n3, t3, _) = crate::ext2::jwait_stage(3);
            let (n5, t5, _) = crate::ext2::jwait_stage(5);
            println!("JRNWAIT: the wait by the holder's phases: \"{}\" {} episode(s) {} ms, \"{}\" {} episode(s) {} ms",
                crate::ext2::JSTAGE_NAMES[3], n3, t3 / msd, crate::ext2::JSTAGE_NAMES[5], n5, t5 / msd);
            let split_ok = n3 == 1 && n5 == 1
                && t3 / msd >= 500 && t3 / msd <= 1000
                && t5 / msd >= 500 && t5 / msd <= 1000;
            if got != 0 && !early && long == 1 && byp == 0 && rc == 0 && after_ms < 100 && split_ok { println!("JRNWAIT: OK"); }
            else { println!("JRNWAIT: FAIL (the breakdown by the phases {})", split_ok); }
            crate::suite::park()
        }
        "dirtyage" => {
            // STEP 1 OF SESSION 40 (the second half): THE FLUSH BY AGE.
            //
            // Session 39's instrument said that there IS SOMETHING to pre-empt: by the moment of
            // the flush the dirty data are 2.7-3.2 s old (the worst one 58 s), while the §8.7 threshold does not apply
            // to this path (45-50 blocks per call with a threshold of 32768).
            // The gate requires exactly what the knob was introduced for: a block that
            // has lain longer than the threshold must leave BEFORE the `fsync`, while the `fsync` itself
            // must find no dirty data at all. And the converse — with `dirtyage=0`
            // everything stays as it was (otherwise there is nothing to judge the A/B on the stand with).
            use core::sync::atomic::Ordering::SeqCst;
            if !crate::fs::init() { println!("DIRTYAGE: FAIL — there is no disk"); crate::suite::park() }
            let hz = crate::tsc::hz().max(1);
            let blk = [0x41u8; 4096];
            let h = crate::ext2::open(b"/dage.txt", 0o100 | 0o1000 | 2, 0o644);
            if h < 0 { println!("DIRTYAGE: FAIL — /dage.txt was not created"); crate::suite::park() }
            const AGE: usize = crate::ext2::WbC::Age as usize;
            const FS: usize = crate::ext2::WbC::Fsync as usize;
            let mut fsync_dirty = [0u64; 2];
            let mut age_blocks = [0u64; 2];
            for r in 0..2usize {
                // r=0 — the knob is OFF (the former behaviour), r=1 — 200 ms
                crate::ext2::DIRTY_AGE_MS.store(if r == 0 { 0 } else { 200 }, SeqCst);
                let (c0, d0, _) = crate::ext2::fsd_stats();
                let (_, ab0, as0, _) = crate::ext2::wbage_stats_of(AGE);
                let (_, fb0, _, _) = crate::ext2::wbage_stats_of(FS);
                let _ = crate::ext2::write(h, blk.as_ptr(), blk.len());
                // let it lie longer than the threshold
                let t0 = crate::tsc::rdtsc();
                while crate::tsc::rdtsc().wrapping_sub(t0) < hz * 4 / 10 { crate::paging::tlb_service(); core::hint::spin_loop(); }
                // THE NEXT WRITE — the one that dirties is the one that pays (there is no background thread)
                let _ = crate::ext2::write(h, blk.as_ptr(), blk.len());
                let (_, ab1, as1, _) = crate::ext2::wbage_stats_of(AGE);
                let _ = crate::ext2::fsync(h);
                let (c1, d1, _) = crate::ext2::fsd_stats();
                let (_, fb1, _, _) = crate::ext2::wbage_stats_of(FS);
                fsync_dirty[r] = d1 - d0;
                age_blocks[r] = ab1 - ab0;
                println!("DIRTYAGE: talaria.dirtyage={}: {} blocks were flushed by age (the average age is {} us), the fsync carried away {} dirty ones over \
                    {} call(s), its population +{}",
                    if r == 0 { 0 } else { 200 }, ab1 - ab0,
                    if ab1 > ab0 { (as1 - as0) / (ab1 - ab0) } else { 0 }, d1 - d0, c1 - c0, fb1 - fb0);
            }
            crate::ext2::DIRTY_AGE_MS.store(0, SeqCst);
            let _ = crate::ext2::close(h);
            let _ = crate::ext2::unlink(b"/dage.txt", false);
            // The knob that is off flushes NOTHING and the fsync carries its own blocks;
            // the one that is on carries a block that has lain long enough away BEFORE the call.
            let ok = age_blocks[0] == 0 && fsync_dirty[0] >= 1 && age_blocks[1] >= 1 && fsync_dirty[1] < fsync_dirty[0];
            if ok { println!("DIRTYAGE: OK"); }
            else { println!("DIRTYAGE: FAIL (off: by age {} / the fsync {}; on: by age {} / the fsync {})",
                age_blocks[0], fsync_dirty[0], age_blocks[1], fsync_dirty[1]); }
            crate::suite::park()
        }
        "agewrap" => {
            // SESSION 41: THE AGE'S INSTRUMENT LIED ON THE STAND, AND IT LIED WITH ARITHMETIC.
            //
            // The phase `talaria.dirtyage=10` (the run s41age-20260919) printed
            // "the average age 14 103 784 374 us, the worst 1 553 501 078 413"
            // — 18 days with a run of five minutes. The reason: the selection "older than
            // the threshold" computed an UNSIGNED difference of the stamps, while a block's stamp
            // is sometimes NEWER than the snapshot (the snapshot is taken at the entry into a pass, while the FS
            // lock is released by the pass for the time of the device — §8.6). "Minus 0.4 ms"
            // turned into 4 294 967 295 units, and the freshest block looked
            // infinitely old: the pre-emptive flush carried away exactly what it
            // must not, while the instrument hid that under the guise of "the age".
            //
            // The gate holds the arithmetic itself: a signed reading of the difference is correct even at
            // an overflow of the counter, while "a stamp from the future" is an age of 0 and
            // "it has not lain long enough" rather than 18 days.
            if crate::ext2::age_arith_check() { println!("AGEWRAP: OK"); }
            else { println!("AGEWRAP: FAIL — the age of the dirty data is not computed as a signed difference"); }
            crate::suite::park()
        }
        "dirtysmear" => {
            // STEP 1 OF SESSION 41: THE PRICE OF THE PRE-EMPTION MUST BE SMEARED rather than paid in a batch.
            //
            // Session 40's stand (`docs/MEASUREMENTS.md` §3) measured both sides of the knob
            // `dirtyage`: it does take the data off an `fsync` call (47 -> 9.4 blocks),
            // but `write` pays for that WITH A BATCH — 44 flushes of 136 blocks, and
            // the insert's `p99.9` is four times worse. The batch was a consequence of
            // the construction: the kick's period EQUALLED the age's threshold.
            //
            // The gate judges exactly that and on ONE run (session 39's rule —
            // an A/B with a knob): one and the same debt of 400 blocks that have lain long enough
            // is carried away twice — the old way (`dirtykick=<the threshold>`,
            // `dirtybatch=256`) and smeared (the defaults). Three things are required:
            // the smeared one's worst flush is NOT LARGER than the kick's ceiling, the debt left
            // ENTIRELY all the same (we smeared the price rather than threw the work away), and the clock
            // `DN_OLD` did not lie once — otherwise a block that has lain long enough will be noticed by
            // not a single kick.
            use core::sync::atomic::Ordering::SeqCst;
            if !crate::fs::init() { println!("DIRTYSMEAR: FAIL — there is no disk"); crate::suite::park() }
            let hz = crate::tsc::hz().max(1);
            let ms = |n: u64| { let t0 = crate::tsc::rdtsc();
                while crate::tsc::rdtsc().wrapping_sub(t0) < hz * n / 1000 { crate::paging::tlb_service(); core::hint::spin_loop(); } };
            const AGE: usize = 200;          // the age's threshold, ms
            const DEBT: usize = 400;         // the debt that has lain long enough, blocks
            const SPACED: usize = 60;        // the writes in the phase "the one that dirties is the one that pays"
            let blk = [0x41u8; 4096];
            let h = crate::ext2::open(b"/dsmear.txt", 0o100 | 0o1000 | 2, 0o644);
            if h < 0 { println!("DIRTYSMEAR: FAIL — /dsmear.txt was not created"); crate::suite::park() }
            let mut flushes = [0u64; 2]; let mut blocks = [0u64; 2];
            let mut worst = [0u64; 2]; let mut skips = [0u64; 2];
            let mut audit_ok = true;
            for r in 0..2usize {
                // The debt is accumulated with the knob OFF — both halves of the A/B
                // start from one and the same state of the cache.
                crate::ext2::DIRTY_AGE_MS.store(0, SeqCst);
                let _ = crate::ext2::lseek(h, 0, 0);
                for _ in 0..DEBT { let _ = crate::ext2::write(h, blk.as_ptr(), blk.len()); }
                ms(AGE as u64 * 3 / 2);                     // let it lie longer than the threshold
                if !crate::ext2::dn_old_audit().2 { audit_ok = false; }
                crate::ext2::AGE_MAXBLK.store(0, SeqCst);
                let (f0, b0, _, s0) = crate::ext2::age_stats();
                // r=0 — in a batch, as in session 40; r=1 — session 41's defaults
                crate::ext2::DIRTY_KICK_MS.store(if r == 0 { AGE } else { 0 }, SeqCst);
                crate::ext2::DIRTY_BATCH.store(if r == 0 { 256 } else { 0 }, SeqCst);
                crate::ext2::DIRTY_AGE_MS.store(AGE, SeqCst);
                // There is no background thread: the kick lives inside `write`, so the debt
                // is carried away by the very writes that dirty further on.
                for _ in 0..SPACED { let _ = crate::ext2::write(h, blk.as_ptr(), blk.len()); ms(AGE as u64 / 16); }
                let (f1, b1, mx, s1) = crate::ext2::age_stats();
                if !crate::ext2::dn_old_audit().2 { audit_ok = false; }
                flushes[r] = f1 - f0; blocks[r] = b1 - b0; worst[r] = mx; skips[r] = s1 - s0;
                crate::ext2::DIRTY_AGE_MS.store(0, SeqCst);
                let _ = crate::ext2::fsync(h);
                println!("DIRTYSMEAR: {}: the flushes {}, the blocks {}, THE WORST flush {} blocks, the kicks without a walk {}",
                    if r == 0 { "in a batch (the kick=the threshold, the ceiling 256)" } else { "smeared (the defaults)" },
                    flushes[r], blocks[r], worst[r], skips[r]);
            }
            // THE CLOCK AGAINST A WALK. The kick started going EIGHT times as often, and if it
            // still answered the question "is there anything that has lain long enough?"
            // by a walk over the whole cache, the smearing would buy the tail at the price of 165
            // us of pure processor (session 38's instrument) in somebody else's `write` once every
            // 12 ms. The phase of silence: everything dirty is YOUNGER than the threshold — the kick must
            // get by with two comparisons and carry nothing away.
            crate::ext2::DIRTY_AGE_MS.store(0, SeqCst);
            crate::ext2::DIRTY_KICK_MS.store(0, SeqCst);
            crate::ext2::DIRTY_BATCH.store(0, SeqCst);
            let _ = crate::ext2::fsync(h);
            crate::ext2::DIRTY_AGE_MS.store(AGE, SeqCst);
            let (qf0, _, _, qs0) = crate::ext2::age_stats();
            for _ in 0..5 { let _ = crate::ext2::write(h, blk.as_ptr(), blk.len()); ms(AGE as u64 / 8 + 5); }
            let (qf1, _, _, qs1) = crate::ext2::age_stats();
            let (quiet_flush, quiet_skip) = (qf1 - qf0, qs1 - qs0);
            if !crate::ext2::dn_old_audit().2 { audit_ok = false; }
            println!("DIRTYSMEAR: the silence (everything dirty is younger than the threshold): the kicks without a walk {}, the flushes {}",
                quiet_skip, quiet_flush);
            crate::ext2::DIRTY_AGE_MS.store(0, SeqCst);
            let _ = crate::ext2::close(h);
            let _ = crate::ext2::unlink(b"/dsmear.txt", false);
            // The ceiling of a smeared kick is automatic: AGE_CHUNKS * dirtychunk / 8.
            let cap = crate::ext2::age_batch_pub() as u64;
            let ok = worst[0] >= 128 && worst[1] <= cap
                && flushes[1] >= 8 && flushes[1] > flushes[0]
                && blocks[1] >= DEBT as u64 && blocks[0] >= DEBT as u64
                && quiet_skip >= 3 && quiet_flush == 0 && audit_ok;
            if ok { println!("DIRTYSMEAR: OK"); }
            else { println!("DIRTYSMEAR: FAIL (the worst {} against {} with the ceiling {}, the flushes {} against {}, the blocks {} against {}, the silence \
                {}/{}, the clock {})",
                worst[0], worst[1], cap, flushes[0], flushes[1], blocks[0], blocks[1], quiet_skip, quiet_flush, audit_ok); }
            crate::suite::park()
        }
        "jrnfreedata" => {
            // STEP 2 OF SESSION 40: THE JOURNAL MUST NOT BE BUSY ON SOMEBODY ELSE'S DATA.
            //
            // Session 39's instrument named the owner of the wait: 99 % of the wait for a foreign
            // transaction is the holder's phase "the data in place" (the worst episode
            // 257-385 ms). The change releases the journal for that time; the gate must
            // show THE DIFFERENCE rather than "it became faster": two rounds on ONE
            // run, with the knob `talaria.jrnfree`, as session 39's rule requires.
            //
            // The holder is the AP: its data phase is stretched by a single-use delay
            // (`JRN_STALL_MS`, with the FS lock RELEASED — in the same way as the device
            // stretches it). The BSP enters an fsync of ANOTHER file when the holder
            // is already inside the phase, and measures how long it stood.
            use core::sync::atomic::{AtomicU64, Ordering::SeqCst};
            static ROUND: AtomicU64 = AtomicU64::new(0);
            static DONE: AtomicU64 = AtomicU64::new(0);
            static HA: AtomicU64 = AtomicU64::new(0);
            fn ap(_id: usize) -> ! {
                let mut seen = 0u64;
                loop {
                    while ROUND.load(SeqCst) == seen { core::hint::spin_loop(); }
                    seen = ROUND.load(SeqCst);
                    let _ = crate::ext2::fsync(HA.load(SeqCst) as i64);
                    DONE.store(seen, SeqCst);
                }
            }
            if !crate::fs::init() { println!("JRNFREE: FAIL — there is no disk"); crate::suite::park() }
            if !crate::ext2::jrn_active() { println!("JRNFREE: FAIL — a volume without a journal"); crate::suite::park() }
            let hz = crate::tsc::hz().max(1);
            let ms = (hz / 1000).max(1);
            let blk = [0x40u8; 4096];
            let ha = crate::ext2::open(b"/jf-a.txt", 0o100 | 0o1000 | 2, 0o644);
            let hb = crate::ext2::open(b"/jf-b.txt", 0o100 | 0o1000 | 2, 0o644);
            if ha < 0 || hb < 0 { println!("JRNFREE: FAIL — the files were not created"); crate::suite::park() }
            HA.store(ha as u64, SeqCst);
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(3, SeqCst);
            crate::smp::AP_HOOK.store((ap as fn(usize) -> !) as usize as u64, SeqCst);
            if !crate::smp::start_cpu(1) { println!("JRNFREE: FAIL — the AP did not come up (a -smp 2 is needed)"); crate::suite::park() }
            let mut waited = [0u64; 2];
            let mut eps = [0u64; 2];
            for r in 0..2u64 {
                crate::ext2::JRNFREE.store(r == 1, SeqCst);
                // dirty data for both files — otherwise there will be no data phase at all
                let _ = crate::ext2::write(ha, blk.as_ptr(), blk.len());
                let _ = crate::ext2::write(hb, blk.as_ptr(), blk.len());
                let (n0, _, _) = crate::ext2::jwait_stage(1);
                let hits0 = crate::ext2::JRN_STALL_HITS.load(SeqCst);
                crate::ext2::JRN_STALL_MS.store(600, SeqCst);
                ROUND.store(r + 1, SeqCst);
                // wait until the holder ENTERS the data phase (otherwise we would measure
                // the race of the AP's start rather than the wait for the journal)
                let tw = crate::tsc::rdtsc();
                while crate::ext2::JRN_STALL_HITS.load(SeqCst) == hits0 {
                    crate::paging::tlb_service(); core::hint::spin_loop();
                    if crate::tsc::rdtsc().wrapping_sub(tw) > hz * 5 { break; }
                }
                let t0 = crate::tsc::rdtsc();
                let _ = crate::ext2::fsync(hb);
                waited[r as usize] = crate::tsc::rdtsc().wrapping_sub(t0) / ms;
                let (n1, _, _) = crate::ext2::jwait_stage(1);
                eps[r as usize] = n1 - n0;
                let td = crate::tsc::rdtsc();
                while DONE.load(SeqCst) != r + 1 {
                    crate::paging::tlb_service(); core::hint::spin_loop();
                    if crate::tsc::rdtsc().wrapping_sub(td) > hz * 10 { break; }
                }
            }
            crate::ext2::JRNFREE.store(true, SeqCst);
            println!("JRNFREE: the holder stretches the data phase for 600 ms; the neighbour: with talaria.jrnfree=0 it waited {} ms (the episodes of \"{}\" \
                {}), with jrnfree=1 — {} ms (the episodes {})",
                waited[0], crate::ext2::JSTAGE_NAMES[1], eps[0], waited[1], eps[1]);
            let _ = crate::ext2::close(ha); let _ = crate::ext2::close(hb);
            let _ = crate::ext2::unlink(b"/jf-a.txt", false);
            let _ = crate::ext2::unlink(b"/jf-b.txt", false);
            // The instrument must show BOTH: that the former behaviour cost the neighbour
            // hundreds of milliseconds (otherwise the gate proves nothing), and that the new one
            // removed them.
            let ok = waited[0] >= 300 && eps[0] >= 1 && waited[1] <= 150 && eps[1] == 0;
            if ok { println!("JRNFREE: OK"); }
            else { println!("JRNFREE: FAIL (the former one {} ms/{} ep., the new one {} ms/{} ep.)", waited[0], eps[0], waited[1], eps[1]); }
            crate::suite::park()
        }
        "fsyncph" => {
            // THE fsync PHASES MUST NAME THE TIME'S OWNER rather than hide it in the remainder.
            //
            // Session 36 measured `fsync` under mongodb/insert: 839 calls of 5173
            // us with a sum of the phases of 3939 — 1.2 ms per call belonged to NOT A SINGLE
            // phase, and any change of the write path would have been guesswork. An instrument that
            // does not cover what it measures has already let us down three times (sessions 26 and 36).
            // So here the wait for A FOREIGN transaction is created deliberately: the AP
            // holds the journal while the BSP does an fsync meanwhile. The gate requires that
            // this time be named by THE PHASE "a foreign transaction", while "THE OTHER" stays
            // small — that is, that the question "where did the milliseconds go" have an answer.
            use core::sync::atomic::{AtomicU64, Ordering::SeqCst};
            static HOLD: AtomicU64 = AtomicU64::new(0);
            static DONE: AtomicU64 = AtomicU64::new(0);
            fn ap(_id: usize) -> ! {
                while HOLD.load(SeqCst) == 0 { core::hint::spin_loop(); }
                let hz = crate::tsc::hz().max(1);
                crate::ext2::jrn_busy_for_test(true);
                let t0 = crate::tsc::rdtsc();
                while crate::tsc::rdtsc().wrapping_sub(t0) < hz / 3 { core::hint::spin_loop(); } // 333 ms
                crate::ext2::jrn_busy_for_test(false);
                DONE.store(1, SeqCst);
                loop { unsafe { core::arch::asm!("hlt") }; }
            }
            if !crate::fs::init() { println!("FSYNCPH: FAIL — there is no disk"); crate::suite::park() }
            if !crate::ext2::jrn_active() { println!("FSYNCPH: FAIL — a volume without a journal (disk_ext2.img is needed)"); crate::suite::park() }
            let hz = crate::tsc::hz().max(1);
            let us = |c: u64| c.saturating_mul(1_000_000) / hz;
            let h = crate::ext2::open(b"/fsyncph.txt", 0o100 | 0o1000 | 2, 0o644) /* O_CREAT|O_TRUNC|O_RDWR */;
            if h < 0 { println!("FSYNCPH: FAIL — /fsyncph.txt was not created"); crate::suite::park() }
            // 1) AN ORDINARY fsync WITHOUT COMPETITORS: how much of its time stayed
            //    uncovered. Here there is nobody to wait for — all the time must lie by
            //    the write's phases, and the remainder speaks about the marking itself.
            let _ = crate::ext2::write(h, b"phase".as_ptr(), 5);
            let _ = crate::ext2::fsync(h);              // the warm-up: the first call drags the file's creation along
            let (n0, t0) = crate::ext2::ph_get(0);
            let (r0, rt0) = crate::ext2::ph_get(12);
            // Session 38: the details of the phases "the data" and "THE OTHER" (the indices from PH_SUB).
            // We take them around the same quiet call: the marking must lie
            // INSIDE its parent rather than next to it.
            let mut sub0 = [(0u64, 0u64); 22];
            for i in crate::ext2::PH_SUB..22 { sub0[i] = crate::ext2::ph_get(i); }
            let _ = crate::ext2::write(h, b"phase".as_ptr(), 5);
            let rc = crate::ext2::fsync(h);
            let (n1, t1) = crate::ext2::ph_get(0);
            let (r1, rt1) = crate::ext2::ph_get(12);
            let whole = t1 - t0; let rest = rt1 - rt0;
            let calls = n1 - n0;
            println!("FSYNCPH: a quiet fsync: the calls {} (the remainder was computed {} times), in total {} us, THE OTHER {} us ({} %)",
                calls, r1 - r0, us(whole), us(rest), if whole > 0 { rest * 100 / whole } else { 0 });
            let mut wb_n = 0u64; let mut wb_t = 0u64;   // the flush's details (13-17)
            let mut ot_n = 0u64; let mut ot_t = 0u64;   // THE OTHER's details (18-21)
            for i in crate::ext2::PH_SUB..22 {
                let (n, t) = crate::ext2::ph_get(i);
                let (dn, dt) = (n - sub0[i].0, t - sub0[i].1);
                println!("FSYNCPH:   {} — {} times, {} us", crate::ext2::FSYNC_PH_NAMES[i], dn, us(dt));
                if i < 18 { wb_n += dn; wb_t += dt; } else { ot_n += dn; ot_t += dt; }
            }
            // A DETAIL MUST LIE INSIDE ITS PARENT. THE OTHER's items
            // (mmap_harvest, prea_release, the estimation of the capacity, jrn_pending) go
            // BETWEEN the phases — their sum cannot be larger than the remainder itself, while
            // if it is larger, then the marking got inside an already covered phase and
            // the instrument lies again (sessions 26, 36 — the same class of defect).
            let sub_ok = wb_n > 0 && ot_n >= 4 && ot_t <= rest && wb_t <= whole;
            let quiet_ok = rc == 0 && calls == 1 && r1 - r0 == 1 && whole > 0 && rest * 2 <= whole && sub_ok;
            // 2) THE JOURNAL IS BUSY WITH A NEIGHBOUR: the wait must fall into its own phase.
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(3, SeqCst);
            crate::smp::AP_HOOK.store((ap as fn(usize) -> !) as usize as u64, SeqCst);
            if !crate::smp::start_cpu(1) { println!("FSYNCPH: FAIL — the AP did not come up (a -smp 2 is needed)"); crate::suite::park() }
            let (w0, wt0) = crate::ext2::ph_get(8);
            let (_, rt2) = crate::ext2::ph_get(12);
            let (_, t2) = crate::ext2::ph_get(0);
            HOLD.store(1, SeqCst);
            let mut spins = 0u64;
            while !crate::ext2::jrn_busy_now() { core::hint::spin_loop(); spins += 1; if spins > 20_000_000_000 { break; } }
            let _ = crate::ext2::write(h, b"wait!".as_ptr(), 5);
            let rc2 = crate::ext2::fsync(h);
            let (w1, wt1) = crate::ext2::ph_get(8);
            let (_, rt3) = crate::ext2::ph_get(12);
            let (_, t3) = crate::ext2::ph_get(0);
            let waited = wt1 - wt0; let whole2 = t3 - t2; let rest2 = rt3 - rt2;
            println!("FSYNCPH: under a foreign transaction: the fsync {} us, the phase \"a foreign transaction\" {} us ({} times), THE OTHER {} us",
                us(whole2), us(waited), w1 - w0, us(rest2));
            // The neighbour holds the journal for 333 ms: the wait must be named (>=250
            // ms in its own phase), while THE OTHER must not grow by the same time (<50 ms).
            let busy_ok = rc2 == 0 && w1 - w0 == 1 && us(waited) >= 250_000 && us(rest2) < 50_000;
            let _ = crate::ext2::close(h);
            let _ = crate::ext2::unlink(b"/fsyncph.txt", false);
            if quiet_ok && busy_ok { println!("FSYNCPH: OK"); } else { println!("FSYNCPH: FAIL (the quiet one {}, under a neighbour {}, the details {}: the \
                flush {} times/{} us, the other {} times/{} us with a remainder of {} us)",
                quiet_ok, busy_ok, sub_ok, wb_n, us(wb_t), ot_n, us(ot_t), us(rest)); }
            crate::suite::park()
        }
        "dcnt" => {
            // THE DIRTY ONES ARE COUNTED BY A COUNTER — AND THE COUNTER MUST AGREE WITH A RECOMPUTATION.
            //
            // Session 39 took 320 us off an `fsync` call (10 % of its price, the instrument
            // `docs/MEASUREMENTS.md` §3) of PURE processor: `jrn_pending` no longer
            // walks the whole cache (131072 slots with fscache=512), while the second
            // pass of `writeback_locked` is called only when it has something
            // to write. Both gains rest on four counters that depend
            // on FOUR arrays at once (CTAG/CDIRTY/COWN/CJRN) — and the price of
            // a missed wrapper `dcnt_del`/`dcnt_add` is not "slower" but
            // A LOST DIRTY BLOCK with a successful fsync code: exactly that class
            // of silent lie because of which the FS and device locks were
            // rewritten. So the gate checks three things:
            //   1) the counters against a full recomputation at all the turns of a slot's
            //      life (a write, an fsync, an eviction, an ftruncate, an unlink, a sync);
            //   2) at an fsync of THE DATA the second pass over the cache is not called at all — and
            //      the dirty metadata meanwhile ARE NOT LOST (they are taken by a sync,
            //      after which not a single dirty slot is left in the cache);
            //   3) the price of `jrn_pending` does not grow with the cache's size (8 -> 128 MB):
            //      that is exactly what "a counter rather than a walk" means, and on QEMU with a cache of
            //      2 MB there is no other way to tell one from the other.
            use core::sync::atomic::Ordering::SeqCst;
            fn audit(label: &str, bad: &mut usize) -> [(usize, usize); 4] {
                let a = crate::ext2::dcnt_audit();
                for i in 0..4 {
                    if a[i].0 != a[i].1 {
                        *bad += 1;
                        crate::println!("DCNT: A DIVERGENCE after \"{}\": {} — the counter {}, the recomputation {}",
                            label, crate::ext2::DCNT_NAMES[i], a[i].0, a[i].1);
                    }
                }
                a
            }
            if !crate::fs::init() { println!("DCNT: FAIL — there is no disk"); crate::suite::park() }
            if !crate::ext2::jrn_active() { println!("DCNT: FAIL — a volume without a journal (disk_ext2.img is needed)"); crate::suite::park() }
            let hz = crate::tsc::hz().max(1);
            let us = |c: u64| c.saturating_mul(1_000_000) / hz;
            let mut bad = 0usize;
            let blk = [0x5au8; 4096];
            let h = crate::ext2::open(b"/dcnt.txt", 0o100 | 0o1000 | 2, 0o644) /* O_CREAT|O_TRUNC|O_RDWR */;
            if h < 0 { println!("DCNT: FAIL — /dcnt.txt was not created"); crate::suite::park() }
            audit("the file's creation", &mut bad);
            for _ in 0..16 { let _ = crate::ext2::write(h, blk.as_ptr(), blk.len()); }
            let a_wr = audit("16 blocks of writing", &mut bad);
            // The dirty ones must be VISIBLE: otherwise the gate would agree on zeroes and would
            // check nothing (this error of the instrument has already cost session 36).
            let seen = a_wr[0].0 >= 16 && a_wr[1].0 >= 16 && a_wr[3].0 >= 16;
            println!("DCNT: after the write: the dirty ones {}, of them data {}, waiting for the journal are {} metadata and {} data",
                a_wr[0].0, a_wr[1].0, a_wr[2].0, a_wr[3].0);
            // 2) an fsync of THE DATA: there is no second pass over the cache, nobody waits for the journal,
            //    while the dirty metadata stayed (they went into the journal as copies).
            let (p17n0, _) = crate::ext2::ph_get(17);
            let rc = crate::ext2::fsync(h);
            let (p17n1, _) = crate::ext2::ph_get(17);
            let a_fs = audit("fsync", &mut bad);
            let meta_left = a_fs[0].0 - a_fs[1].0;
            let pass2 = p17n1 - p17n0;
            println!("DCNT: the fsync (the code {}): the dirty ones {} (the data {}, the metadata {}), waiting for the journal are {}+{}, the second pass over \
                the cache {} times",
                rc, a_fs[0].0, a_fs[1].0, meta_left, a_fs[2].0, a_fs[3].0, pass2);
            let fsync_ok = rc == 0 && pass2 == 0 && a_fs[2].0 == 0 && a_fs[3].0 == 0 && meta_left > 0;
            let rcs = crate::ext2::sync_all();
            let a_sy = audit("sync", &mut bad);
            println!("DCNT: the sync (the code {}): the dirty ones {} — the metadata skipped by the second pass have not been lost", rcs, a_sy[0].0);
            let sync_ok = rcs == 0 && a_sy[0].0 == 0;
            // 3) the other turns of a slot's life
            for _ in 0..64 { let _ = crate::ext2::write(h, blk.as_ptr(), blk.len()); }
            audit("another 64 blocks (an eviction)", &mut bad);
            let _ = crate::ext2::ftruncate(h, 8192);
            audit("ftruncate", &mut bad);
            let _ = crate::ext2::fsync(h);
            audit("an fsync after an ftruncate", &mut bad);
            let _ = crate::ext2::close(h);
            let _ = crate::ext2::unlink(b"/dcnt.txt", false);
            audit("unlink", &mut bad);
            let _ = crate::ext2::sync_all();
            let a_end = audit("a sync at the end", &mut bad);
            // 4) THE PRICE OF `jrn_pending` AGAINST THE CACHE'S SIZE
            let mut cost = [0u64; 2]; let mut calls = [0u64; 2]; let mut mbs = [0usize; 2];
            let mut sized = true;
            for k in 0..2 {
                let mb = if k == 0 { 8usize } else { 128usize };
                if !crate::ext2::cache_resize(mb) { sized = false; break; }
                mbs[k] = crate::ext2::cache_mb();
                let h2 = crate::ext2::open(b"/dcnt2.txt", 0o100 | 0o1000 | 2, 0o644);
                if h2 < 0 { sized = false; break; }
                let (n0, t0) = crate::ext2::ph_get(21);
                for _ in 0..50 { let _ = crate::ext2::write(h2, blk.as_ptr(), 512); let _ = crate::ext2::fsync(h2); }
                let (n1, t1) = crate::ext2::ph_get(21);
                calls[k] = n1 - n0; cost[k] = t1 - t0;
                let _ = crate::ext2::close(h2);
                let _ = crate::ext2::unlink(b"/dcnt2.txt", false);
                let _ = crate::ext2::sync_all();
            }
            let percall = |i: usize| if calls[i] > 0 { cost[i] / calls[i] } else { 0 };
            let (c_small, c_big) = (percall(0), percall(1));
            println!("DCNT: the counting of the dirty ones with a cache of {} MB — {} cycles per call ({} calls, {} us in total); with {} MB — {} cycles ({} \
                calls, {} us)",
                mbs[0], c_small, calls[0], us(cost[0]), mbs[1], c_big, calls[1], us(cost[1]));
            // The cache grew sixteenfold. A walk would grow together with it;
            // a counter does not grow — a tolerance of three times plus a noise of 200 cycles.
            let size_ok = sized && calls[0] > 0 && calls[1] > 0 && c_big <= c_small * 3 + 200;
            // 5) AN A/B RIGHT HERE: `talaria.fscnt=0` returns the former walk.
            //    The knob is needed for a measurement on the stand (one run, before and after), and
            //    it must be HONEST: if under it the price is the same, then either
            //    the knob does not work or there was no saving.
            let mut c_off = 0u64;
            if sized {
                crate::ext2::FSCNT.store(false, SeqCst);
                let h3 = crate::ext2::open(b"/dcnt3.txt", 0o100 | 0o1000 | 2, 0o644);
                if h3 >= 0 {
                    let (n0, t0) = crate::ext2::ph_get(21);
                    for _ in 0..50 { let _ = crate::ext2::write(h3, blk.as_ptr(), 512); let _ = crate::ext2::fsync(h3); }
                    let (n1, t1) = crate::ext2::ph_get(21);
                    if n1 > n0 { c_off = (t1 - t0) / (n1 - n0); }
                    let _ = crate::ext2::close(h3);
                    let _ = crate::ext2::unlink(b"/dcnt3.txt", false);
                }
                crate::ext2::FSCNT.store(true, SeqCst);
                let _ = crate::ext2::sync_all();
            }
            println!("DCNT: the same counting with talaria.fscnt=0 (a walk over the cache of {} MB) — {} cycles per call, that is, the counter is cheaper by a \
                factor of {}",
                mbs[1], c_off, if c_big > 0 { c_off / c_big } else { 0 });
            let ab_ok = !sized || (c_off > 0 && c_off >= c_big * 10);
            let ok = bad == 0 && seen && fsync_ok && sync_ok && a_end[0].0 == 0 && size_ok && ab_ok;
            if ok { println!("DCNT: OK"); }
            else { println!("DCNT: FAIL (the divergences {}, the dirty ones are visible {}, the fsync {}, the sync {}, it is clean at the end {}, the price \
                does not grow {}, the knob is honest {}, the cache was reallocated {})",
                bad, seen, fsync_ok, sync_ok, a_end[0].0 == 0, size_ok, ab_ok, sized); }
            crate::suite::park()
        }
        "wbage" => {
            // THE INSTRUMENT OF STEP 2 OF SESSION 39 MUST MEASURE THE AGE rather than print a zero.
            //
            // The candidate "a pre-emptive flush by the AGE of the dirty data" (like
            // Linux's `dirty_expire_centisecs`) exists exactly as long as
            // the age of our dirty data has not been measured: if a block is given to
            // the device within single milliseconds after it was
            // dirtied, then Linux's background writeback (a kick once every 5 s) would not have managed
            // to carry it away either — and the explanation "with Linux the dirty data left
            // in advance" is wrong together with the knob. The rule "the instrument before the change"
            // paid off twice in a row (sessions 37 and 38), but an instrument that
            // returns a zero by construction is worse than a missing one: it gives
            // the answer "the age is small" to any question. The gate separates two cases —
            // a flush right after a write and a flush after 300 ms — and requires the
            // instrument to TELL them apart.
            if !crate::fs::init() { println!("WBAGE: FAIL — there is no disk"); crate::suite::park() }
            let hz = crate::tsc::hz().max(1);
            let blk = [0x37u8; 4096];
            let h = crate::ext2::open(b"/wbage.txt", 0o100 | 0o1000 | 2, 0o644) /* O_CREAT|O_TRUNC|O_RDWR */;
            if h < 0 { println!("WBAGE: FAIL — /wbage.txt was not created"); crate::suite::park() }
            // STEP 1 OF SESSION 40: AND SEPARATE THE POPULATIONS. The age "2.7-3.2 s"
            // is averaged over everyone who carries the dirty data away (the `fsync` itself, the §8.7 threshold from
            // `write`, a slot's eviction, sync/checkpoint), while the knob "the flush by
            // age" exists only for the `fsync` population. The gate requires
            // the instrument to attribute a block TO THE ONE that carried it away: a block that has lain
            // 300 ms and was taken by the fsync must land in the `fsync` population,
            // while a block taken by the threshold from `write` must land in the threshold's population,
            // and the first must not change from the second.
            const FS: usize = crate::ext2::WbC::Fsync as usize;
            const BAL: usize = crate::ext2::WbC::Balance as usize;
            let (c0, d0, f0) = crate::ext2::fsd_stats();
            // 1) A FLUSH AT ONCE: the age must be small
            for _ in 0..3 { let _ = crate::ext2::write(h, blk.as_ptr(), blk.len()); let _ = crate::ext2::fsync(h); }
            let (an1, ab1, as1, am1) = crate::ext2::wbage_stats_of(FS);
            let (c1, d1, f1) = crate::ext2::fsd_stats();
            println!("WBAGE: a flush at once: the portions {}, the blocks {}, the average age {} us, the worst {} us; the fsync calls {} (the dirty blocks {}, \
                the file has {})",
                an1, ab1, if ab1 > 0 { as1 / ab1 } else { 0 }, am1, c1 - c0, d1 - d0, f1 - f0);
            let fresh_ok = ab1 >= 3 && am1 < 50_000 && c1 - c0 == 3 && d1 - d0 == 3 && f1 - f0 >= 3;
            // 2) A FLUSH AFTER 300 ms: the age must see that
            let _ = crate::ext2::write(h, blk.as_ptr(), blk.len());
            let t0 = crate::tsc::rdtsc();
            while crate::tsc::rdtsc().wrapping_sub(t0) < hz * 3 / 10 { crate::paging::tlb_service(); core::hint::spin_loop(); }
            let _ = crate::ext2::fsync(h);
            let (an2, ab2, as2, am2) = crate::ext2::wbage_stats_of(FS);
            let waited = if ab2 > ab1 { (as2 - as1) / (ab2 - ab1) } else { 0 };
            println!("WBAGE: a flush after 300 ms: the portions {}, the blocks {}, this portion's age {} us, the worst over the run {} us",
                an2 - an1, ab2 - ab1, waited, am2);
            let aged_ok = ab2 > ab1 && (250_000..=600_000).contains(&waited) && am2 >= 250_000;
            // 3) THE SAME, BUT IT IS CARRIED AWAY BY THE THRESHOLD FROM `write` (§8.7). The threshold is lowered
            // with a knob down to "any dirty data" — the flush is done by THE NEXT write rather than by
            // the fsync, and the instrument must put the age into ANOTHER population.
            use core::sync::atomic::Ordering::SeqCst;
            let (bn0, bb0, bs0, _bm0) = crate::ext2::wbage_stats_of(BAL);
            let frac0 = crate::ext2::DIRTY_FRAC.load(SeqCst);
            crate::ext2::DIRTY_FRAC.store(1 << 20, SeqCst);
            let _ = crate::ext2::write(h, blk.as_ptr(), blk.len());
            let t1 = crate::tsc::rdtsc();
            while crate::tsc::rdtsc().wrapping_sub(t1) < hz * 3 / 10 { crate::paging::tlb_service(); core::hint::spin_loop(); }
            let _ = crate::ext2::write(h, blk.as_ptr(), blk.len());   // and there it is, the threshold
            crate::ext2::DIRTY_FRAC.store(frac0, SeqCst);
            let (bn1, bb1, bs1, bm1) = crate::ext2::wbage_stats_of(BAL);
            let (an3, ab3, _as3, _am3) = crate::ext2::wbage_stats_of(FS);
            let bwait = if bb1 > bb0 { (bs1 - bs0) / (bb1 - bb0) } else { 0 };
            println!("WBAGE: a flush by the write's threshold: the portions {}, the blocks {}, this portion's age {} us, the worst {} us; the fsync population \
                is untouched: the portions +{}, the blocks +{}",
                bn1 - bn0, bb1 - bb0, bwait, bm1, an3 - an2, ab3 - ab2);
            let bal_ok = bb1 > bb0 && (250_000..=600_000).contains(&bwait) && an3 == an2 && ab3 == ab2;
            let _ = crate::ext2::fsync(h);
            let _ = crate::ext2::close(h);
            let _ = crate::ext2::unlink(b"/wbage.txt", false);
            if fresh_ok && aged_ok && bal_ok { println!("WBAGE: OK"); }
            else { println!("WBAGE: FAIL (the fresh one {}, the one that lay long enough {}, the threshold's population {})", fresh_ok, aged_ok, bal_ok); }
            crate::suite::park()
        }
        "fslockdiag" => {
            // The same for the FS lock: the BSP holds an `fs_lock` for 1.5 s, an AP asks — one
            // diagnosis, a take after the release, the bypasses are zero.
            use core::sync::atomic::{AtomicU64, Ordering::SeqCst};
            static START: AtomicU64 = AtomicU64::new(0);
            static GOT: AtomicU64 = AtomicU64::new(0);
            fn ap(_id: usize) -> ! {
                while START.load(SeqCst) == 0 { core::hint::spin_loop(); }
                let g = crate::ext2::fs_lock();
                let held = g.held();
                GOT.store(crate::tsc::rdtsc() | 1, SeqCst);
                if !held { crate::println!("FSLOCK: FAIL — the AP got a guard without the lock (a bypass)"); }
                drop(g);
                loop { unsafe { core::arch::asm!("hlt") }; }
            }
            if !crate::fs::init() { println!("FSLOCK: FAIL — there is no disk"); crate::suite::park() }
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(3, SeqCst);
            crate::smp::AP_HOOK.store((ap as fn(usize) -> !) as usize as u64, SeqCst);
            if !crate::smp::start_cpu(1) { println!("FSLOCK: FAIL — the AP did not come up (a -smp 2 is needed)"); crate::suite::park() }
            let hz = crate::tsc::hz().max(1);
            let hold = hz + hz / 2;
            let g = crate::ext2::fs_lock();
            let t0 = crate::tsc::rdtsc();
            START.store(1, SeqCst);
            while crate::tsc::rdtsc().wrapping_sub(t0) < hold { crate::paging::tlb_service(); core::hint::spin_loop(); }
            let early = GOT.load(SeqCst) != 0;
            let t_rel = crate::tsc::rdtsc();
            drop(g);
            let mut spins = 0u64;
            while GOT.load(SeqCst) == 0 { core::hint::spin_loop(); spins += 1; if spins > 20_000_000_000 { break; } }
            let got = GOT.load(SeqCst);
            let after_ms = got.wrapping_sub(t_rel) / (hz / 1000);
            let timeouts = crate::ext2::FS_LOCK_TIMEOUT.load(SeqCst);
            let bypass = crate::ext2::FS_LOCK_BYPASS.load(SeqCst);
            println!("FSLOCK: we held it for {} ms, the AP took it {} ms after the release, the diagnoses {}, the bypasses {}, ahead of time: {}",
                hold / (hz / 1000), after_ms, timeouts, bypass, early);
            if got != 0 && !early && timeouts == 1 && bypass == 0 && after_ms < 100 { println!("FSLOCK: OK"); }
            else { println!("FSLOCK: FAIL"); }
            crate::suite::park()
        }
        "fsyncconc" => {
            crate::smp::start_from_cmdline(cmdline);
            // §8.6-8.7: the flush of the dirty blocks goes WITHOUT the FS lock. A writer drives to
            // the disk twice as much as the cache (the evictions + an fsync), an overwrite pulls
            // the blocks in flight, while a neighbour measures every one of its cheap operations: not
            // a single stop longer than 20 ms. Before the change the lock was held through the disk —
            // MongoDB on EC2 stalled for a second once a minute.
            // The summary per task is printed at the workload's exit (`suite::park`):
            // the gate rqwait judges "it yielded to the device" separately from "it waited".
            crate::metrics::WAITERS_AT_EXIT.store(true, core::sync::atomic::Ordering::Relaxed);
            if crate::fs::init() { unsafe { crate::user::run_process_disk(b"/fsyncconc", &[b"fsyncconc"], &[b"HOME=/"]); } }
            crate::suite::park()
        }
        "fsbig" => {
            crate::smp::start_from_cmdline(cmdline);
            // The file is LARGER than the block cache: it is there, where the eviction of dirty pages goes
            // constantly, that the FS broke — while all the previous gates worked with files
            // that fit into the cache entirely.
            if crate::fs::init() { unsafe { crate::user::run_process_disk(b"/fsbig", &[b"fsbig"], &[b"HOME=/"]); } }
            crate::suite::park()
        }
        "evictbatch" => {
            crate::smp::start_from_cmdline(cmdline);
            // §8.14: THE EVICTION OF A DIRTY SLOT IN A BATCH. The gate takes the same workload
            // as `fsbig` (a file four times larger than the cache — there the eviction goes
            // constantly) and requires two things at once: the data agreed byte by byte
            // (the line "FSBIG: ALL OK" is checked by test.sh) AND the batch REALLY
            // assembled. The second is not a trifle: a knob that is parsed and does not
            // work cost session 35 a whole run of the stand (`158ffdc`).
            // THE EVICTION OF A DIRTY ONE MUST BE ACHIEVED, OTHERWISE THE GATE CHECKS EMPTINESS.
            // With the defaults it almost NEVER HAPPENS: the pre-emptive flush (§8.7,
            // `dirtyfrac`) cleans the blocks in advance, while `victim` prefers a clean
            // way to a dirty one — the whole `fsbig` run gives ONE eviction per 32 MB
            // of writing. So the mode itself squeezes the cache down to 2 MB and removes
            // the pre-emptive flush (`dirtyfrac=1` — the threshold is unreachable): then every
            // miss hits a dirty way, as on the stand under mongodb/insert.
            // The operator may name their own — we do not touch what is explicit from the command line.
            use core::sync::atomic::Ordering::SeqCst;
            if crate::cmdline::value(cmdline, "talaria.fscache").is_none() {
                crate::ext2::FSCACHE_MB.store(2, SeqCst);
            }
            if crate::cmdline::value(cmdline, "talaria.dirtyfrac").is_none() {
                crate::ext2::DIRTY_FRAC.store(1, SeqCst);
            }
            crate::println!("evictbatch: the cache is {} MB, the pre-emptive flush is 1/{}, the batch is up to {} blocks",
                crate::ext2::FSCACHE_MB.load(SeqCst), crate::ext2::DIRTY_FRAC.load(SeqCst),
                crate::ext2::EVICT_BATCH.load(SeqCst));
            // The result is computed by test.sh from the line `stat: the eviction of slots` (it is printed by
            // the `talaria.stat=` block): the code AFTER the workload's launch is not executed —
            // `run_process_disk` does not return from a self-check mode.
            if crate::fs::init() { unsafe { crate::user::run_process_disk(b"/fsbig", &[b"fsbig"], &[b"HOME=/"]); } }
            crate::suite::park()
        }
        "fsinfo" => {
            crate::smp::start_from_cmdline(cmdline); // talaria.smp=N works here too
            // What the OS tells about the file system: while statfs returned zeroes,
            // a database refused to write ("there is no room") on an empty volume.
            if crate::fs::init() { unsafe { crate::user::run_process_disk(b"/fsinfo", &[b"fsinfo"], &[b"HOME=/"]); } }
            crate::suite::park()
        }
        _ => return false,
    }
    true
}
