//! The self-tests of the network, of the control plane and of the supervisor.
//!
//! The name of the mode is compared AS A WHOLE (a `match` on the value of `talaria.test=`),
//! so a name that is a prefix of a foreign name can no longer start a foreign mode
//! — see `crate::cmdline`.

/// Run the self-test `name`, if it is one of this set.
/// Returns `true` if the name was recognised (the body may well not have returned — many
/// modes end with `suite::park()`).
// Many gates do not return at all (`suite::park()` or the start of a workload),
// so in the sets where ALL the modes do that, the trailing `true` is unreachable.
#[allow(unused_variables, unreachable_code)]
pub fn run(name: &str, cmdline: &str) -> bool {
    match name {
        "tickcmd" => {
            // Session 26: the marker "a command is being executed from the tick" was one per
            // machine, and while the BSP was printing the answer to `stat` from the tick, the network stack
            // answered EAGAIN to the syscalls of ALL the other cores — mongod's `close()`
            // is not retried, the socket stayed in CLOSE-WAIT forever, the slots
            // ran out. Here the BSP's tick executes `uptime` every 5 ms, while three
            // threads on the APs spin socket()+close(): the refusals are bound to be zero.
            static TC: &[u8] = user_elf!("tickcmd");
            println!("talaria: TICKCMD — a console command from the BSP's tick against the network syscalls on the APs ({} bytes)", TC.len());
            // The stack is bound to be up: without a card socket() answers
            // EAFNOSUPPORT before the lock, and the gate would be measuring the wrong path.
            match crate::runmode::open_nic() {
                Some((dev, mac)) => crate::netsys::init(dev, mac, crate::tsc::hz()),
                None => println!("TICKCMD: FAIL — no NIC was found (virtio-net is required)"),
            }
            crate::smp::lapic_enable();
            crate::smp::AP_MODE.store(2, core::sync::atomic::Ordering::SeqCst);
            for cpu in 1..4usize {
                if !crate::smp::start_cpu(cpu) { println!("TICKCMD: FAIL — AP{} did not come up (-smp 4 is required)", cpu); }
            }
            // The command is short (one line): a long print from the tick with IF=0
            // spends a VMEXIT per byte and itself becomes a load on the BSP.
            crate::console::tickcmd_arm(5_000_000);
            unsafe { crate::user::run_threaded(TC) }
        }
        "watchdog" => {
            // A4.2: first there is progress (the watchdog is healthy), then an artificial stall.
            println!("talaria: watchdog — the health of the workload (the detection of a hang -> a restart)");
            crate::watchdog::arm(0);
            let mut restarts = 0u32;
            // phase 1: the progress goes on — there must be no restarts
            for p in 1..=6u64 {
                if crate::watchdog::tick(p) { restarts += 1; }
            }
            let healthy_restarts = restarts;
            // phase 2: the progress has frozen (the same counter) — the watchdog is bound to fire
            for _ in 0..5 {
                if crate::watchdog::tick(6) { restarts += 1; }
            }
            println!("watchdog: restarts_under_progress={} restarts_in_total={} stalls_total={} telem_restarts={}",
                healthy_restarts, restarts, crate::watchdog::stalls_total(), crate::telemetry::restarts());
            if healthy_restarts == 0 && restarts >= 1 && crate::watchdog::stalls_total() >= 1 {
                println!("watchdog: ALL CHECKS PASSED (healthy under progress, a restart on a hang)");
            } else {
                println!("watchdog: FAILURE (healthy={} total={})", healthy_restarts, restarts);
            }
            crate::suite::park()
        }
        "ctlauth" => {
            // The whole authentication contract of the control plane: the token is taken
            // from the command line, without it the door is shut, the comparison is in
            // constant time (the details and the why are in ctlplane.rs).
            println!("talaria: control-plane auth — the token is mandatory");
            crate::ctlplane::selftest();
            crate::suite::park()
        }
        "syscallfilter" => {
            // A3.2: the allowlist = {write(1), exit(60)}; sfilter calls getuid(102) -> EPERM.
            static SF: &[u8] = user_elf!("sfilter");
            println!("talaria: syscall-filter — the allowlist {{1,60}}, the forbidden getuid(102) -> EPERM");
            crate::syscall::filter_enable(&[1, 60]);
            crate::user::run(SF);
        }
        "soak" => {
            // A4.1 soak: the allocator's churn (alloc/write/verify/free) over N iterations —
            // we check the absence of a leak/drift (the sum of the free memory is preserved) and of crashes.
            const N: u64 = 20000;
            let free0 = crate::mem::free_frames() + crate::mem::free_list_pages();
            let mut lcg: u64 = 0x1234_5678_9abc_def1;
            let mut ok = true; let mut peak_used = 0u64;
            let mut live: [(u64, u64); 64] = [(0, 0); 64]; // (addr, pages)
            let mut nlive = 0usize;
            for i in 0..N {
                lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let sz = 1 + (lcg >> 40) % 32; // 1..32 pages
                // sometimes we free, otherwise we allocate — we keep a window of live blocks
                if nlive >= 48 || (nlive > 0 && (lcg & 1) == 0) {
                    let k = (lcg >> 3) as usize % nlive;
                    let (a, p) = live[k];
                    // the verification of the pattern before the free (the detection of corruption)
                    let pat = a ^ 0xA5A5_A5A5_A5A5_A5A5;
                    if unsafe { core::ptr::read_volatile(a as *const u64) } != pat { ok = false; }
                    crate::mem::free_contig(a, p);
                    live[k] = live[nlive - 1]; nlive -= 1;
                } else if let Some(a) = crate::mem::alloc_contig(sz) {
                    let pat = a ^ 0xA5A5_A5A5_A5A5_A5A5;
                    unsafe {
                        core::ptr::write_volatile(a as *mut u64, pat);                          // the first qword
                        core::ptr::write_volatile((a + sz * 4096 - 8) as *mut u64, pat ^ i);     // the last one
                    }
                    if unsafe { core::ptr::read_volatile((a + sz * 4096 - 8) as *const u64) } != pat ^ i { ok = false; }
                    live[nlive] = (a, sz); nlive += 1;
                }
                let used = free0.saturating_sub(crate::mem::free_frames() + crate::mem::free_list_pages());
                if used > peak_used { peak_used = used; }
            }
            for k in 0..nlive { crate::mem::free_contig(live[k].0, live[k].1); } // give back the remainder
            let free1 = crate::mem::free_frames() + crate::mem::free_list_pages();
            println!("soak: iterations={} free_before={} free_after={} peak_used={} correctness={}",
                N, free0, free1, peak_used, ok);
            if ok && free1 == free0 {
                println!("soak: ALL CHECKS PASSED (0 crashes, 0 leak, free is stable)");
            } else {
                println!("soak: FAILURE (a leak={} or corruption={})", free0 as i64 - free1 as i64, !ok);
            }
            crate::metrics::dump();
            crate::suite::park()
        }
        "netcfg" => {
            // Without any cards: the arithmetic of routing. A mistake here does not crash, it QUIETLY
            // sends a packet into another interface — the most expensive class of mistake in
            // a network configuration.
            let ok = crate::netcfg::selftest();
            println!("netcfg-test: {}", if ok { "ALL OK" } else { "THERE ARE FAILURES" });
            crate::suite::park()
        }
        "filed" => {
            // Without the network: the checksum of the file service. If the CRC32 diverges from
            // the reference, "the file matches" will be said about DIFFERENT files, and an upload
            // will silently not arrive — the most expensive class of mistake in this service.
            crate::filed::selftest();
            crate::suite::park()
        }
        _ => return false,
    }
    true
}
