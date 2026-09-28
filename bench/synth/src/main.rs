//! Synthetic workloads for checking isolcpus + CAT on the stand:
//!   latency <core> <ws_kb> <iters> — a pointer chase over a buffer of ws_kb (LLC-sensitive),
//!                                     measures per-request latency and prints percentiles.
//!   hog <core> <ws_mb>             — streaming over a large buffer (pollutes the LLC).
//! Pinning + SCHED_FIFO(latency)/SCHED_IDLE(hog) come from outside (taskset/chrt).
use std::time::Instant;

fn rdtsc() -> u64 { unsafe { std::arch::x86_64::_rdtsc() } }
fn hz() -> u64 { let t=rdtsc(); std::thread::sleep(std::time::Duration::from_millis(50)); (rdtsc()-t)*20 }

fn pin(core: usize, fifo: bool) {
    unsafe {
        let mut s: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(core, &mut s);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &s);
        let mut p: libc::sched_param = std::mem::zeroed();
        p.sched_priority = if fifo {80} else {0};
        libc::sched_setscheduler(0, if fifo {libc::SCHED_FIFO} else {libc::SCHED_IDLE}, &p);
    }
}

/// Build a ring of pointers (indices) with a random walk over the buffer.
fn build_chase(n: usize) -> Vec<usize> {
    let mut next: Vec<usize> = (0..n).collect();
    // shuffle (xorshift, deterministic)
    let mut x = 0x2545F4914F6CDD1Du64;
    for i in (1..n).rev() {
        x ^= x<<13; x^=x>>7; x^=x<<17;
        let j = (x as usize) % (i+1);
        next.swap(i, j);
    }
    // turn the permutation into a linked list: chain[perm[k]] = perm[k+1]
    let mut chain = vec![0usize; n];
    for k in 0..n { chain[next[k]] = next[(k+1)%n]; }
    chain
}


// --- a minimal perf counter for dTLB-load-misses (read() at the end, for the benchmark) ---
#[repr(C)]
struct PerfAttr { type_: u32, size: u32, config: u64, sample: u64, sample_type: u64,
    read_format: u64, flags: u64, wakeup: u32, bp_type: u32, config1: u64, config2: u64,
    b: u64, sru: u64, ssu: u32, clockid: i32, sri: u64, aw: u32, sms: u16, r: u16 }
fn dtlb_counter() -> i32 {
    let mut a: PerfAttr = unsafe { std::mem::zeroed() };
    a.type_ = 3; // HW_CACHE
    a.size = std::mem::size_of::<PerfAttr>() as u32;
    a.config = 3 | (0<<8) | (1<<16); // DTLB, READ, MISS
    a.flags = (1<<5) | (1<<6); // exclude_kernel, exclude_hv
    unsafe { libc::syscall(libc::SYS_perf_event_open, &a as *const _, 0, -1i32, -1i32, 0) as i32 }
}
fn read_ctr(fd: i32) -> u64 {
    if fd < 0 { return 0; }
    let mut v: u64 = 0;
    unsafe { libc::read(fd, &mut v as *mut u64 as *mut libc::c_void, 8); }
    v
}
const MAP_HUGE_1GB: i32 = 30 << 26;
/// Allocate an array of usize on a 1G hugepage (physically contiguous, 1 TLB entry).
unsafe fn huge_alloc(n: usize) -> *mut usize {
    let bytes = ((n * 8) + (1<<30) - 1) & !((1<<30) - 1);
    let p = libc::mmap(std::ptr::null_mut(), bytes, libc::PROT_READ|libc::PROT_WRITE,
        libc::MAP_PRIVATE|libc::MAP_ANONYMOUS|libc::MAP_HUGETLB|MAP_HUGE_1GB|libc::MAP_POPULATE, -1, 0);
    assert_ne!(p, libc::MAP_FAILED, "mmap 1G hugepage (no free 1G pages?)");
    p as *mut usize
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mode = a.get(1).map(|s| s.as_str()).unwrap_or("");
    let core: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let hzv = hz();

    match mode {
        "latency" => {
            pin(core, true);
            let ws_kb: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(256);
            let iters: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(2_000_000);
            let n = ws_kb * 1024 / std::mem::size_of::<usize>();
            let huge = a.iter().any(|x| x == "--huge");
            let chain_vec;
            let chain: &mut [usize] = if huge {
                let p = unsafe { huge_alloc(n) };
                unsafe { std::slice::from_raw_parts_mut(p, n) }
            } else {
                chain_vec = build_chase(n);
                // for the 4K case we use a Vec; for huge pages it is filled below
                Box::leak(chain_vec.into_boxed_slice())
            };
            if huge {
                let built = build_chase(n);
                chain.copy_from_slice(&built);
            }
            let ctr = dtlb_counter();
            // a "request" = D pointer-chase steps over the working set
            const D: usize = 64;
            let mut idx = 0usize;
            let mut lat = Vec::with_capacity(iters);
            // warm-up
            for _ in 0..n { idx = chain[idx]; }
            for _ in 0..iters {
                let t = rdtsc();
                for _ in 0..D { idx = unsafe { *chain.get_unchecked(idx) }; }
                lat.push(rdtsc() - t);
                std::hint::black_box(idx);
            }
            let dtlb = read_ctr(ctr);
            lat.sort_unstable();
            let ns = |c: u64| c * 1_000_000_000 / hzv;
            let pc = |p: f64| ns(lat[((lat.len()-1) as f64 * p) as usize]);
            println!("SYNTH-LAT ws={}kb pages={} D={} n={} p50={} p90={} p99={} p999={} max={} dTLB-miss/req={:.2} (ns/req)",
                ws_kb, if huge {"1G"} else {"4K"}, D, iters, pc(0.5), pc(0.9), pc(0.99), pc(0.999), ns(lat[lat.len()-1]),
                dtlb as f64 / iters as f64);
        }
        "hog" => {
            pin(core, false);
            let ws_mb: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(64);
            let n = ws_mb * 1024 * 1024 / 8;
            let mut buf = vec![0u64; n];
            let start = Instant::now();
            let mut i = 0usize; let mut acc = 0u64;
            loop {
                // streaming read+write — flushes the LLC
                buf[i] = buf[i].wrapping_add(1); acc ^= buf[i];
                i += 8; if i >= n { i = 0; std::hint::black_box(acc); if start.elapsed().as_secs() > 3600 { break; } }
            }
        }
        _ => eprintln!("usage: synth latency <core> <ws_kb> <iters> | hog <core> <ws_mb>"),
    }
}
