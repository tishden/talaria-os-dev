//! talaria — the Talaria control plane (§6.1-6.2 of the design document).
//!   talaria up <manifest>        — contract + start (once)
//!   talaria supervise <manifest> — contract + start with restart on a crash
//!   talaria status               — live telemetry of the cells (from shared memory)
//!   talaria down <name>          — graceful stop

mod manifest;
mod shmread;
mod batchread;
mod proxydrain;
mod tcpctl;
use manifest::Manifest;
use shmread::ShmReader;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const RUN: &str = "/tmp/talaria";

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let cmd = a.get(1).map(|s| s.as_str()).unwrap_or("help");
    std::fs::create_dir_all(RUN).ok();
    let code = match cmd {
        "up" => match prepare(arg(&a, 2)) { Ok(p) => { let _ = launch(&p); settle(&p); ok(&p.name); 0 } Err(()) => fail() },
        "supervise" => match prepare(arg(&a, 2)) { Ok(p) => supervise(p), Err(()) => fail() },
        "status" => { status(); 0 }
        "coloc" => coloc(arg(&a, 2), a.iter().any(|x| x == "--no-sched"), a.iter().any(|x| x == "--fine")),
        "run" => run(arg(&a, 2)),
        "down" => down(arg(&a, 2)),
        "serve" => tcpctl::serve(arg(&a, 2)),
        "remote" => tcpctl::remote(arg(&a, 2), &a.get(3..).map(|s| s.to_vec()).unwrap_or_default()),
        _ => { eprintln!("usage: talaria up|supervise <manifest> | status | down <name> | serve [addr] | remote <addr> <cmd...>"); 2 }
    };
    std::process::exit(code);
}
fn arg(a: &[String], i: usize) -> &str { a.get(i).map(|s| s.as_str()).unwrap_or("") }
fn ok(name: &str) { println!("\ncell '{name}' is up. `talaria status` / `talaria down {name}`"); }
fn fail() -> i32 { eprintln!("\n\x1b[31mcontract not met — cell not started\x1b[0m"); 1 }

macro_rules! check {
    ($ok:expr, $($x:tt)*) => {
        if $ok { println!("  \x1b[32m✓\x1b[0m {}", format!($($x)*)); }
        else   { println!("  \x1b[31m✗\x1b[0m {}", format!($($x)*)); return Err(()); }
    };
}

struct Params {
    name: String, binabs: String, mode: String, seconds: i64,
    core: usize, fifo: bool, llc_mask: Option<u64>, crash_after: i64,
    shm: String, status: String, stop: String, log: String, proxy: String, gate: Option<String>, numa: Option<i64>,
}

fn prepare(mpath: &str) -> Result<Params, ()> {
    let man = Manifest::load(mpath).map_err(|e| { eprintln!("manifest: {e}"); })?;
    let name = man.str("cell.name").map_err(|e| eprintln!("{e}"))?;
    let binary = man.str("cell.binary").unwrap_or_default();
    let mode = man.str("cell.mode").unwrap_or("cell".into());
    let cores = man.ints("cpu.cores").map_err(|e| eprintln!("{e}"))?;
    let require_iso = man.bool("cpu.require_isolation");
    let fifo = man.bool("cpu.sched_fifo");
    let llc_mask = man.str("cpu.llc_mask").ok()
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok());
    let hp = man.int("memory.hugepages_1g").unwrap_or(0);
    let nic = man.str("device.nic").unwrap_or_default();
    let driver = man.str("device.driver").unwrap_or("vfio-pci".into());
    let seconds = man.int("run.seconds").unwrap_or(0);
    let crash_after = man.int("run.crash_after").unwrap_or(0);
    let numa = man.int("memory.numa_node").ok();

    println!("contract of cell '{name}':");
    let binabs = std::fs::canonicalize(&binary).map(|p| p.to_string_lossy().into_owned()).unwrap_or(binary);
    check!(Path::new(&binabs).exists(), "binary {binabs}");

    let ncpu = num_cpus();
    let isolated = read_cpu_list("/sys/devices/system/cpu/isolated");
    for &c in &cores {
        check!((c as usize) < ncpu, "core {c} exists (nproc={ncpu})");
        let iso = isolated.contains(&(c as usize));
        if require_iso { check!(iso, "core {c} is isolated (isolcpus)"); }
        else { println!("  \x1b[33m•\x1b[0m core {c}: {}", if iso {"isolated"} else {"NOT isolated (soft)"}); }
    }
    if hp > 0 {
        let free = read_int("/sys/kernel/mm/hugepages/hugepages-1048576kB/free_hugepages");
        check!(free >= hp, "1G hugepages: need {hp}, free {free}");
    }
    if !nic.is_empty() {
        let cur = pci_driver(&nic);
        check!(cur.as_deref() == Some(driver.as_str()),
            "NIC {nic} on driver {driver} (now: {})", cur.as_deref().unwrap_or("none"));
    }
    if let Some(mask) = llc_mask {
        let cat = Path::new("/sys/fs/resctrl/info/L3/cbm_mask").exists();
        check!(cat, "LLC/CAT available for mask {:#x} (resctrl)", mask);
    }
    if let Some(node) = numa {
        check!(Path::new(&format!("/sys/devices/system/node/node{node}")).exists(),
            "NUMA node {node} exists");
        let cnode = core_numa_node(cores[0] as usize);
        if let Some(cn) = cnode {
            if cn as i64 == node { println!("  \x1b[32m✓\x1b[0m core {} on node {node} (local)", cores[0]); }
            else { println!("  \x1b[33m•\x1b[0m WARNING: core {} on node {cn}, memory on node {node} (cross-NUMA)", cores[0]); }
        }
    }

    Ok(Params {
        name: name.clone(), binabs, mode, seconds, core: cores[0] as usize, fifo, llc_mask, crash_after,
        shm: format!("{RUN}/{name}.shm"), status: format!("{RUN}/{name}.status"),
        stop: format!("{RUN}/{name}.stop"), log: format!("{RUN}/{name}.log"),
        proxy: format!("{RUN}/{name}.proxy"),
        gate: None, numa,
    })
}

fn launch(p: &Params) -> std::io::Result<Child> {
    std::fs::remove_file(&p.stop).ok();
    let logf = std::fs::OpenOptions::new().create(true).append(true).open(&p.log)?;
    let mut cmd = Command::new("sudo");
    cmd.arg(&p.binabs).arg(&p.mode).arg(p.seconds.to_string())
        .arg("--core").arg(p.core.to_string())
        .arg("--status").arg(&p.status).arg("--stop").arg(&p.stop).arg("--shm").arg(&p.shm);
    if p.fifo { cmd.arg("--fifo"); }
    if let Some(m) = p.llc_mask { cmd.arg("--llc-mask").arg(format!("{:x}", m)); }
    if !p.proxy.is_empty() { cmd.arg("--proxy").arg(&p.proxy); }
    if let Some(g) = &p.gate { cmd.arg("--gate").arg(g); }
    if let Some(n) = p.numa { cmd.arg("--numa").arg(n.to_string()); }
    if p.crash_after > 0 { cmd.arg("--crash-after").arg(p.crash_after.to_string()); }
    cmd.stdout(Stdio::from(logf.try_clone()?)).stderr(Stdio::from(logf));
    println!("\nstarting: sudo {} {} {} --core {}{}{}", p.binabs, p.mode, p.seconds, p.core,
        if p.fifo {" --fifo"} else {""},
        p.llc_mask.map(|m| format!(" --llc-mask {:x}", m)).unwrap_or_default());
    cmd.spawn()
}

fn settle(p: &Params) {
    for _ in 0..50 { std::thread::sleep(Duration::from_millis(100)); if Path::new(&p.shm).exists() { break; } }
}

fn supervise(p: Params) -> i32 {
    println!("\n\x1b[1msupervisor\x1b[0m of cell '{}' (restart on a crash; `talaria down {}` to stop)", p.name, p.name);
    let mut restarts = 0;
    loop {
        let mut child = match launch(&p) { Ok(c) => c, Err(e) => { eprintln!("start: {e}"); return 1; } };
        let st = child.wait();
        if Path::new(&p.stop).exists() {
            println!("supervisor: got down — stopping"); return 0;
        }
        restarts += 1;
        let code = st.ok().and_then(|s| s.code()).unwrap_or(-1);
        println!("\x1b[33msupervisor: the cell crashed (code {code}), restart #{restarts} in 1 s\x1b[0m");
        std::thread::sleep(Duration::from_secs(1));
        if Path::new(&p.stop).exists() { println!("supervisor: got down — stopping"); return 0; }
        if restarts > 100 { eprintln!("too many restarts — stop"); return 1; }
    }
}


fn ht_sibling(core: usize) -> usize {
    let p = format!("/sys/devices/system/cpu/cpu{core}/topology/thread_siblings_list");
    if let Ok(s) = std::fs::read_to_string(&p) {
        for tok in s.trim().split(&[',', '-'][..]) {
            if let Ok(c) = tok.parse::<usize>() { if c != core { return c; } }
        }
    }
    core // no HT — return the core itself
}

fn signal(pid: i32, sig: i32) { unsafe { libc::kill(pid, sig); } }

/// Co-location: a latency cell + a batch cell on the hyperthread sibling. The control
/// plane donates the core to the batch cell while the latency cell idles (SIGSTOP/SIGCONT).
fn coloc(lat_manifest: &str, no_sched: bool, fine: bool) -> i32 {
    let mut p = match prepare(lat_manifest) { Ok(p) => p, Err(()) => return fail() };
    let gate_path = format!("{RUN}/coloc.gate");
    if fine {
        // pre-create the gate file (as the user); both cells honour it
        std::fs::write(&gate_path, [0u8;64]).ok();
        p.gate = Some(gate_path.clone());
    }
    let lat_core = p.core;
    let batch_core = ht_sibling(lat_core);
    let batch_bin = std::fs::canonicalize("control-plane/batch/target/release/talaria-batch")
        .map(|x| x.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "control-plane/batch/target/release/talaria-batch".into());
    let batch_shm = format!("{RUN}/batch.shm");
    let batch_stop = format!("{RUN}/batch.stop");
    let coloc_stop = format!("{RUN}/coloc.stop");
    std::fs::remove_file(&coloc_stop).ok();
    std::fs::remove_file(&batch_stop).ok();

    println!("\n\x1b[1mco-location\x1b[0m: latency='{}' core {}  |  batch core {} (HT sibling){}",
        p.name, lat_core, batch_core, if no_sched {"  [NO donation]"} else if fine {"  [fine donation: gate ~5 µs]"} else {"  [donation: CP SIGSTOP ~2 ms]"});

    // 1. the latency cell
    if launch(&p).is_err() { eprintln!("latency launch"); return 1; }
    settle(&p);

    // 2. the batch cell (an ordinary user, SCHED_IDLE)
    let blog = std::fs::OpenOptions::new().create(true).append(true).open(format!("{RUN}/batch.log")).unwrap();
    let mut bcmd = std::process::Command::new(&batch_bin);
    bcmd.arg("--core").arg(batch_core.to_string())
        .arg("--shm").arg(&batch_shm).arg("--stop").arg(&batch_stop);
    if fine { bcmd.arg("--gate").arg(&gate_path); }
    let batch = bcmd
        .stdout(Stdio::from(blog.try_clone().unwrap())).stderr(Stdio::from(blog))
        .spawn();
    let batch = match batch { Ok(c) => c, Err(e) => { eprintln!("batch launch: {e}"); return 1; } };
    let bpid = batch.id() as i32;
    println!("batch pid={bpid} (SCHED_IDLE)");

    // 3. the donation loop
    let lat = ShmReader::open(&p.shm).ok();
    std::thread::sleep(Duration::from_millis(300));
    let bread = batchread::BatchReader::open(&batch_shm).ok();

    let mut last_pkts = lat.as_ref().map(|r| r.pkts()).unwrap_or(0);
    let mut last_work = bread.as_ref().map(|r| r.work_units()).unwrap_or(0);
    let mut last_work_t = std::time::Instant::now();
    let mut parked = false;
    let mut ticks = 0u64; let mut parked_ticks = 0u64;
    let mut last_report = std::time::Instant::now();

    loop {
        std::thread::sleep(Duration::from_millis(2));
        ticks += 1;
        if Path::new(&coloc_stop).exists() {
            std::fs::remove_file(&coloc_stop).ok();
            signal(bpid, libc::SIGCONT); // unfreeze before stopping
            std::fs::write(&batch_stop, "1").ok();
            std::fs::write(&p.stop, "1").ok();
            println!("co-location: down — both cells are stopping");
            return 0;
        }
        let pkts = lat.as_ref().map(|r| r.pkts()).unwrap_or(last_pkts);
        let busy = pkts != last_pkts;
        last_pkts = pkts;

        if !no_sched && !fine {
            if busy && !parked { signal(bpid, libc::SIGSTOP); parked = true; }
            else if !busy && parked { signal(bpid, libc::SIGCONT); parked = false; }
        }
        if parked { parked_ticks += 1; }

        if last_report.elapsed() >= Duration::from_secs(2) {
            let work = bread.as_ref().map(|r| r.work_units()).unwrap_or(last_work);
            let secs = last_work_t.elapsed().as_secs_f64().max(0.001);
            let brate = ((work - last_work) as f64 / secs / 1000.0) as u64;
            last_work = work; last_work_t = std::time::Instant::now();
            let latst = lat.as_ref().and_then(|r| r.read());
            let (lavg, ltail) = latst.map(|s| (s.rx_tx_avg_ns, s.gaps_gt_10us)).unwrap_or((0,0));
            println!("coloc: latency pkts={} rx→tx={}ns gaps>10us={} | batch rate={}k/s park={}%",
                pkts, lavg, ltail, brate, if ticks>0 {parked_ticks*100/ticks} else {0});
            ticks = 0; parked_ticks = 0; last_report = std::time::Instant::now();
        }
    }
}


/// Start a cell with the syscall proxy: the control plane executes its cold
/// syscalls (log, file read) and measures from outside that the cell is syscall-free.
fn run(mpath: &str) -> i32 {
    let p = match prepare(mpath) { Ok(p) => p, Err(()) => return fail() };
    // pre-create the proxy file as the user (both sides write)
    let mut drainer = match proxydrain::ProxyDrainer::create(&p.proxy) {
        Ok(d) => d, Err(e) => { eprintln!("proxy: {e}"); return 1; }
    };
    let mut child = match launch(&p) { Ok(c) => c, Err(e) => { eprintln!("launch: {e}"); return 1; } };
    settle(&p);
    let audit_path = format!("{RUN}/{}.audit", p.name);
    let mut audit = std::fs::File::create(&audit_path).unwrap();
    let lat = ShmReader::open(&p.shm).ok();
    println!("\n\x1b[1mrun\x1b[0m '{}': cell + syscall proxy. audit → {audit_path}. `talaria down {}` to stop.", p.name, p.name);

    let mut last_report = std::time::Instant::now();
    let mut last_vcs = 0u64; let mut vcs_init = false; let mut total_logs = 0usize;
    loop {
        total_logs += drainer.poll(&mut audit);
        std::thread::sleep(Duration::from_millis(3));
        if Path::new(&p.stop).exists() { println!("run: down — stopping the cell"); break; }
        if let Ok(Some(_)) = child.try_wait() { println!("run: the cell exited"); break; }
        if last_report.elapsed() >= Duration::from_secs(2) {
            if let Some(st) = lat.as_ref().and_then(|r| { let pid = r.pid(); r.read().map(|s| (pid, s)) }) {
                let (pid, s) = st;
                let v = proxydrain::vcs(pid);
                let dvcs = if vcs_init { v.saturating_sub(last_vcs) } else { 0 };
                last_vcs = v; vcs_init = true;
                println!("run: pkts={} rx→tx={}ns dTLB-miss/kpkt={} LLC-miss/kpkt={} | proxy: logs={} | CELL vcs Δ={}",
                    s.pkts, s.rx_tx_avg_ns, s.dtlb_mpk, s.llc_mpk, total_logs, dvcs);
            }
            last_report = std::time::Instant::now();
        }
    }
    drainer.poll(&mut audit);
    0
}

fn status() { print!("{}", status_text(true)); }

/// Collect the telemetry text of all cells. color=false — no ANSI (for TCP clients).
pub fn status_text(color: bool) -> String {
    let (b, r) = if color { ("\x1b[1m", "\x1b[0m") } else { ("", "") };
    let rd = match std::fs::read_dir(RUN) { Ok(r) => r, Err(_) => return "no cells\n".into() };
    let mut out = String::new();
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().map(|x| x == "shm").unwrap_or(false) {
            let name = path.file_stem().unwrap().to_string_lossy();
            if let Ok(rr) = ShmReader::open(&path.to_string_lossy()) {
                if let Some(s) = rr.read() {
                    let mac = s.mac;
                    let llc = if s.llc_mask != 0 { format!("{:#x}", s.llc_mask) } else { "—".into() };
                    out += &format!("cell {b}{name}{r}: link={} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} \
pkts={} rx→tx={}ns <100ns={}% dTLB-miss/kpkt={} LLC-miss/kpkt={} syscalls_hotpath={} gaps>10us={} LLC={} up={}s\n",
                        if s.link != 0 {"up"} else {"down"},
                        mac[0],mac[1],mac[2],mac[3],mac[4],mac[5],
                        s.pkts, s.rx_tx_avg_ns, s.under_100ns_pct, s.dtlb_mpk, s.llc_mpk,
                        s.syscalls_hotpath, s.gaps_gt_10us, llc, s.uptime_s);
                }
            }
        }
    }
    if out.is_empty() { "no cells are up\n".into() } else { out }
}

/// Names of the cells that are up (by the .shm files in RUN).
pub fn cell_names() -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(RUN) {
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().map(|x| x == "shm").unwrap_or(false) {
                if let Some(n) = path.file_stem() { v.push(n.to_string_lossy().into_owned()); }
            }
        }
    }
    v.sort();
    v
}

fn down(name: &str) -> i32 {
    if name.is_empty() { eprintln!("usage: talaria down <name>"); return 2; }
    let msg = down_result(name);
    if msg.starts_with("cell") { print!("{msg}"); 0 } else { eprint!("{msg}"); 1 }
}

/// Graceful stop of a cell (writes the stop file); returns the answer text. Shared
/// by the CLI `down` and the TCP command `down`.
pub fn down_result(name: &str) -> String {
    if name.is_empty() { return "usage: down <name>\n".into(); }
    let stop = format!("{RUN}/{name}.stop");
    match std::fs::write(&stop, "1") {
        Ok(_) => format!("cell '{name}': stop sent (it exits on its next tick)\n"),
        Err(e) => format!("{stop}: {e}\n"),
    }
}

// --- read-only system helpers ---
fn num_cpus() -> usize {
    std::fs::read_to_string("/sys/devices/system/cpu/present").ok()
        .and_then(|s| s.trim().rsplit('-').next().and_then(|x| x.trim().parse::<usize>().ok()))
        .map(|n| n + 1).unwrap_or(1)
}
fn read_cpu_list(path: &str) -> Vec<usize> {
    let mut v = Vec::new();
    if let Ok(s) = std::fs::read_to_string(path) {
        for part in s.trim().split(',') {
            if part.is_empty() { continue; }
            if let Some((a, b)) = part.split_once('-') {
                if let (Ok(a), Ok(b)) = (a.parse(), b.parse()) { for i in a..=b { v.push(i); } }
            } else if let Ok(i) = part.parse() { v.push(i); }
        }
    }
    v
}
fn read_int(path: &str) -> i64 { std::fs::read_to_string(path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0) }
fn core_numa_node(core: usize) -> Option<usize> {
    // /sys/devices/system/cpu/cpuN/nodeX -> X
    let dir = format!("/sys/devices/system/cpu/cpu{core}");
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if let Some(n) = name.strip_prefix("node") {
                if let Ok(x) = n.parse() { return Some(x); }
            }
        }
    }
    Some(0) // the only node
}
fn pci_driver(bdf: &str) -> Option<String> {
    std::fs::read_link(format!("/sys/bus/pci/devices/{bdf}/driver")).ok()
        .and_then(|p| p.file_name().map(|x| x.to_string_lossy().into_owned()))
}
