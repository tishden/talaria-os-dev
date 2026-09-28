//! Reading PMU counters (dTLB/LLC-load-misses) through perf_event_open + rdpmc.
//! rdpmc is a userspace instruction: a read without a syscall, so the cell stays syscall-free.
use std::os::unix::io::RawFd;

const PERF_TYPE_HW_CACHE: u32 = 3;
// config = cache_id | (op<<8) | (result<<16)
const DTLB: u64 = 3 | (0 << 8) | (1 << 16); // DTLB, READ, MISS
const LLC: u64 = 2 | (0 << 8) | (1 << 16);  // LL, READ, MISS

#[repr(C)]
struct PerfAttr {
    type_: u32, size: u32, config: u64,
    sample: u64, sample_type: u64, read_format: u64,
    flags: u64, // disabled(0) inherit(1) ... exclude_user(5)? see below
    wakeup: u32, bp_type: u32,
    config1: u64, config2: u64,
    branch_sample_type: u64, sample_regs_user: u64,
    sample_stack_user: u32, clockid: i32,
    sample_regs_intr: u64, aux_watermark: u32, sample_max_stack: u16, _r: u16,
}

fn open(config: u64) -> RawFd {
    let mut a: PerfAttr = unsafe { std::mem::zeroed() };
    a.type_ = PERF_TYPE_HW_CACHE;
    a.size = std::mem::size_of::<PerfAttr>() as u32;
    a.config = config;
    // flags: disabled=0 (counts immediately); exclude_kernel(bit5)=1, exclude_hv(bit6)=1
    a.flags = (1 << 5) | (1 << 6);
    let fd = unsafe {
        libc::syscall(libc::SYS_perf_event_open, &a as *const _, 0, -1i32, -1i32, 0) as RawFd
    };
    fd
}

#[inline]
unsafe fn rdpmc(idx: u32) -> u64 {
    let lo: u32; let hi: u32;
    core::arch::asm!("rdpmc", in("ecx") idx, out("eax") lo, out("edx") hi, options(nostack, nomem));
    ((hi as u64) << 32) | lo as u64
}

pub struct Counter { page: *const u8, ok: bool }

impl Counter {
    fn new(config: u64) -> Self {
        let fd = open(config);
        if fd < 0 { return Counter { page: std::ptr::null(), ok: false }; }
        let page = unsafe {
            libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ, libc::MAP_SHARED, fd, 0)
        };
        if page == libc::MAP_FAILED { return Counter { page: std::ptr::null(), ok: false }; }
        Counter { page: page as *const u8, ok: true }
    }
    /// The counter's current value through rdpmc (the offset comes from the mmap page).
    pub fn read(&self) -> u64 {
        if !self.ok { return 0; }
        unsafe {
            let p = self.page;
            loop {
                let seq = std::ptr::read_volatile(p.add(8) as *const u32); // lock
                std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
                let index = std::ptr::read_volatile(p.add(12) as *const u32);
                let offset = std::ptr::read_volatile(p.add(16) as *const i64);
                let width = std::ptr::read_volatile(p.add(48) as *const u16);
                let mut count = offset as i64;
                if index != 0 {
                    let raw = rdpmc(index - 1) as i64;
                    let shift = 64 - width as i64;
                    count += (raw << shift) >> shift; // sign-extend by the counter width
                }
                std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
                if std::ptr::read_volatile(p.add(8) as *const u32) == seq {
                    return count as u64;
                }
            }
        }
    }
    pub fn available(&self) -> bool { self.ok }
}

pub struct Pmu { pub dtlb: Counter, pub llc: Counter }
impl Pmu {
    pub fn new() -> Self { Pmu { dtlb: Counter::new(DTLB), llc: Counter::new(LLC) } }
}
