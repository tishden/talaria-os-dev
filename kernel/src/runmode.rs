//! The kernel's production modes (`talaria.mode=`): what the OS does when it is started not
//! for a self-check but for work.
//!
//! `rust_main` goes here on its last line and never returns. Three
//! layers, in parsing order:
//!
//! 1. `simple` — the modes that do not need the network: the operator shell, the supervisor,
//!    the gdb stub, process demonstrations, open-source runtimes from disk.
//! 2. a workload by the manifest `/cell.manifest` (`mode=cell|proc`) and
//!    the network servers (`mode=server|eserver|userver|…`);
//! 3. the default mode — the datapath on an AP, the control plane on the BSP.
//!
//! The mode's name is compared IN FULL (`crate::cmdline`) rather than searched for as a substring:
//! `mode=fast` and `mode=fastflood` are different modes, and their difference used to be held
//! by the order of the checks in the file.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::virtnet::{MMIO_ADDR, MMIO_SIZE};

/// The packets served by the datapath cell — for the control plane's line.
pub static CELL_PKTS: AtomicU64 = AtomicU64::new(0);

/// Choose and run a production mode. Does not return.
pub fn run(cmdline: &str, tsc_hz: u64) -> ! {
    if let Some(name) = crate::cmdline::value(cmdline, "talaria.mode") {
        simple(name, cmdline, tsc_hz);
    }
    // partition-first: bring an AP up and give it the datapath; the BSP = the control plane
    let smoltcp = crate::cmdline::mode_is(cmdline, "smoltcp");
    SMOLTCP.store(smoltcp, Ordering::SeqCst);
    TEST_IPI.store(crate::cmdline::test_is(cmdline, "ipi"), Ordering::SeqCst);

    HTTPD.store(crate::cmdline::mode_is(cmdline, "httpd"), Ordering::SeqCst);
    // The zero-syscall datapath. `fastflood` is the same mode plus a TX ring microbenchmark,
    // and that has to be said out loud: while the mode was chosen by substring search,
    // `fastflood` got here because it contains `fast`. The coincidence was
    // LOAD-BEARING — without it the cell did not come up at all — and written down nowhere.
    let is_fast = crate::cmdline::mode_is(cmdline, "fast") || crate::cmdline::mode_is(cmdline, "fastflood");
    FAST.store(is_fast, Ordering::SeqCst);
    if is_fast {
        let frames = (crate::fastpath::REGION_SZ as u64).div_ceil(4096);
        let base = crate::mem::alloc_contig(frames).expect("fastpath region");
        crate::fastpath::init_region(base as usize);
        crate::fastpath::set_region(base);
        println!("talaria: the shared zero-syscall region @ {:#x} ({} KB, user-accessible)", base, frames * 4);
    }
    // find the NIC on the BSP and pass the address to the AP
    if let Some((a, sz)) = crate::virtnet::mmio_window_from_cmdline(cmdline).or_else(crate::virtnet::scan_mmio_window) {
        MMIO_ADDR.store(a as u64, Ordering::SeqCst);
        MMIO_SIZE.store(sz as u64, Ordering::SeqCst);
    }
    // ── The canonical launch of a workload by the manifest /cell.manifest (see USER-GUIDE §6):
    //    talaria.mode=cell — a multithreaded workload (clone/futex) under the supervisor;
    //    talaria.mode=proc — a process in
    //    its own AS (fork/COW, signals, a dynamic ELF). Both with the network (connect/DNS),
    //    the NIC is optional for cell/proc. Plus talaria.test=connect — a regression of outgoing TCP.
    if crate::cmdline::mode_is(cmdline, "cell") || crate::cmdline::mode_is(cmdline, "proc") || crate::cmdline::test_is(cmdline,
        "connect") || crate::cmdline::test_is(cmdline, "netshoot") || crate::cmdline::test_is(cmdline, "idleshoot") || crate::cmdline::test_is(cmdline,
        "forktls") {
        // talaria.smp=N (2..=MAXCPU): bring up N-1 APs with schedulers of pinned threads —
        // the workload (mode=cell/proc) migrates its threads with sched_setaffinity.
        crate::smp::start_from_cmdline(cmdline);
        // THE NETWORK MAY HAVE BEEN BROUGHT UP EARLIER — AND THEN IT IS NOT TOUCHED.
        //
        // On EC2 the stack comes up before the mode is chosen, in order to fetch the user-data
        // (`talaria.userdata=1`, main.rs). The comment there promised that "the modes'
        // branches see `netsys::ready()` and do not reopen the NIC" — but
        // there was no check in A SINGLE branch: `open_nic()` and `netsys::init()`
        // were called unconditionally. The NIC was initialised a SECOND time, with a second
        // set of rings and DMA buffers.
        //
        // That cost the node's health: on EC2 a machine with a large workload
        // (mongod) received from the device, after a few seconds, a FATAL
        // ERROR (`DEV_STS=0x21` — READY|FATAL_ERROR), receive stopped dead, and
        // the node disappeared from the network AFTER a successful DHCP lease. The discriminator:
        // the same workload with ONE initialisation works (`sts=0x1`), while a
        // tiny tlgen survives even two.
        let reuse = crate::netsys::ready();
        // Two reasons not to open the card, and neither of them is an error: the fast
        // datapath does not want a smoltcp NIC at all, and a network already up from the
        // user-data phase must not be reopened (see the comment above).
        let nic = if crate::cmdline::flag(cmdline, "talaria.fast") || reuse { None } else { open_nic() };
        if reuse { println!("talaria: the network is already up from the user-data phase — I am not reopening the NIC"); }
        if !reuse && nic.is_none() && !crate::cmdline::flag(cmdline, "talaria.fast") {
            // A workload without a network does not need a NIC: we start it by the manifest without netsys
            // (the socket syscalls will return an error; compute/file applications work).
            println!("talaria: cell: no NIC found — starting without a network");
            // Neither of the two needs a network: without this they fell through to the
            // manifest on a machine with no NIC (arm64's `virt` before its virtio-net).
            if crate::cmdline::test_is(cmdline, "forktls") { run_forktls(); }
            if crate::cmdline::test_is(cmdline, "idleshoot") { run_idleshoot(); }
            if crate::cmdline::mode_is(cmdline, "proc") { run_manifest_process_at(manifest_path(cmdline)); }
            run_manifest_threaded();
        }
        else if crate::cmdline::flag(cmdline, "talaria.fast") {
            // The zero-syscall datapath for a workload from disk: the NIC is given to the cell on an AP (virtio <->
            // rings in shared memory), the kernel stack (netsys) is NOT brought up; the application
            // (the io=talaria backend) polls the rings without syscalls (talaria_fastmap -> the base).
            let frames = (crate::fastpath::REGION_SZ as u64).div_ceil(4096);
            let base = crate::mem::alloc_contig(frames).expect("fastpath region");
            crate::fastpath::init_region(base as usize);
            crate::fastpath::set_region(base);
            FAST.store(true, Ordering::SeqCst);
            println!("talaria: the zero-syscall region @ {:#x} ({} KB); the datapath cell -> an AP", base, frames * 4);
            // The NIC is opened by the BSP (ENA requires PCIe/admin from the boot
            // core) and handed to the cell on an AP — the AP used to bring virtio-mmio up itself.
            match open_nic() {
                Some((d, m)) => nic_handoff_put(d, m),
                None => println!("talaria: fast: no NIC found — the cell will run idle"),
            }
            if !crate::smp::start_cpu(1) { println!("talaria: the AP did not come up — -smp 2 is needed"); crate::suite::park() }
            run_manifest_threaded();
        } else {
            if let Some((dev, mac)) = nic { crate::netsys::init(dev, mac, tsc_hz); }
            if crate::cmdline::test_is(cmdline, "connect") {
                static CT: &[u8] = user_elf!("connect_test");
                println!("talaria: a test of outgoing TCP + DNS + the wall clock ({} bytes)", CT.len());
                crate::user::run(CT);
            }
            if crate::cmdline::test_is(cmdline, "forktls") { run_forktls(); }
            if crate::cmdline::test_is(cmdline, "idleshoot") { run_idleshoot(); }
            if crate::cmdline::test_is(cmdline, "netshoot") {
                // A network waiter must acknowledge TLB shootdowns to its neighbours: the wait
                // path runs with interrupts disabled, and the shootdown vector's call
                // does not reach it (see user/netshoot.c).
                static NS: &[u8] = user_elf!("netshoot");
                println!("talaria: netshoot {} bytes — a thread in accept() against neighbours in mmap/munmap", NS.len());
                unsafe { crate::user::run_threaded(NS) };
            }
            if crate::cmdline::mode_is(cmdline, "proc") { run_manifest_process_at(manifest_path(cmdline)); }
            run_manifest_threaded();
        }
    }
    // The server mode: netsys on the BSP + a musl server in ring 3 (the socket syscalls -> smoltcp)
    if crate::cmdline::mode_is(cmdline, "server") || crate::cmdline::mode_is(cmdline, "eserver") || crate::cmdline::mode_is(cmdline,
        "diskserver") || crate::cmdline::mode_is(cmdline, "userver") || crate::cmdline::mode_is(cmdline, "mtsrv") || crate::cmdline::mode_is(cmdline,
        "forksrv") || crate::cmdline::mode_is(cmdline, "mongoose") || crate::cmdline::mode_is(cmdline, "redis") {
        // The same check as for cell/proc above: if the stack is already up from the
        // user-data phase, we do not create a second set of rings.
        match if crate::netsys::ready() { None } else { open_nic() } {
        None if !crate::netsys::ready() => println!("talaria: server: no NIC found (neither virtio-mmio nor ENA)"),
        nic => {
            if let Some((dev, mac)) = nic { crate::netsys::init(dev, mac, tsc_hz); }
            if crate::cmdline::mode_is(cmdline, "userver") {
                static UD: &[u8] = user_elf!("userver");
                println!("talaria: a musl UDP server, {} bytes — recvfrom/sendto -> smoltcp (ring 3)", UD.len());
                crate::user::run(UD);
            }
            if crate::cmdline::mode_is(cmdline, "diskserver") {
                if crate::fs::init() {
                    static mut EBUF: [u8; 262144] = [0; 262144];
                    let b: &mut [u8] = unsafe { &mut *core::ptr::addr_of_mut!(EBUF) };
                    let n = crate::fs::read_all(b"epsrv\0", b);
                    println!("talaria: an epoll server loaded from disk ({} bytes) — the network, epoll and storage together", n);
                    if n > 0 { crate::user::run(&b[..n]); }
                }
            }
            if crate::cmdline::mode_is(cmdline, "redis") {
                // Real open source: the Redis 7.2 server database from disk in ring 3 (RESP over TCP).
                if crate::fs::init() {
                    println!("talaria: Redis (an open-source server database) from disk — redis-server /redis.conf");
                    unsafe { crate::user::run_disk(b"/redis-server\0".as_ptr(), &[b"redis-server", b"/redis.conf"]); }
                }
            }
            if crate::cmdline::mode_is(cmdline, "mongoose") {
                // Real open source: the mongoose web server (cesanta) from disk in ring 3.
                if crate::fs::init() {
                    static mut MBUF: [u8; 1 << 20] = [0; 1 << 20]; // 1 MB for the binary
                    let b: &mut [u8] = unsafe { &mut *core::ptr::addr_of_mut!(MBUF) };
                    let n = crate::fs::read_all(b"mongoose_srv\0", b);
                    println!("talaria: mongoose (an open-source web server) loaded from disk ({} bytes)", n);
                    if n > 0 { crate::user::run(&b[..n]); }
                }
            }
            if crate::cmdline::mode_is(cmdline, "mtsrv") {
                // A THREAD PER CONNECTION — MariaDB's model. It is needed as a separate mode
                // because the rule "one stack pass per machine" touches
                // only it: in a single-threaded server the waiter is one and the same as the
                // poller, and the parking branch is never taken.
                static MT: &[u8] = user_elf!("mtsrv");
                println!("talaria: a musl TCP server with A THREAD PER CONNECTION, {} bytes (MariaDB's model)", MT.len());
                unsafe { crate::user::run_threaded(MT) };
            }
            if crate::cmdline::mode_is(cmdline, "forksrv") {
                // A FORK PER CONNECTION — PostgreSQL's model. It goes with the process model
                // (from disk), because the gate needs a real fork with COW rather than a
                // thread: the defect lives exactly in the fact that socket ownership
                // survives the parent closing the descriptor.
                if crate::fs::init() {
                    // `talaria.forkconns=N` — how many connections to serve before
                    // exiting. Six are enough for the socket-ownership gate; the question
                    // "where does the listener go" (a stand run: "listeners
                    // 0" after 119 accepted) needs hundreds of cycles.
                    static mut NBUF: [u8; 8] = [0; 8];
                    let n = crate::cmdline::value(cmdline, "talaria.forkconns")
                        .and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
                    println!("talaria: a server with A FORK PER CONNECTION from disk (PostgreSQL's model), connections {}",
                        if n == 0 { 6 } else { n });
                    if n == 0 {
                        unsafe { crate::user::run_process_disk(b"/forksrv", &[b"forksrv"], &[b"HOME=/"]); }
                    } else {
                        // The number goes into an argument as a string, without alloc: the process has
                        // its own address space, and argv is assembled here.
                        let b = unsafe { &mut *core::ptr::addr_of_mut!(NBUF) };
                        let mut i = b.len() - 1;
                        b[i] = 0;
                        let mut v = n;
                        loop { i -= 1; b[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
                        if crate::cmdline::flag(cmdline, "talaria.forkreap") {
                            unsafe { crate::user::run_process_disk(b"/forksrv", &[b"forksrv", &b[i..], b"reap"], &[b"HOME=/"]); }
                        } else {
                            unsafe { crate::user::run_process_disk(b"/forksrv", &[b"forksrv", &b[i..]], &[b"HOME=/"]); }
                        }
                    }
                }
            }
            if crate::cmdline::mode_is(cmdline, "eserver") {
                static ESRV: &[u8] = user_elf!("epsrv");
                println!("talaria: a musl EPOLL server, {} bytes — epoll/accept4 -> smoltcp (ring 3)", ESRV.len());
                crate::user::run(ESRV);
            }
            static SRV: &[u8] = user_elf!("srv");
            println!("talaria: a musl TCP server, {} bytes — the socket syscalls -> smoltcp (ring 3)", SRV.len());
            crate::user::run(SRV);
        }
        }
    }
    println!("talaria: bringing an AP up (APIC ID 1) for the datapath cell...");
    if let Some((d, m)) = open_nic() { nic_handoff_put(d, m); } // the NIC is opened on the BSP
    if crate::smp::start_cpu(1) {
        println!("talaria: the AP is up — the datapath on core 1, the BSP = the control plane");
        if FAST.load(Ordering::SeqCst) {
            if crate::cmdline::mode_is(cmdline, "fastflood") {
                // A microbenchmark of the cell's TX rings (the price of a virtio kick; with vhost
                // poll-us the host suppresses notify through EVENT_IDX — the kick disappears)
                static FF: &[u8] = user_elf!("fastflood");
                println!("talaria: fastflood {} bytes — flooding the cell's TX ring", FF.len());
                crate::user::run(FF);
            }
            static FAPP: &[u8] = user_elf!("fastapp");
            println!("talaria: a musl zero-syscall application, {} bytes — it polls the rings in ring 3", FAPP.len());
            crate::user::run(FAPP);
        }
    } else {
        println!("talaria: the AP did not come up — the datapath on the BSP (a fallback)");
        run_datapath();
    }
    // The BSP = the control plane: a preemptive scheduler of cold tasks
    let _ = tsc_hz;
    crate::smp::lapic_enable();
    crate::smp::lapic_timer_init(crate::smp::SLICE_NS);
    let sa = crate::mem::alloc_frame().unwrap() + 4096;
    let sb = crate::mem::alloc_frame().unwrap() + 4096;
    unsafe {
        crate::sched::spawn(task_control_plane, sa);
        crate::sched::spawn(task_housekeeping, sb);
        // No `irq_enable` between the two: from `install` on a tick is a context switch, and one
        // that lands before `start` saves THE BOOT CONTEXT into task 0's slot — `start` then
        // "enters task 0" on the boot stack (arm64 under TCG hit it in 1 boot of 10: the
        // timer's period had long passed, so the tick was pending at the unmask). The task's
        // own frame turns interrupts on at the entry.
        crate::sched::install();                 // the timer -> a context switch
        crate::sched::start()                    // -> ! : enter task 0, the timer preempts from there on
    }
}

/// The production modes that do not need the network: the shell, the supervisor, the debugger, process
/// demonstrations and open-source runtimes from disk. Returns `true` if the name was recognised
/// (most modes do not return at all).
// Many gates do not return at all (`suite::park()` or starting a workload),
// so in sets where EVERY mode does that the trailing `true` is unreachable.
#[allow(unused_variables, unreachable_code)]
fn simple(name: &str, cmdline: &str, tsc_hz: u64) -> bool {
    match name {
        "supervise" => {
            if crate::fs::init() {
                // A declarative launch by the manifest /cell.manifest (if there is one), otherwise /crashme.
                static mut MBUF: [u8; 8192] = [0; 8192];
                let mbuf: &mut [u8] = unsafe { &mut *core::ptr::addr_of_mut!(MBUF) };
                let mn = crate::fs::read_all(b"cell.manifest\0", mbuf);
                manifest_fit(mn, mbuf.len());
                let m = crate::cellman::parse(&mbuf[..mn]);
                if m.valid {
                    let mut argv: [&[u8]; 8] = [b""; 8];
                    for i in 0..m.argc { argv[i] = m.arg(i); }
                    println!("talaria: the supervisor by the manifest (restart={}, argc={})", m.restart, m.argc);
                    crate::supervise::supervise(m.binary_cstr().as_ptr(), &argv[..m.argc], m.restart);
                }
                println!("talaria: the supervisor — there is no manifest, starting /crashme (max 3)");
                crate::supervise::supervise(b"/crashme\0".as_ptr(), &[b"crashme"], 3);
            }
        }
        "gdb" => {
            crate::gdb::enable();
            println!("talaria: the gdb stub is on — connect with: target remote :1234 (an int3 stop)");
            crate::gdb::breakpoint(); // stop for GDB
            // after continue — a simple loop, so that there is something to debug
            let mut x: u64 = 0;
            loop { x = x.wrapping_add(1); if x.is_multiple_of(100000000) { crate::gdb::breakpoint(); } }
        }
        "console" | "filed" => {
            // The operator console with the network. The stack is needed by the `ip`/`route`/`filed` commands;
            // without a NIC the console is still complete — files, memory, tasks, the clock.
            if !crate::netsys::ready() {
                if let Some((a, sz)) = crate::virtnet::mmio_window_from_cmdline(cmdline).or_else(crate::virtnet::scan_mmio_window) {
                    MMIO_ADDR.store(a as u64, Ordering::SeqCst);
                    MMIO_SIZE.store(sz as u64, Ordering::SeqCst);
                }
                // We bring up ALL the machine's NICs rather than the first one. That is exactly how a node
                // learns that its management network is separate from the working one; the second
                // NIC simply did not exist for the OS before this.
                match crate::netdev::probe_next() {
                    Some((d, m, drv)) => crate::netsys::init_named(d, m, drv, tsc_hz),
                    None => println!("talaria: console: no NIC found — the console without a network"),
                }
                if crate::netsys::ready() {
                    while crate::netsys::nifs() < crate::netsys::MAXIF {
                        match crate::netdev::probe_next() {
                            Some((d, m, drv)) => { if crate::netsys::add_iface(d, m, drv, tsc_hz).is_none() { break; } }
                            None => break,
                        }
                    }
                    crate::netcfg::apply_pending(); // talaria.net=/talaria.route= — by interface names
                    println!("talaria: interfaces brought up: {} (`ip` — the addresses, `route` — the routes)", crate::netsys::nifs());
                }
            }
            // talaria.filed=<port> (or mode=filed) — the upload service comes up at once:
            // an operator who came to upload a file should not have to log in first.
            if crate::filed::configured_port() != 0 { crate::filed::start(crate::filed::configured_port()); }
            crate::console::run();
        }
        "procdemo" => {
            // The full process cycle: launcher fork -> child execve(/busybox) -> parent wait4.
            if crate::fs::init() {
                static LAUNCHER: &[u8] = user_elf!("launcher");
                println!("talaria: a process model demo — fork + execve(busybox) + wait4");
                unsafe { crate::user::run_processes(&[LAUNCHER]); }
            }
        }
        "busybox" => {
            // Real open source: busybox (a multi-tool) runs an applet in ring 3.
            if crate::fs::init() {
                println!("talaria: busybox (an open-source multi-tool) from disk — the awk applet");
                unsafe { crate::user::run_disk(b"/busybox\0".as_ptr(), &[b"busybox", b"awk",
                    b"BEGIN{s=0;for(i=1;i<=1000;i++)s+=i*i;printf \"busybox awk: sum(i^2,1..1000)=%d\\n\",s;\
    n=\"talaria\";printf \"busybox awk: upper(%s)=%s len=%d\\n\",n,toupper(n),length(n);\
    print \"busybox 1.36.1 on Talaria OS\"}"]); }
            }
        }
        "sqlite" => {
            // Real open source: the SQLite database engine (in-memory) runs SQL in ring 3.
            if crate::fs::init() {
                println!("talaria: SQLite (an open-source database engine) from disk — in-memory SQL");
                unsafe { crate::user::run_disk(b"/sqlite3\0".as_ptr(), &[b"sqlite3", b":memory:",
                    b"CREATE TABLE t(id INTEGER PRIMARY KEY,name TEXT,val REAL);\
    INSERT INTO t(name,val) VALUES('alpha',1.5),('beta',2.5),('gamma',3.5),('delta',4.5);\
    SELECT '=rows='||count(*)||' sum='||round(sum(val),1)||' avg='||round(avg(val),2) FROM t;\
    SELECT name||'='||val FROM t WHERE val>2 ORDER BY val DESC;\
    SELECT 'SQLite '||sqlite_version()||' on Talaria OS';"]); }
            }
        }
        "quickjs" => {
            // Real open source: the QuickJS JavaScript engine (Bellard) runs demo.js in ring 3.
            if crate::fs::init() {
                println!("talaria: QuickJS (an open-source JS engine) from disk — qjs /demo.js");
                unsafe { crate::user::run_disk(b"/qjs\0".as_ptr(), &[b"qjs", b"/demo.js"]); }
            }
        }
        "lua" => {
            // Real open source: the Lua 5.4 interpreter from disk runs script.lua in ring 3.
            if crate::fs::init() {
                println!("talaria: the Lua interpreter (open source) from disk — lua /script.lua");
                unsafe { crate::user::run_disk(b"/lua\0".as_ptr(), &[b"lua", b"/script.lua"]); }
            }
        }
        _ => return false,
    }
    true
}

extern "C" fn task_control_plane() -> ! {
    use core::sync::atomic::Ordering;
    let mut last_pkts = 0u64;
    loop {
        for _ in 0..300_000_000u64 { core::hint::spin_loop(); }
        let p = CELL_PKTS.load(Ordering::Relaxed);
        crate::smp::send_ipi(crate::smp::apic_of(1), crate::idt::IPI_VECTOR as u8);
        println!("[task A: the control plane] the cell {} pkts | IPI->AP={}",
            p - last_pkts, crate::idt::IPI_COUNT.load(Ordering::Relaxed));
        last_pkts = p;
    }
}


extern "C" fn task_housekeeping() -> ! {
    let mut n = 0u64;
    loop {
        for _ in 0..300_000_000u64 { core::hint::spin_loop(); }
        n += 1;
        println!("[task B: housekeeping] tick {} | free frames={}", n, crate::mem::free_frames());
    }
}


// The virtio-mmio window lives in its own driver (virtnet.rs) — here there are only the names,
// so as not to edit every place where it is filled in while parsing the cmdline.
static SMOLTCP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// `talaria.test=forktls`: after a fork the child's TLS must be intact (see user/forktls.c).
/// It returns only when there is no disk to take the program from.
fn run_forktls() {
    if crate::fs::init() {
        println!("talaria: forktls — verifying the TLS of forked children");
        // SAFETY: L1 + L4 — the launch path: once, from the boot core, before any ring 3.
        unsafe { crate::user::run_process_disk(b"/forktls", &[b"forktls"], &[b"HOME=/"]); }
    }
}
/// `talaria.test=idleshoot`: one thread removes mappings, one sleeps, two cores idle.
fn run_idleshoot() -> ! {
    static IS: &[u8] = user_elf!("idleshoot");
    println!("talaria: idleshoot {} bytes — one thread removes mappings, one sleeps, two cores idle", IS.len());
    // SAFETY: L1 + L4 — the launch path: once, from the boot core, before any ring 3.
    unsafe { crate::user::run_threaded(IS) }
}

pub static TEST_IPI: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static HTTPD: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static FAST: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);


/// Open the network card through THE DRIVER TABLE (`crate::netdev::DRIVERS`): which
/// card is in the machine and which driver takes it is decided by the registry, not by this place.
/// Every driver keeps its device in its own static slot —
/// the card lives until the reboot and travels through the code by reference rather than by copy
/// (`Nic` used to be copied at every step of the path to the cell, and the AP's stack had to be
/// kept at 128 KB).
pub fn open_nic() -> Option<(&'static mut dyn crate::netdev::NetDev, [u8; 6])> {
    crate::netdev::probe_all()
}


/// Handing the NIC from the BSP to an AP: ENA is initialised on the BSP (PCIe
/// enumeration + the admin queue), while the cell/kernel modes live on an AP. We put it here
/// BEFORE start_ap and take it in run_datapath.
static mut NIC_HANDOFF: Option<(&'static mut dyn crate::netdev::NetDev, [u8; 6])> = None;
pub fn nic_handoff_put(n: &'static mut dyn crate::netdev::NetDev, mac: [u8; 6]) { unsafe { NIC_HANDOFF = Some((n, mac)); } }
fn nic_handoff_take() -> Option<(&'static mut dyn crate::netdev::NetDev, [u8; 6])> { unsafe { (*core::ptr::addr_of_mut!(NIC_HANDOFF)).take() } }


/// The cell's datapath: the NIC + a busy-poll echo reactor. Called from an AP.
pub fn run_datapath() -> ! {
    let tsc_hz = crate::tsc::calibrate_cached();
    // The NIC is already open on the BSP (virtio-mmio or ENA) and handed here.
    // The fallback of opening virtio-mmio itself is for the old paths.
    let handoff = nic_handoff_take().or_else(open_nic);
    let (nic, mac) = match handoff {
        Some(x) => x,
        None => crate::suite::park(), // no NIC — the AP idles
    };
    if FAST.load(Ordering::SeqCst) {
        let base = crate::fastpath::region() as usize;
        println!("talaria: the zero-syscall cell on core 1 — the NIC<->rings @ {:#x}", base);
        crate::fastpath::run_cell(nic, mac, base);
    }
    if HTTPD.load(Ordering::SeqCst) { crate::l1::run_httpd(nic, mac, tsc_hz); }
    if SMOLTCP.load(Ordering::SeqCst) { crate::l1::run(nic, mac, tsc_hz); }
    // The default datapath is the smoltcp echo. The legacy raw reactor (an in-place echo
    // without a stack, an artefact of v0) went away with the device model: it could
    // work ONLY with raw virtio, i.e. it required knowing the NIC's type — exactly
    // what `NetDev` removes. It duplicated the same echo, and on ENA that path
    // went into `crate::l1::run` anyway.
    crate::l1::run(nic, mac, tsc_hz)
}


/// Starting a multithreaded workload from disk by /cell.manifest under the supervisor (the cell mode).
fn run_manifest_threaded() -> ! {
    if crate::fs::init() {
        static mut MBUF: [u8; 8192] = [0; 8192];
        let mbuf: &mut [u8] = unsafe { &mut *core::ptr::addr_of_mut!(MBUF) };
        let mn = crate::fs::read_all(b"cell.manifest\0", mbuf);
        manifest_fit(mn, mbuf.len());
        let m = crate::cellman::parse(&mbuf[..mn]);
        if m.valid {
            let mut argv: [&[u8]; crate::cellman::MAXARG] = [b""; crate::cellman::MAXARG];
            for i in 0..m.argc { argv[i] = m.arg(i); }
            let mut envv: [&[u8]; crate::cellman::MAXENV] = [b""; crate::cellman::MAXENV];
            for i in 0..m.envc { envv[i] = m.env(i); }
            println!("talaria: a workload by the manifest /cell.manifest (restart={}, argc={}, envc={})", m.restart, m.argc, m.envc);
            crate::affinity::configure(&m); // the placement of threads on cores (pin/spread/balance/numa)
            crate::fs::set_exe(m.binary_cstr().as_ptr());
            crate::supervise::WATCHDOG_SECS.store(m.watchdog as u64, Ordering::SeqCst);
            crate::supervise::supervise_threaded(m.binary_cstr().as_ptr(), &argv[..m.argc], &envv[..m.envc], m.restart);
        }
        println!("talaria: cell: there is no /cell.manifest on the disk (build a disk with talaria-pack --app ...)");
    }
    crate::suite::park()
}


/// talaria.mode=proc: a process from disk by /cell.manifest (binary/args/env) in its own AS —
/// fork/COW, signals, fd tables, RW ext2 (the PostgreSQL postmaster and the like).
fn run_manifest_process_at(path: &[u8]) -> ! {
    if crate::fs::init() {
        static mut MBUF: [u8; 8192] = [0; 8192];
        let mbuf: &mut [u8] = unsafe { &mut *core::ptr::addr_of_mut!(MBUF) };
        let mn = crate::fs::read_all(path, mbuf);
        let m = crate::cellman::parse(&mbuf[..mn]);
        if m.valid {
            let mut argv: [&[u8]; crate::cellman::MAXARG] = [b""; crate::cellman::MAXARG];
            for i in 0..m.argc { argv[i] = m.arg(i); }
            let mut envv: [&[u8]; crate::cellman::MAXENV] = [b""; crate::cellman::MAXENV];
            for i in 0..m.envc { envv[i] = m.env(i); }
            println!("talaria: a process by the manifest (argc={}, envc={})", m.argc, m.envc);
            crate::affinity::configure(&m);
            crate::fs::set_exe(m.binary_cstr().as_ptr());
            unsafe { crate::user::run_process_disk(m.binary(), &argv[..m.argc], &envv[..m.envc]); }
        }
        println!("talaria: proc: there is no manifest on the disk");
    }
    crate::suite::park()
}

/// The manifest's path: `/cell.manifest` unless `talaria.manifest=` is named.
///
/// Why the parameter. One and the same image (AMI) must be able to be both a server and
/// a load generator: on a "Talaria against Talaria" stand both machines must
/// boot from ONE image, otherwise two builds are compared rather than two roles. The role
/// is chosen by user-data when the instance is launched.
/// A MANIFEST THAT DID NOT FIT INTO THE BUFFER MUST SAY SO.
///
/// The buffer was 4 KB, and the file was read exactly up to it — silently. A tuning setting
/// that ended up beyond the boundary simply did not exist: `fscache=512` in the mongodb
/// manifest (a comment with A/B numbers had grown the file to 5251 bytes) was not applied,
/// and the final scoreboard was taken with the old cache. From the outside that is indistinguishable from "the knob does not
/// work": there are no refusal lines, because there was no refusal.
fn manifest_fit(n: usize, cap: usize) {
    if n >= cap {
        println!("talaria: WARNING — the manifest was read exactly up to the buffer's limit ({} bytes): the file's tail was DISCARDED, the settings in it were \
            not applied", cap);
    }
}

/// THE NODE'S ROLE (the manifest's path) GOES INTO EVERY COUNTER SNAPSHOT RATHER THAN ONLY INTO
/// THE FIRST LINES OF THE BOOT.
///
/// The stand's post-condition "the server has the role /cell.manifest, the generator
/// /loadgen.manifest" was read from serial by the BRING-UP line. As soon as the counter
/// block (`talaria.stat=`) started working for real, it began pushing the bring-up
/// lines out of the `get-console-output` window (64 KB) within a couple of minutes — and
/// the role check started honestly failing a stand with a working kernel: 226 KB of snapshots per
/// run. The role must be visible in the FRESH part of the log.
static ROLE_LEN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static mut ROLE_BUF: [u8; 128] = [0; 128];
pub fn role_path() -> &'static str {
    let n = ROLE_LEN.load(core::sync::atomic::Ordering::Relaxed);
    if n == 0 { return "—"; }
    unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(core::ptr::addr_of!(ROLE_BUF) as *const u8, n)) }
}
fn role_note(p: &[u8]) {
    let n = p.iter().position(|&c| c == 0).unwrap_or(p.len()).min(128);
    unsafe {
        let b: &mut [u8; 128] = &mut *core::ptr::addr_of_mut!(ROLE_BUF);
        b[..n].copy_from_slice(&p[..n]);
    }
    ROLE_LEN.store(n, core::sync::atomic::Ordering::Relaxed);
}
fn manifest_path(cmdline: &str) -> &'static [u8] {
    static mut PBUF: [u8; 128] = [0; 128];
    let r = match crate::cmdline::value(cmdline, "talaria.manifest") {
        None => b"/cell.manifest\0" as &'static [u8],
        Some(v) => unsafe {
            let b: &mut [u8; 128] = &mut *core::ptr::addr_of_mut!(PBUF);
            let bytes = v.as_bytes();
            let n = bytes.len().min(126);
            let off = if bytes[0] == b'/' { 0 } else { b[0] = b'/'; 1 };
            b[off..off + n].copy_from_slice(&bytes[..n]);
            b[off + n] = 0;
            core::slice::from_raw_parts(b.as_ptr(), off + n + 1)
        },
    };
    role_note(r);
    r
}
