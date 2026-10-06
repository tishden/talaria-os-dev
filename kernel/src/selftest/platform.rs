//! The self-tests of the platform: time, interrupts, PCI/MSI-X, the console, the PMU.
//!
//! Here is everything that checks the HARDWARE and the bring-up: the calibration of the slice and of the clock, the PHC,
//! the partitioning of the L3 (CAT), write-combining, the screen, the serial port,
//! the LAPIC timer, the PMU's counters and the crypto primitive.
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
        // The parsing of the command line itself: a name that is a prefix must not start a foreign
        // mode. The gate stands first — all the others are chosen by this mechanism.
        "cmdline" => {
            crate::cmdline::selftest();
            crate::suite::park()
        }
        "slice" => {
            // The scheduler's slice is TIME, not a LAPIC counter. The kernel lived with this
            // mistake all its life: one and the same number meant 8 ms under QEMU and
            // 320 ms on bare metal.
            crate::smp::slice_selftest();
            crate::suite::park()
        }
        "clock" => {
            let mut ok = crate::tsc::servo_selftest();
            if !crate::tsc::have_ref() {
                println!("clock-test: kvmclock is unavailable — the live part is SKIPPED (a machine without a reference)");
            } else {
                crate::tsc::set_sync_ms(50); // over a second of the run ~20 adjustments will accumulate
                let hz = crate::tsc::calibrate_cached();
                let t0 = crate::tsc::rdtsc();
                let mut prev = crate::tsc::wall_ns();
                let mut back = 0u64;
                let mut reads = 0u64;
                // The reader and the writer are in one thread: an adjustment falls exactly
                // between the reads — that is the check of the published reference point.
                while crate::tsc::rdtsc() - t0 < hz {
                    crate::tsc::wall_tick();
                    let w = crate::tsc::wall_ns();
                    if w < prev { back += 1; }
                    prev = w; reads += 1;
                }
                let (ppm, err, adj, steps, sback, feeds) = crate::tsc::wall_stats();
                println!("clock-test: over a second samples {}, adjustments {}, steps {}, reads {}, backwards {}",
                    feeds, adj, steps, reads, back);
                println!("clock-test: the correction is {} ppm, the residual error is {} ns, the reference is {}", ppm, err, crate::tsc::source_name());
                let bad_err = !(-1_000_000..=1_000_000).contains(&err);
                if feeds < 5 { println!("clock-test: FAILURE — the discipline did not work even once"); ok = false; }
                if back != 0 || sback != 0 { println!("clock-test: FAILURE — the wall clock went backwards"); ok = false; }
                if steps != 0 { println!("clock-test: FAILURE — a step by value was needed (the base and kvmclock diverged)"); ok = false; }
                if bad_err { println!("clock-test: FAILURE — the error against kvmclock is greater than a millisecond"); ok = false; }
            }
            println!("clock-test: {}", if ok { "ALL OK" } else { "FAILURE" });
            crate::suite::park()
        }
        // The model of a network device: the table of drivers, the binding rules and the trait
        // `NetDev`. The gate is arranged so as to check the STATEMENT "a new card is
        // a driver file and a line in the table" rather than retell it: the loopback card
        // (`nicloop.rs`) is connected in exactly this way, and the whole datapath is driven
        // through it, knowing nothing about it.
        "nicdev" => {
            let mut ok = true;
            crate::netdev::dump();

            // 1) THE BINDING RULES. A mistake here does not crash — it quietly gives a card
            //    to a foreign driver, and that is a failure which is then hunted for days. We check it
            //    on synthetic devices, including those that stand in the
            //    dev machine and that do not exist under QEMU.
            let mk = |v: u16, d: u16, c: u32| crate::pci::PciDev {
                bus: 0, dev: 0, func: 0, vendor: v, device: d, class: c, msix_cap: 0, msi_cap: 0 };
            let ena_pf = mk(0x1d0f, 0xec20, 0x020000);   // ENA PF (EC2)
            let ixgbe = mk(0x8086, 0x10fb, 0x020000);    // Intel 82599ES 10G
            let alveo = mk(0x10ee, 0x5020, 0x120000);    // Xilinx Alveo U50 (class 0x12)
            use crate::netdev::Bind;
            let eth = Bind::PciClass(0x020000, 0xFFFF00);
            let checks: [(bool, bool, &str); 7] = [
                (crate::netdev::matches(&Bind::Pci(0x1d0f, 0xec20), &ena_pf), true, "the ENA PF fits its own rule"),
                (crate::netdev::matches(&Bind::Pci(0x1d0f, 0xec20), &ixgbe), false, "the 82599 does NOT fit the ENA rule"),
                (crate::netdev::matches(&Bind::Pci(0x1d0f, 0xec21), &ena_pf), false, "the VF rule does not catch the PF"),
                (crate::netdev::matches(&eth, &ixgbe), true, "the 82599 fits the rule by the Ethernet class"),
                (crate::netdev::matches(&eth, &alveo), false, "the Alveo (class 0x12) is not Ethernet"),
                (crate::netdev::matches(&Bind::Platform, &ena_pf), false, "Platform does not bind to PCI"),
                (crate::netdev::matches(&Bind::OnDemand, &ena_pf), false, "OnDemand does not bind by itself"),
            ];
            for (got, want, what) in checks {
                if got != want { println!("nicdev: FAILURE — {}", what); ok = false; }
            }
            println!("nicdev: binding rules checked {}", checks.len());

            // 1b) HOW THE CARD IS ABLE TO INTERRUPT. Before this the kernel parsed only MSI-X,
            //     and a card with a single MSI (an FPGA shell, an old 10G) came up silently and
            //     just as silently delivered not a single interrupt. Now the kind is known
            //     in advance — and about INTx the kernel says out loud that it will not wire it up.
            let mut d_msix = alveo; d_msix.msix_cap = 0x70;
            let mut d_msi = alveo; d_msi.msi_cap = 0x50;
            if crate::pci::intr_kind(&d_msix) != "MSI-X" { println!("nicdev: FAILURE — MSI-X was not recognised"); ok = false; }
            if crate::pci::intr_kind(&d_msi) != "MSI" { println!("nicdev: FAILURE — MSI was not recognised"); ok = false; }
            if crate::pci::intr_kind(&alveo) != "INTx (not wired up)" { println!("nicdev: FAILURE — a card without MSI/MSI-X was not recognised"); ok = false; }
            // The priority: if there are both, MSI-X is taken.
            let mut d_both = alveo; d_both.msix_cap = 0x70; d_both.msi_cap = 0x50;
            if crate::pci::intr_kind(&d_both) != "MSI-X" { println!("nicdev: FAILURE — with both capabilities something other than MSI-X was chosen");
                ok = false; }
            println!("nicdev: the classification of the interrupts: MSI-X / MSI / INTx / both -> MSI-X");

            // 2) A NEW DRIVER THROUGH THE TABLE. We bring the loopback card up by the same path
            //    by which a real one comes up: `probe_all` over the table.
            crate::netdev::force("loop");
            match crate::netdev::probe_all() {
                None => { println!("nicdev: FAILURE — the loopback card did not come up through the table"); ok = false; }
                Some((dev, mac)) => {
                    let mut q = crate::l1::nicq(dev);
                    println!("nicdev: under the rings the driver is '{}', mac={:02x}:{:02x}", q.drv(), mac[0], mac[1]);
                    if q.drv() != "loop" { println!("nicdev: FAILURE — the table gave out the wrong driver"); ok = false; }
                    // 3) THE DATAPATH over an unfamiliar card: the same code that drives
                    //    virtio and ENA — send, take back, compare the bytes.
                    const N: usize = 32;
                    let mut sent = 0usize;
                    let mut got = 0usize;
                    let mut bad = 0usize;
                    for i in 0..N {
                        let s = match q.tx_alloc() { Some(s) => s, None => { q.reap_tx(); continue } };
                        let off = q.tx_hdr(s);
                        let b = q.tx_buf(s);
                        for k in 0..64 { b[off + k] = (i * 7 + k) as u8; }
                        q.tx_send(s, off + 64);
                        sent += 1;
                        q.flush();
                        q.reap_tx();
                        while let Some((slot, ro, len)) = q.poll_rx() {
                            let r = q.rx_buf(slot);
                            let mut mism = len != off + 64;
                            for k in 0..64 { if r[ro + off + k] != (got * 7 + k) as u8 { mism = true; break; } }
                            if mism { bad += 1; }
                            got += 1;
                            q.rx_done(slot);
                        }
                    }
                    let (looped, dropped) = crate::nicloop::counters();
                    println!("nicdev: sent {}, received {}, mismatches {}, the loop looped back {}, dropped {}",
                        sent, got, bad, looped, dropped);
                    if sent != N || got != N || bad != 0 || dropped != 0 {
                        println!("nicdev: FAILURE — the datapath over the new driver did not add up"); ok = false;
                    }
                    if q.db_writes().is_none() { println!("nicdev: FAILURE — the counter of the driver's doorbells is not visible"); ok = false; }
                }
            }
            println!("nicdev: {}", if ok { "ALL OK" } else { "FAILURE" });
            crate::suite::park()
        }
        // §6.3: the partitioning of the L3 (CAT) + the footprint of the DMA rings against the L3. On
        // a stand without CAT (a KVM guest, a dev chip) the hardware part is skipped, and what
        // is checked is what otherwise is not checked at all: the validation of the masks — its
        // violation gives a #GP on live hardware — and the arithmetic of the rings' footprint.
        "cat" => {
            let mut ok = crate::cat::selftest();
            // The whole layout, as from the manifest: a pool of cores -> a share of the cache.
            let mut m = crate::cellman::CellManifest::empty();
            m.add_pool_spec(b"fast:1,2");
            m.add_llc_spec(b"fast:0xf0");
            m.add_llc_spec(b"nosuchpool:0xf");   // a reference into nowhere — it is bound to be dropped
            m.add_llc_spec(b"*:0x0f0f");         // a mask with a hole — it is bound to be dropped by the hardware
            println!("cat-test: llc= rules parsed {} (broken lines {})", m.llcc, m.pin_bad);
            // The whole path end to end: the manifest -> a pool of cores -> a share of the cache -> the hardware.
            // What is checked is not the parsing but the APPLICATION (pitfall A3.3: for three sessions the test
            // confirmed that the number had reached the structure, not that it had worked).
            crate::affinity::configure(&m);
            if m.llcc != 3 { println!("cat-test: FAILURE — the llc= rules were parsed wrongly"); ok = false; }
            if m.llc(0).who() != b"fast" || m.llc(0).mask != 0xf0 {
                println!("cat-test: FAILURE — the rule llc=fast:0xf0 was parsed wrongly"); ok = false;
            }
            // A mask with a hole is bound to be rejected BEFORE the write into the MSR — at the length
            // of the mask of any real chip.
            if crate::cat::mask_valid(0x0f0f, 20) { println!("cat-test: FAILURE — a mask with a hole was deemed fit"); ok = false; }
            println!("cat-test: {}", if ok { "ALL OK" } else { "FAILURE" });
            crate::suite::park()
        }
        // §4.4: the device's clock (PHC) — a reference shared with the neighbour in the rack. Without it
        // the NTP correction by construction halves the RTT and there is nothing to separate RX/TX with.
        "phc" => {
            crate::ena::WANT_PHC.store(true, Ordering::SeqCst);
            // Here it is ENA that is needed (the clock is its feature), so we take the driver
            // directly rather than through the table: the registry gives out a `dyn NetDev`, which has
            // no PHC and must not have one.
            match crate::ena::probe() {
                Some(mut e) => {
                    if !e.phc_ok { println!("phc-test: the device's clock is unavailable — SKIP (an instance type without a PHC)"); }
                    else {
                        let hz = crate::tsc::calibrate_cached();
                        let mut prev = (0u64, 0u64);
                        let mut ok = true;
                        for i in 0..5 {
                            let t0 = crate::tsc::rdtsc();
                            let p = e.phc_read();
                            let t1 = crate::tsc::rdtsc();
                            match p {
                                Some(ns) => {
                                    let local = crate::tsc::mono_ns();
                                    let cost = (t1 - t0) * 1_000_000_000 / hz;
                                    // The device's clock is the REFERENCE for the wall clock: on hardware
                                    // there is no kvmclock, and it is the only source by
                                    // which the discipline can be run at all. We feed it
                                    // from here rather than from the tick: a PHC read is a doorbell into the
                                    // device and a poll of the answer, and there is no place for that in an interrupt
                                    // handler (pitfall A12).
                                    crate::tsc::feed_ref(ns, t1, crate::tsc::SRC_PHC);
                                    println!("phc-test: #{} phc={} ns, local {} ns, the read {} ns", i, ns, local, cost);
                                    if prev.0 != 0 {
                                        let d_phc = ns.wrapping_sub(prev.0);
                                        let d_loc = local.wrapping_sub(prev.1);
                                        let lo = d_loc - d_loc / 100; let hi = d_loc + d_loc / 100;
                                        println!("phc-test:    the step phc={} ns, local={} ns", d_phc, d_loc);
                                        if d_phc < lo || d_phc > hi { ok = false; }
                                    }
                                    prev = (ns, local);
                                }
                                None => { println!("phc-test: #{} a read timeout", i); ok = false; }
                            }
                            let t = crate::tsc::rdtsc(); while crate::tsc::rdtsc() - t < hz / 100 { core::hint::spin_loop(); }
                        }
                        let (ppm, err, adj, steps, back, feeds) = crate::tsc::wall_stats();
                        println!("phc-test: the wall-clock discipline by the PHC: samples {}, adjustments {}, steps {}, backwards {}, the correction {} ppm, \
                            the error {} ns",
                            feeds, adj, steps, back, ppm, err);
                        if back != 0 { println!("phc-test: FAILURE — the wall clock went backwards"); ok = false; }
                        println!("phc-test: {}", if ok { "ALL OK" } else { "FAILURE" });
                    }
                }
                None => println!("phc-test: ENA was not found — SKIP"),
            }
            crate::suite::park()
        }
        "pci" => {
            let (b, s) = crate::pci::ivshmem();
            if b != 0 { // write a marker into the host's shared memory and read it back
                let k = crate::arch::mmio_va(b); // ivshmem() is the physical address (ring 3 gets it)
                unsafe { core::ptr::write_volatile(crate::mem::devptr::<u64>(k), 0x5441_4C41_5249_4131); }
                let v = unsafe { core::ptr::read_volatile(crate::mem::devptr::<u64>(k + 8)) };
                println!("pci-test: ivshmem base={:#x} size={} the marker was written, host@8={:#x}", b, s, v);
            } else { println!("pci-test: ivshmem was not found"); }
            crate::suite::park()
        }
        // §A1: write-combining. A local gate for the PAT slot and the change of a page's type —
        // there is nothing to check the LLQ datapath itself with under QEMU (there is no ENA), but the burning of the PAT,
        // the splitting of a leaf down to 4K and the granularity ("the neighbouring page stayed UC")
        // are checked on any machine.
        "wc" => {
            let mut ok = true;
            let pat = crate::paging::pat_read();
            println!("wc-test: IA32_PAT={:#x} (we expected {:#x})", pat, crate::paging::PAT_VALUE);
            if pat != crate::paging::PAT_VALUE { ok = false; }
            match crate::mem::alloc_contig(2) {
                Some(p) => {
                    // the neighbouring page is the control of the granularity: its type must not change
                    let before = crate::paging::leaf_entry(p + 0x1000).unwrap_or(0);
                    if !crate::paging::map_mmio_wc(p, 4096) { println!("wc-test: map_mmio_wc refused"); ok = false; }
                    let e = crate::paging::leaf_entry(p).unwrap_or(0);
                    let wc = e & (1 << 7) != 0 && e & (1 << 3) == 0 && e & (1 << 4) == 0;
                    println!("wc-test: PTE {:#x} -> PAT={} PWT={} PCD={} ({})",
                        e, (e >> 7) & 1, (e >> 3) & 1, (e >> 4) & 1, if wc { "write-combining" } else { "NOT WC" });
                    if !wc { ok = false; }
                    // The granularity: the neighbouring page lives in the same 2M leaf that
                    // had to be split. Its type is bound to stay the same — the PAT is cleared,
                    // and the PCD/PWT are the same as the parent's (the PAT bit in a 2M leaf has
                    // the number 12, in a 4K one it is 7, so we compare by meaning rather than by a mask).
                    let after = crate::paging::leaf_entry(p + 0x1000).unwrap_or(0);
                    if after & (1 << 7) != 0 || after & 0b1_1000 != before & 0b1_1000 || before & (1 << 12) != 0 {
                        println!("wc-test: the neighbouring page changed its type: {:#x} -> {:#x}", before, after); ok = false;
                    }
                    // a write with a stream of 8-byte stores and a read back (the WC buffer is flushed by a fence)
                    // DMAP: a RAM frame that map_mmio_wc re-typed WC AT ITS IDENTITY ADDRESS — read as a device window
                    unsafe {
                        for i in 0..8u64 { core::ptr::write_volatile(crate::mem::devptr::<u64>(p + i * 8), 0x1122_3344_0000_0000 | i); }
                        core::sync::atomic::fence(Ordering::SeqCst);
                        for i in 0..8u64 {
                            let v = core::ptr::read_volatile(crate::mem::devptr::<u64>(p + i * 8));
                            if v != (0x1122_3344_0000_0000 | i) { println!("wc-test: the word {} = {:#x}", i, v); ok = false; }
                        }
                    }
                    println!("wc-test: 64 B were written and read back");
                }
                None => { println!("wc-test: there is no memory"); ok = false; }
            }
            println!("wc-test: {}", if ok { "ALL OK" } else { "FAILURE" });
            crate::suite::park()
        }
        // PLAN-V1.6 §2: the screen as a second output channel. The self-test reads the buffer
        // back — under QEMU that is the only available judge (that the metal firmware
        // redirects the screen cannot be checked locally by anything).
        "vga" => {
            let mut ok = true;
            crate::vga::enable();
            println!("VGA-PROBE-MARK-1");                   // it will settle in the first line
            if !crate::vga::contains(b"VGA-PROBE-") { println!("vga-test: the line is not in the screen's buffer"); ok = false; }
            else { println!("vga-test: the line was found in the screen's buffer"); }
            // The scrolling: the screen is 25 lines, we print 40 — the top ones are bound to go away, and
            // the last printed one is bound to end up in the BOTTOM line.
            for i in 0..40 { println!("VGA-SCROLL-{}", i); }
            let last_ok = crate::vga::row_contains(24, b"VGA-SCROLL-39") || crate::vga::row_contains(23, b"VGA-SCROLL-39");
            let gone = !crate::vga::contains(b"VGA-PROBE-");
            println!("vga-test: the last line is at the bottom={} the top ones went away={}", last_ok, gone);
            if !last_ok || !gone { ok = false; }
            println!("vga-test: {}", if ok { "ALL OK" } else { "FAILURE" });
            crate::suite::park()
        }
        "printf" => {
            static MP: &[u8] = user_elf!("mprintf");
            println!("talaria: the musl printf/malloc ELF, {} bytes (writev/mmap/brk/ioctl)", MP.len());
            crate::user::run(MP);
        }
        "manifest" => {
            println!("talaria: the self-test of the parser of the cell's manifest");
            crate::cellman::selftest();
        }
        "tuning" => {
            println!("talaria: the self-test of the APPLICATION of the measurement settings from the manifest");
            crate::cellman::selftest_tuning();
        }
        "its" => {
            // arm64: a device's MSI-X vector through the ITS to a chosen core (talaria.pci=1).
            crate::smp::start_from_cmdline(cmdline);
            crate::arch::msi_selftest()
        }
        "msix" => {
            // The routing of the devices' interrupts: the message's address is written into the table
            // and read BACK. The parsing of PCI is needed, so the mode requires talaria.pci=1.
            println!("talaria: the self-test of MSI-X routing (where the devices' interrupts arrive)");
            crate::affinity::selftest_msix();
        }
        "console" => {
            crate::console::selftest();
        }
        "wakeprobe" => {
            // What a store and an SGI cost to wake a sleeping core, under wfe and under wfi
            // (`arch::smp::wake_probe`; docs/ENGINEERING-NOTES.md item 47).
            #[cfg(target_arch = "aarch64")]
            crate::smp::wake_probe();
            #[cfg(not(target_arch = "aarch64"))]
            println!("wakeprobe: arm64 only (x86 has no wfe tier to probe)");
            crate::suite::park()
        }
        "dirtyprot" => {
            // A WRITTEN page that is then write-protected is still written: `protect` rebuilt
            // the leaf without D on both machines, and arm64 kept D in AP[2], which a read-only
            // page needs for itself (Linux's ff1712f953e2; Redis refuses to start on a kernel
            // that loses it). An ext2 MAP_SHARED page written, mprotect(PROT_READ)ed and synced
            // was skipped by the writeback.
            let ok = match crate::mem::alloc_frame() {
                None => false,
                Some(f) => {
                    crate::paging::split_to_4k(f);
                    crate::paging::protect(f, 4096, true, false);
                    while crate::paging::take_dirty(f) {}               // clean, as the writeback leaves it
                    // the page under test is the LEAF at virtual address `f` (its identity), so the
                    // write goes there: through `kptr` it lands on the direct map's leaf, whose D
                    // bit nobody reads (the probe failed so on both machines when the map came)
                    unsafe { core::ptr::write_volatile(crate::mem::vptr::<u64>(f), 0x5a5a); }
                    crate::paging::protect(f, 4096, false, false);      // mprotect(PROT_READ) after the write
                    let kept = crate::paging::take_dirty(f);
                    let cleared = !crate::paging::take_dirty(f);        // and the take still clears it
                    crate::paging::protect(f, 4096, true, false);
                    println!("dirtyprot: a written page write-protected: dirty {}, after the take {}", kept, !cleared);
                    kept && cleared
                }
            };
            println!("dirtyprot: {}", if ok { "OK — the write survives the protection" } else { "FAIL — the dirty bit was lost" });
        }
        "pmu" => {
            let have = crate::pmu::available(); // the counters are up from the boot — a second init only makes noise in the log
            let iters: u64 = 20_000_000;
            let (c0, i0) = crate::pmu::read();
            let t0 = crate::tsc::rdtsc();
            let mut acc: u64 = 0x9E37_79B9_7F4A_7C15;
            let mut k = 0u64;
            while k < iters { acc = acc.wrapping_mul(6364136223846793005).wrapping_add(k); k += 1; }
            let t1 = crate::tsc::rdtsc();
            let (c1, i1) = crate::pmu::read();
            core::hint::black_box(acc);
            let (cycles, source) = if have && c1 > c0 { (c1 - c0, "rdpmc") } else { (t1 - t0, "tsc") };
            if have && c1 > c0 {
                let cyc = c1 - c0; let ins = i1 - i0;
                let ipc100 = if cyc > 0 { ins * 100 / cyc } else { 0 };
                println!("pmu: {} iterations (rdpmc): cycles={} instrs={} IPC={}.{:02}",
                    iters, cyc, ins, ipc100 / 100, ipc100 % 100);
            } else {
                println!("pmu: {} iterations (a TSC fallback): cycles~{}", iters, cycles);
            }
            // The cache trace: walk 16 MB with a step of 64 bytes x4 -> LLC references/misses.
            let (m0, r0) = crate::pmu::read_cache();
            if let Some(bufp) = crate::mem::alloc_contig(4096) { // 16 MB
                let p = crate::mem::kptr::<u8>(bufp); let sz = 4096u64 * 4096; let mut sink = 0u64;
                for _pass in 0..4u64 {
                    let mut o = 0u64;
                    while o < sz { sink = sink.wrapping_add(unsafe { core::ptr::read_volatile(p.add(o as usize)) } as u64); o += 64; }
                }
                core::hint::black_box(sink);
                crate::mem::free_contig(bufp, 4096);
            }
            let (m1, r1) = crate::pmu::read_cache();
            println!("pmu: LLC refs={} misses={} (memory 16 MB x4 passes)", r1.wrapping_sub(r0), m1.wrapping_sub(m0));
            // The branch predictor: the fourth architectural counter. Without it
            // "the IPC is low" cannot be told apart from "a data-dependent branch in a hot loop".
            let (bm, brt) = crate::pmu::read_branch();
            println!("pmu: branches={} predictor misses={} (GP counters {}, fixed {})",
                brt, bm, crate::pmu::ngp(), crate::pmu::nfixed());
            if let Some((ins, cyc)) = crate::pmu::selftest_wrap() {
                let ok = ins > 1_000_000 && ins < 1 << 31;
                println!("pmu: across the 32-bit wrap: instrs={} cycles={} — {}", ins, cyc,
                    if ok { "the widening holds" } else { "FAIL (the widening missed the wrap)" });
            }
            crate::pmu::selftest_idle();
            crate::pmu::selftest_chain();
            crate::pmu::selftest_events();
            if cycles > 0 { println!("pmu: OK (cycles={} source={})", cycles, source); }
            else { println!("pmu: FAIL (a zero counter)"); }
            crate::suite::park()
        }
        "serial" => {
            // A12: A PORT THAT IS NOT THERE must not be taken for a source of data.
            // On a port that does not exist the bus floats and ALL the registers read as 0xFF —
            // that is, the bit "a byte is ready" in the LSR is always set. The console's poll spun
            // without a bound and was called from the tick's handler: on a `c6i.metal` (there is no COM1
            // there) THE VERY FIRST tick of the scheduler went into an infinite loop INSIDE
            // the interrupt, and from the outside that looked like "the workload is silent".
            // At home there is nothing to reproduce a machine without COM1 with — but one can ask a port
            // that knowingly does not exist on this machine (COM4, 0x2E8).
            const DEAD: u16 = 0x2E8;
            let lsr = unsafe { crate::serial::inb(DEAD + 5) };
            println!("serial-test: the LSR of the absent port {:#x} = {:#x}", DEAD, lsr);
            let mut ok = true;
            if lsr == 0xFF {
                if crate::serial::port_has_data(DEAD) {
                    println!("serial-test: FAILURE — a floating bus was taken for data");
                    ok = false;
                } else {
                    println!("serial-test: a floating bus (0xFF) was NOT taken for data");
                }
            } else {
                println!("serial-test: the port {:#x} answers on this machine — the logic cannot be checked on it", DEAD);
            }
            // A live port with no input is also bound to say "there is no data":
            // otherwise the console's poll would spin idly on every tick.
            if crate::serial::getc().is_some() {
                println!("serial-test: there is unexpectedly a byte in COM1's input register (is somebody typing into the console?)");
            }
            // And the console's poll itself is bound to RETURN: the body of an interrupt handler is
            // not entitled to spin without a bound under any data.
            for _ in 0..1000 { crate::console::poll_tick(); }
            println!("serial-test: the console's poll returns (1000 calls, the loop's bound is alive)");
            println!("serial-test: {}", if ok { "ALL OK" } else { "THERE ARE FAILURES" });
            crate::suite::park()
        }
        "timer" => {
            // A12: A SELF-SUFFICIENT probe of the LAPIC timer. It answers the question that
            // was the last one left on bare metal: do the timer's interrupts arrive AT ALL,
            // in the kernel, without a workload and without the scheduler. Everything that had been measured until then went
            // through the production path, and "there are no interrupts" could mean both "the timer does not
            // tick" and "there is nobody to take them".
            println!("timer-test: I am programming the periodic LAPIC timer and waiting a second with the interrupts open");
            crate::smp::lapic_enable();
            crate::idt::init(); // the regular timer handler (it counts crate::idt::TICKS and sends an EOI)
            let base = crate::idt::TICKS.load(Ordering::SeqCst);
            crate::smp::lapic_timer_init(crate::smp::SLICE_NS);
            let hz = crate::tsc::calibrate_cached();
            let t0 = crate::tsc::rdtsc();
            unsafe { crate::arch::cpu::irq_enable(); }
            let mut last = 0u64;
            while crate::tsc::rdtsc() - t0 < hz {          // exactly a second
                let n = crate::idt::TICKS.load(Ordering::Relaxed) - base;
                if n != last {
                    last = n;
                    println!("timer-test: interrupt {} at {} ms (current={} IRR={} ISR={})",
                        n, (crate::tsc::rdtsc() - t0) * 1000 / hz, crate::smp::lapic_peek(0x390),
                        crate::smp::irr_pending(crate::idt::TIMER_VECTOR as u8), crate::smp::isr_pending(crate::idt::TIMER_VECTOR as u8));
                    if n >= 5 { break; }
                }
                core::hint::spin_loop();
            }
            unsafe { crate::arch::cpu::irq_disable(); }
            let n = crate::idt::TICKS.load(Ordering::SeqCst) - base;
            println!("timer-test: over a second interrupts {} — LVT={:#x} initial={} current={} IRR={} ISR={}",
                n, crate::smp::lapic_peek(0x320), crate::smp::lapic_peek(0x380), crate::smp::lapic_peek(0x390),
                crate::smp::irr_pending(crate::idt::TIMER_VECTOR as u8), crate::smp::isr_pending(crate::idt::TIMER_VECTOR as u8));
            if n > 0 { println!("timer-test: ALL OK — the timer interrupts the core"); }
            else if crate::smp::irr_pending(crate::idt::TIMER_VECTOR as u8) {
                println!("timer-test: FAILURE — the interrupt IS WAITING in the IRR but is not accepted (IF/TPR)");
            } else {
                println!("timer-test: FAILURE — the timer counts but does not raise interrupts");
            }
            crate::suite::park()
        }
        "crypto" => {
            println!("AES-128-GCM self-test (NIST): {}", if crate::crypto::selftest() {"PASS"} else {"FAIL"});
            let n = 1_000_000u64;
            let t0 = crate::tsc::rdtsc();
            unsafe {
                let aes = crate::crypto::Aes128::new(&[0x42u8; 16]);
                let mut buf = [0u8; 256];
                for i in 0..n { buf[0] = i as u8; aes.seal_inplace(&[0u8;12], &[0u8;13], &mut buf, 96); core::hint::black_box(&buf); }
            }
            let cyc = (crate::tsc::rdtsc() - t0) / n;
            let hz = crate::tsc::calibrate_cached();
            println!("AES-128-GCM seal in-place (96 B): ~{} cycles ({} ns) per order — the crypto datapath on a bare core",
                cyc, cyc * 1_000_000_000 / hz);
        }
        "fault" => {
            println!("talaria: the #PF + flight-recorder test — an access to 0xdead0000...");
            crate::fr::record(crate::fr::NOTE, 0xdead_0000_0000);
            unsafe { core::ptr::read_volatile(crate::mem::vptr::<u64>(0xdead_0000_0000)); }
            println!("talaria: (this must not be printed)");
        }
        _ => return false,
    }
    true
}
