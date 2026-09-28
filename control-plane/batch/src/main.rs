//! The Talaria batch cell: a CPU-bound throughput workload on a dedicated core,
//! SCHED_IDLE (it yields to the latency cell). The control plane parks and releases
//! it with SIGSTOP/SIGCONT by the latency cell's load signal (core donation).
//! Telemetry goes to shared memory (work_units, rate). Stop — by the stop file.
use std::os::unix::io::AsRawFd;
include!("../../shared/batch_stats.rs");

fn rdtsc() -> u64 { unsafe { std::arch::x86_64::_rdtsc() } }

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let get = |k: &str| a.iter().position(|x| x == k).and_then(|i| a.get(i + 1)).cloned();
    let core: usize = get("--core").and_then(|s| s.parse().ok()).unwrap_or(0);
    let shm_path = get("--shm");
    let stop = get("--stop");
    let gate: *const u32 = get("--gate").map(|path| unsafe {
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path).unwrap();
        f.set_len(64).unwrap();
        let p = libc::mmap(std::ptr::null_mut(), 64, libc::PROT_READ|libc::PROT_WRITE, libc::MAP_SHARED, f.as_raw_fd(), 0);
        assert_ne!(p, libc::MAP_FAILED); p as *const u32
    }).unwrap_or(std::ptr::null());

    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(core, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        // SCHED_IDLE: batch yields to everything else on this core
        let p = libc::sched_param { sched_priority: 0 };
        libc::sched_setscheduler(0, libc::SCHED_IDLE, &p);
    }

    // shm
    let shm = shm_path.as_ref().map(|path| {
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
            .open(path).unwrap();
        f.set_len(4096).unwrap();
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ|libc::PROT_WRITE,
            libc::MAP_SHARED, f.as_raw_fd(), 0) };
        assert_ne!(ptr, libc::MAP_FAILED);
        let p = ptr as *mut BatchStats;
        unsafe { std::ptr::write_bytes(p as *mut u8, 0, std::mem::size_of::<BatchStats>()); (*p).magic = BATCH_MAGIC; }
        p
    });

    // TSC calibration
    let t0 = rdtsc(); std::thread::sleep(std::time::Duration::from_millis(50)); let hz = (rdtsc()-t0)*20;

    let mut work: u64 = 0;
    let mut x: u64 = 0x9e3779b97f4a7c15;
    let t_start = rdtsc();
    let mut last = rdtsc();
    let mut last_work = 0u64;
    let window = hz / 5; // 200 ms

    loop {
        // fine donation: if the latency cell is busy — yield the core (PAUSE spin)
        if !gate.is_null() {
            while unsafe { std::ptr::read_volatile(gate) } != 0 {
                std::hint::spin_loop();
                if let Some(ref sp) = stop { if std::path::Path::new(sp).exists() {
                    let _ = std::fs::remove_file(sp);
                    if let Some(ref p) = shm_path { let _ = std::fs::remove_file(p); }
                    return;
                }}
            }
        }
        // a unit of work: a heavy hash mix (imitating a batch computation)
        for _ in 0..4096 {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            x = x.wrapping_mul(0x2545F4914F6CDD1D);
        }
        work += 1;
        std::hint::black_box(x);

        let now = rdtsc();
        if now - last >= window {
            let dw = work - last_work;
            let secs = (now - last) as f64 / hz as f64;
            let rate_kps = (dw as f64 / secs / 1000.0) as u64;
            if let Some(p) = shm {
                unsafe {
                    std::ptr::write_volatile(&mut (*p).seq, (*p).seq + 1);
                    (*p).work_units = work; (*p).rate_kps = rate_kps;
                    (*p).running = 1; (*p).uptime_s = (now - t_start) / hz;
                    std::ptr::write_volatile(&mut (*p).seq, (*p).seq + 2);
                }
            }
            last = now; last_work = work;
            if let Some(ref sp) = stop {
                if std::path::Path::new(sp).exists() {
                    let _ = std::fs::remove_file(sp);
                    if let Some(ref p) = shm_path { let _ = std::fs::remove_file(p); }
                    return;
                }
            }
        }
    }
}
