mod vfio;
mod dma;
mod ixgbe;
mod reflect;
mod shm;
mod resctrl;
mod proxy;
mod pmu;
mod spsc;
mod l1pipe;
mod crypto;
mod trade;

use dma::HugeRegion;
use ixgbe::Ixgbe;
use std::time::Duration;
use vfio::VfioDevice;

#[inline(always)]
fn rdtsc() -> u64 { unsafe { std::arch::x86_64::_rdtsc() } }

fn tsc_hz() -> u64 {
    let t0 = rdtsc();
    std::thread::sleep(Duration::from_millis(50));
    let t1 = rdtsc();
    (t1 - t0) * 20
}



struct Flags { core: Option<usize>, fifo: bool, status: Option<String>, stop: Option<String>, shm: Option<String>, llc_mask: Option<u64>, crash_after: u64, proxy: Option<String>, gate: Option<String>, numa: Option<usize>, pipeline: Option<usize>, l1: bool, trade: bool, ccore: Option<usize> }
fn parse_flags(a: &[String]) -> Flags {
    let mut f = Flags { core: None, fifo: false, status: None, stop: None, shm: None, llc_mask: None, crash_after: 0, proxy: None, gate: None, numa: None, pipeline: None, l1: false, trade: false, ccore: None };
    let mut i = 0;
    while i < a.len() {
        match a[i].as_str() {
            "--core" => { f.core = a.get(i+1).and_then(|x| x.parse().ok()); i += 1; }
            "--fifo" => f.fifo = true,
            "--status" => { f.status = a.get(i+1).cloned(); i += 1; }
            "--stop" => { f.stop = a.get(i+1).cloned(); i += 1; }
            "--shm" => { f.shm = a.get(i+1).cloned(); i += 1; }
            "--llc-mask" => { f.llc_mask = a.get(i+1).and_then(|x| u64::from_str_radix(x.trim_start_matches("0x"),16).ok()); i += 1; }
            "--crash-after" => { f.crash_after = a.get(i+1).and_then(|x| x.parse().ok()).unwrap_or(0); i += 1; }
            "--proxy" => { f.proxy = a.get(i+1).cloned(); i += 1; }
            "--gate" => { f.gate = a.get(i+1).cloned(); i += 1; }
            "--numa" => { f.numa = a.get(i+1).and_then(|x| x.parse().ok()); i += 1; }
            "--pipeline" => { f.pipeline = a.get(i+1).and_then(|x| x.parse().ok()); i += 1; }
            "--l1" => { f.l1 = true; }
            "--trade" => { f.trade = true; }
            "--ccore" => { f.ccore = a.get(i+1).and_then(|x| x.parse().ok()); i += 1; }
            _ => {}
        }
        i += 1;
    }
    f
}
fn pin_self(core: usize, fifo: bool) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(core, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        if fifo {
            let p = libc::sched_param { sched_priority: 80 };
            libc::sched_setscheduler(0, libc::SCHED_FIFO, &p);
        }
    }
}

fn voluntary_ctxt_switches() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("voluntary_ctxt_switches:") {
            return v.trim().parse().unwrap_or(0);
        }
    }
    0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("link");
    let flags = parse_flags(&args[2.min(args.len())..]);
    if let Some(c) = flags.core { pin_self(c, flags.fifo); }
    let mut llc_applied = 0u64;
    if let Some(mask) = flags.llc_mask {
        if !resctrl::available() {
            println!("resctrl: not mounted — the LLC mask was NOT applied \
(needs root and `mount -t resctrl resctrl /sys/fs/resctrl`)");
        } else {
            match resctrl::setup("talaria-cell", mask) {
                Ok(()) => { llc_applied = mask; println!("resctrl: LLC mask {:#x} applied", mask); }
                Err(e) => println!("resctrl: could not apply LLC ({e})"),
            }
        }
    }
    // The PCI address and the VFIO group are configurable through env (for a turnkey run on hardware)
    let pci = std::env::var("TALARIA_PCI").unwrap_or_else(|_| "0000:03:00.0".to_string());
    let group: u32 = std::env::var("TALARIA_VFIO_GROUP").ok().and_then(|s| s.parse().ok()).unwrap_or(0);

    if mode == "crypto" {
        println!("AES-128-GCM self-test (NIST TC2): {}", if crypto::selftest() {"PASS"} else {"FAIL"});
        let aes = crypto::Aes128::new(&[0x42u8; 16]);
        let iv = [0u8; 12];
        let aad = [0u8; 13]; // a TLS-like record header
        let mut buf = [0u8; 256];
        let order_len = 96usize; // a typical order is ~96 B
        let hz = tsc_hz();
        let n = 2_000_000u64;
        let t0 = rdtsc();
        for i in 0..n { buf[0] = i as u8; aes.seal_inplace(&iv, &aad, &mut buf, order_len); std::hint::black_box(&buf); }
        let dt = rdtsc() - t0;
        let ns = dt as f64 / n as f64 / hz as f64 * 1e9;
        println!("AES-128-GCM seal in-place ({} B order): {:.1} ns/order ({:.2} GB/s)",
            order_len, ns, order_len as f64 / (ns * 1e-9) / 1e9);
        return;
    }
    let dev = VfioDevice::open(&pci, group).expect("vfio open");
    let (vid, did) = dev.read_ids();
    assert_eq!(vid, 0x8086);
    println!("82599 {pci}: device={did:#06x}");
    let bar = dev.map_bar0().expect("bar0");

    let mut mem = HugeRegion::alloc_1g(flags.numa).expect("hugepage 1G");
    println!("DMA: 1G hugepage phys={:#x} numa_req={:?} numa_actual={:?}",
        mem.phys, flags.numa, mem.actual_node());

    let mut nic = Ixgbe::init(&bar, &mut mem);
    let m = nic.mac();
    println!("mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0],m[1],m[2],m[3],m[4],m[5]);

    print!("waiting for link... ");
    if nic.wait_link(Duration::from_secs(8)) {
        println!("UP (10G)");
    } else {
        println!("DOWN — check the AOC cable and that f1 is up");
    }

    if mode == "link" {
        return;
    }

    if let Some(bcore) = flags.pipeline {
        let rs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
        let hz = tsc_hz();
        if flags.trade {
            let ccore = flags.ccore.unwrap_or(bcore + 1);
            run_trade(nic, m, hz, bcore, ccore, &flags, rs);
        }
        run_pipeline(nic, m, hz, bcore, &flags, rs);
    }

    // A Talaria cell on a real 82599: a run-to-completion reactor,
    // zero syscalls/allocations on the hot path, TSC telemetry — as in the no_std kernel.
    let run_secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let hz = tsc_hz();
    let shm = flags.shm.as_ref().and_then(|p| shm::ShmWriter::create(p).ok());
    if let Some(w) = &shm { unsafe { (*w.p).pid = std::process::id(); } }
    let prox = flags.proxy.as_ref().and_then(|p| proxy::ProxyWriter::create(p).ok());
    // the donation "gate": the cell sets 1 (busy) on a packet, 0 after a short idle period
    let gate: *mut u32 = flags.gate.as_ref().map(|path| unsafe {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).unwrap();
        f.set_len(64).unwrap();
        let p = libc::mmap(std::ptr::null_mut(), 64, libc::PROT_READ|libc::PROT_WRITE, libc::MAP_SHARED, f.as_raw_fd(), 0);
        assert_ne!(p, libc::MAP_FAILED); p as *mut u32
    }).unwrap_or(std::ptr::null_mut());
    let mut gate_state = 0u32;
    let gate_idle_thresh = hz / 200_000; // ~5 µs of idling -> give the core to the batch job
    if let Some(px) = &prox {
        // a demonstration of the synchronous syscall proxy: ask the CP to read a file
        let host = px.readfile("/etc/hostname");
        px.log(&format!("cell up: pid={} host={} (the file was read through the syscall proxy)",
            std::process::id(), String::from_utf8_lossy(&host).trim()));
    }
    let t_start = rdtsc();
    let pmu = pmu::Pmu::new();
    let mut dtlb0 = pmu.dtlb.read();
    let mut llc0 = pmu.llc.read();
    println!("cell :7777, ip 10.0.0.2, tsc={} Hz, PMU={}.", hz,
        if pmu.dtlb.available() && pmu.llc.available() {"on"} else {"off"});
    let interval = 2 * hz;
    let gap_thresh = hz / 100_000; // 10 us
    let mut pkts: u64 = 0;
    let mut c_sum = 0u64; let mut c_min = u64::MAX; let mut c_max = 0u64;
    let mut hist = [0u64; 6]; // <100 <200 <400 <800 <1600 >=1600 ns
    let mut gap_max = 0u64; let mut gap_over = 0u64;
    let mut last = rdtsc();
    let mut prev = last;
    let mut last_pkt = last;
    let deadline = if run_secs > 0 { rdtsc() + run_secs * hz } else { 0 };
    let _ = voluntary_ctxt_switches; // not called in steady state

    loop {
        if deadline != 0 && rdtsc() >= deadline {
            let ns = |c: u64| if pkts>0 { c / hz.max(1) } else { 0 };
            let _ = ns;
            println!("cell: total pkts={} (for the run)", pkts);
            return;
        }
        let now = rdtsc();
        let gap = now - prev;
        if gap > gap_max { gap_max = gap; }
        if gap > gap_thresh { gap_over += 1; }
        prev = now;

        let mut t0 = 0u64;
        let got = nic.poll_echo(|frame| { t0 = rdtsc(); reflect::reflect(frame, &m) });
        if got {
            // a packet arrived -> take the core away from the batch job (set "busy" on the transition)
            if !gate.is_null() && gate_state == 0 { unsafe { std::ptr::write_volatile(gate, 1); } gate_state = 1; }
            last_pkt = now;
        } else if !gate.is_null() && gate_state == 1 && now - last_pkt > gate_idle_thresh {
            // idle -> give the core to the batch job
            unsafe { std::ptr::write_volatile(gate, 0); } gate_state = 0;
        }
        if got && t0 != 0 {
            if let Some(ref w) = shm { w.bump_pkts(); }
            let c = rdtsc() - t0;
            let cns = c * 1_000_000_000 / hz;
            pkts += 1; c_sum += c;
            if c < c_min { c_min = c; }
            if c > c_max { c_max = c; }
            let b = match cns { 0..=99=>0, 100..=199=>1, 200..=399=>2, 400..=799=>3, 800..=1599=>4, _=>5 };
            hist[b]+=1;
        }

        if now - last >= interval {
            let ns = |c: u64| c * 1_000_000_000 / hz;
            let dvcs = 0u64; // vcs is now measured by the control plane from outside
            let dtlb_now = pmu.dtlb.read(); let llc_now = pmu.llc.read();
            let (dtlb_mpk, llc_mpk) = if pkts > 0 {
                ((dtlb_now - dtlb0) * 1000 / pkts, (llc_now - llc0) * 1000 / pkts)
            } else { (0, 0) };
            dtlb0 = dtlb_now; llc0 = llc_now;
            if let Some(ref w) = shm {
                let up = nic.link_up();
                w.publish(up, m, if pkts>0 {ns(c_sum/pkts)} else {0}, if pkts>0 {ns(c_min)} else {0},
                    ns(c_max), if pkts>0 {hist[0]*100/pkts} else {0}, dvcs, gap_over,
                    llc_applied, (rdtsc()-t_start)/hz, dtlb_mpk, llc_mpk);
            }
            if flags.crash_after != 0 && (rdtsc()-t_start)/hz >= flags.crash_after {
                eprintln!("cell: artificial crash (--crash-after)"); std::process::exit(2);
            }
            if pkts > 0 {
                let line = format!("cell: pkts={} rx->tx ns min/avg/max={}/{}/{} <100ns={}% gap_max_ns={} gaps>10us={} dTLB-miss/kpkt={} LLC-miss/kpkt={}",
                    pkts, ns(c_min), ns(c_sum/pkts), ns(c_max),
                    hist[0]*100/pkts, ns(gap_max), gap_over, dtlb_mpk, llc_mpk);
                if let Some(px) = &prox { px.log(&line); } else { println!("{line}"); }
            }
            if let Some(ref stop) = flags.stop {
                if std::path::Path::new(stop).exists() {
                    let _ = std::fs::remove_file(stop);
                    if let Some(ref p) = flags.shm { let _ = std::fs::remove_file(p); }
                    println!("cell: stop received, exiting");
                    return;
                }
            }
            pkts=0; c_sum=0; c_min=u64::MAX; c_max=0; hist=[0;6]; gap_max=0; gap_over=0;
            last = rdtsc(); prev = last;
        }
    }
}


/// A two-stage datapath: shard A (RX/TX) <-SPSC-> shard B (reflect) on different
/// cores, zero-copy (only a buffer index travels between the shards).
fn run_trade(mut nic: ixgbe::Ixgbe, m: [u8;6], hz: u64, bcore: usize, ccore: usize, flags: &Flags, run_secs: u64) -> ! {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let a2b = Arc::new(spsc::Spsc::new(2048));
    let b2c = Arc::new(spsc::Spsc::new(2048));
    let c2a = Arc::new(spsc::Spsc::new(2048));
    let bufs = Arc::new(nic.buf_addrs());
    let stop = Arc::new(AtomicBool::new(false));

    // stage B: strategy + risk (builds the order packet in place)
    let (a, o, bf, st) = (a2b.clone(), b2c.clone(), bufs.clone(), stop.clone());
    std::thread::spawn(move || {
        pin_self(bcore, true);
        let mut seq = 0u64;
        while !st.load(Ordering::Relaxed) {
            if let Some(v) = a.pop() {
                let b = (v & 0xffff_ffff) as u32; let len = (v >> 32) as usize;
                let frame = unsafe { std::slice::from_raw_parts_mut(bf[b as usize] as *mut u8, 2048) };
                seq += 1;
                let out = trade::strategy_risk(frame, len, seq).unwrap_or(0);
                while !o.push(((out as u64) << 32) | b as u64) { std::hint::spin_loop(); }
            } else { std::hint::spin_loop(); }
        }
    });
    // stage C: crypto (AES-GCM of the order in place)
    let (o2, c, bf2, st2) = (b2c.clone(), c2a.clone(), bufs.clone(), stop.clone());
    std::thread::spawn(move || {
        pin_self(ccore, true);
        let aes = crypto::Aes128::new(&[0x42u8; 16]);
        let mut nonce = 0u64;
        while !st2.load(Ordering::Relaxed) {
            if let Some(v) = o2.pop() {
                let b = (v & 0xffff_ffff) as u32; let fl = (v >> 32) as usize;
                if fl > 0 {
                    nonce += 1;
                    let mut iv = [0u8; 12]; iv[4..].copy_from_slice(&nonce.to_be_bytes());
                    let payload = unsafe { std::slice::from_raw_parts_mut(
                        (bf2[b as usize] as *mut u8).add(trade::ORDER_OFF), trade::ORDER_LEN + trade::TAG) };
                    aes.seal_inplace(&iv, &[], payload, trade::ORDER_LEN);
                }
                while !c.push(((fl as u64) << 32) | b as u64) { std::hint::spin_loop(); }
            } else { std::hint::spin_loop(); }
        }
    });

    let shm = flags.shm.as_ref().and_then(|p| shm::ShmWriter::create(p).ok());
    if let Some(w) = &shm { unsafe { (*w.p).pid = std::process::id(); } }
    let pmu = pmu::Pmu::new();
    let mut dtlb0 = pmu.dtlb.read(); let mut llc0 = pmu.llc.read();
    println!("cell TRADE: feed(A,c{}) -> strategy+risk(B,c{bcore}) -> crypto(C,c{ccore}) -> TX(A), zero-copy AES-GCM orders.", 2);

    let mut t_in = vec![0u64; bufs.len()];
    let interval = 2 * hz;
    let mut pkts=0u64; let mut c_sum=0u64; let mut c_min=u64::MAX; let mut c_max=0u64; let mut drops=0u64;
    let mut last = rdtsc(); let deadline = if run_secs>0 { last + run_secs*hz } else {0};
    loop {
        if deadline != 0 && rdtsc() >= deadline { stop.store(true, Ordering::Relaxed); std::process::exit(0); }
        if let Some((b, len)) = nic.rx_poll() {
            t_in[b as usize] = rdtsc();
            while !a2b.push(((len as u64) << 32) | b as u64) { std::hint::spin_loop(); }
        }
        if let Some(v) = c2a.pop() {
            let b = (v & 0xffff_ffff) as u32; let fl = (v >> 32) as usize;
            let c = rdtsc() - t_in[b as usize];
            if fl > 0 { pkts+=1; c_sum+=c; if c<c_min{c_min=c;} if c>c_max{c_max=c;} nic.tx_submit(b, fl); }
            else { drops+=1; nic.free_buf(b); }
        }
        let now = rdtsc();
        if now - last >= interval {
            let ns = |c: u64| c * 1_000_000_000 / hz;
            let dn = pmu.dtlb.read(); let ln = pmu.llc.read();
            let (dm, lm) = if pkts>0 {((dn-dtlb0)*1000/pkts,(ln-llc0)*1000/pkts)} else {(0,0)};
            dtlb0=dn; llc0=ln;
            if pkts>0 {
                println!("trade: orders={} drops={} tick->trade ns min/avg/max={}/{}/{} dTLB/kpkt={} LLC/kpkt={}",
                    pkts, drops, ns(c_min), ns(c_sum/pkts), ns(c_max), dm, lm);
                if let Some(w)=&shm { w.publish(nic.link_up(), m, ns(c_sum/pkts), ns(c_min), ns(c_max), 0,0,0,0, now/hz, dm, lm); }
            }
            pkts=0; c_sum=0; c_min=u64::MAX; c_max=0; drops=0; last=now;
            if let Some(ref sp)=flags.stop { if std::path::Path::new(sp).exists() { let _=std::fs::remove_file(sp); stop.store(true,Ordering::Relaxed); if let Some(ref p)=flags.shm { let _=std::fs::remove_file(p);} std::process::exit(0);} }
        }
        std::hint::spin_loop();
    }
}

fn run_pipeline(mut nic: ixgbe::Ixgbe, m: [u8;6], hz: u64, bcore: usize, flags: &Flags, run_secs: u64) -> ! {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let a2b = Arc::new(spsc::Spsc::new(2048));
    let b2a = Arc::new(spsc::Spsc::new(2048));
    let txbuf = Arc::new(spsc::Spsc::new(256)); // buffers for stage B's TX replies (for L1)
    let bufs = Arc::new(nic.buf_addrs());
    let stop = Arc::new(AtomicBool::new(false));
    let l1 = flags.l1;

    if l1 {
        // shard B: the full smoltcp L1 stack (UDP echo)
        let (a,b,t,bf,st) = (a2b.clone(), b2a.clone(), txbuf.clone(), bufs.clone(), stop.clone());
        std::thread::spawn(move || { pin_self(bcore, true); l1pipe::run(a,b,t,bf,m,hz,st); });
    } else {
        // shard B: raw reflect
        let (a2b_b, b2a_b, bufs_b, stop_b) = (a2b.clone(), b2a.clone(), bufs.clone(), stop.clone());
        std::thread::spawn(move || {
            pin_self(bcore, true);
            while !stop_b.load(Ordering::Relaxed) {
                if let Some(v) = a2b_b.pop() {
                    let b = (v & 0xffff_ffff) as u32;
                    let len = (v >> 32) as usize;
                    let ptr = bufs_b[b as usize] as *mut u8;
                    let frame = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
                    let out = reflect::reflect(frame, &m).unwrap_or(0);
                    while !b2a_b.push(((out as u64) << 32) | b as u64) { std::hint::spin_loop(); }
                } else { std::hint::spin_loop(); }
            }
        });
    }

    let shm = flags.shm.as_ref().and_then(|p| shm::ShmWriter::create(p).ok());
    if let Some(w) = &shm { unsafe { (*w.p).pid = std::process::id(); } }
    let pmu = pmu::Pmu::new();
    let mut dtlb0 = pmu.dtlb.read(); let mut llc0 = pmu.llc.read();
    println!("cell PIPELINE: shard A (RX/TX) <-> shard B ({}, core {bcore}), zero-copy.",
        if l1 {"L1 smoltcp"} else {"reflect"});

    let mut t_in = vec![0u64; bufs.len()];
    let interval = 2 * hz;
    let mut pkts = 0u64; let mut c_sum=0u64; let mut c_min=u64::MAX; let mut c_max=0u64;
    let mut last = rdtsc();
    let t_start = last;
    let deadline = if run_secs > 0 { t_start + run_secs * hz } else { 0 };

    loop {
        if deadline != 0 && rdtsc() >= deadline {
            stop.store(true, Ordering::Relaxed);
            std::process::exit(0);
        }
        // A: RX -> a2b
        if let Some((b, len)) = nic.rx_poll() {
            t_in[b as usize] = rdtsc();
            while !a2b.push(((len as u64) << 32) | b as u64) { std::hint::spin_loop(); }
        }
        // A: refill txbuf with free buffers (for stage B's TX replies)
        if l1 {
            while txbuf.len() < 64 {
                match nic.alloc_buf() {
                    Some(tb) => { if !txbuf.push(tb as u64) { nic.free_buf(tb); break; } }
                    None => break,
                }
            }
        }
        // A: b2a -> a TX reply (len>0) or freeing an RX buffer (len=0)
        if let Some(v) = b2a.pop() {
            let idx = (v & 0xffff_ffff) as u32;
            let len = (v >> 32) as usize;
            if len > 0 {
                pkts += 1;
                if !l1 { let c = rdtsc()-t_in[idx as usize]; c_sum+=c; if c<c_min{c_min=c;} if c>c_max{c_max=c;} }
                nic.tx_submit(idx, len);
            } else {
                if l1 { let c = rdtsc()-t_in[idx as usize]; c_sum+=c; if c<c_min{c_min=c;} if c>c_max{c_max=c;} }
                nic.free_buf(idx);
            }
        }
        let now = rdtsc();
        if now - last >= interval {
            let ns = |c: u64| c * 1_000_000_000 / hz;
            let dtlb_now = pmu.dtlb.read(); let llc_now = pmu.llc.read();
            let (dmpk, lmpk) = if pkts>0 { ((dtlb_now-dtlb0)*1000/pkts, (llc_now-llc0)*1000/pkts) } else {(0,0)};
            dtlb0 = dtlb_now; llc0 = llc_now;
            if pkts > 0 {
                let lbl = if l1 {"A->B(stack) ns"} else {"A->B->A ns"};
                println!("pipeline[{}]: pkts={} {} min/avg/max={}/{}/{} dTLB/kpkt={} LLC/kpkt={}",
                    if l1 {"L1"} else {"reflect"}, pkts, lbl, ns(c_min), ns(c_sum/pkts.max(1)), ns(c_max), dmpk, lmpk);
                if let Some(w) = &shm {
                    w.publish(nic.link_up(), m, ns(c_sum/pkts), ns(c_min), ns(c_max), 0, 0, 0, 0,
                        (now)/hz, dmpk, lmpk);
                }
            }
            pkts=0; c_sum=0; c_min=u64::MAX; c_max=0; last=now;
            if let Some(ref sp) = flags.stop {
                if std::path::Path::new(sp).exists() {
                    let _ = std::fs::remove_file(sp);
                    stop.store(true, Ordering::Relaxed);
                    if let Some(ref p) = flags.shm { let _ = std::fs::remove_file(p); }
                    println!("pipeline: stop"); std::process::exit(0);
                }
            }
        }
        std::hint::spin_loop();
    }
}

