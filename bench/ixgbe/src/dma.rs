//! DMA memory for noiommu mode: one 1 GiB hugepage, physically contiguous, with
//! the physical address taken from /proc/self/pagemap (root needed). Inside the page
//! there is a simple bump + free-list allocator for packet buffers.

use libc::{c_void, off_t};
use std::fs::File;
use std::os::unix::io::AsRawFd;

const HUGE_1G: usize = 1 << 30;
const MAP_HUGE_1GB: i32 = 30 << 26; // MAP_HUGE_SHIFT=26

pub struct HugeRegion {
    pub virt: *mut u8,
    pub phys: usize,
    pub size: usize,
    off: usize,
}

impl HugeRegion {
    pub fn alloc_1g(numa: Option<usize>) -> std::io::Result<Self> {
        // no MAP_POPULATE: mbind first, fault afterwards -> the page lands on the right node
        let virt = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                HUGE_1G,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB | MAP_HUGE_1GB,
                -1,
                0,
            )
        };
        assert_ne!(virt, libc::MAP_FAILED, "mmap 1G hugepage (no free 1G pages?)");
        if let Some(node) = numa {
            const MPOL_BIND: i32 = 2;
            let mask: u64 = 1u64 << node;
            let rc = unsafe {
                libc::syscall(libc::SYS_mbind, virt, HUGE_1G, MPOL_BIND,
                    &mask as *const u64, 64i64, 0i32)
            };
            if rc != 0 { eprintln!("mbind(node={node}) failed (errno {})", unsafe { *libc::__errno_location() }); }
        }
        unsafe {
            libc::mlock(virt, HUGE_1G);
            std::ptr::write_bytes(virt as *mut u8, 0, 4096); // fault -> a 1G allocation on the node
        }
        let phys = virt_to_phys(virt as usize)?;
        Ok(HugeRegion { virt: virt as *mut u8, phys, size: HUGE_1G, off: 0 })
    }

    /// The NUMA node the page actually landed on (from /proc/self/numa_maps).
    pub fn actual_node(&self) -> Option<usize> {
        let maps = std::fs::read_to_string("/proc/self/numa_maps").ok()?;
        let key = format!("{:x}", self.virt as usize);
        for line in maps.lines() {
            if line.starts_with(&key) {
                for tok in line.split_whitespace() {
                    if let Some(n) = tok.strip_prefix("N") {
                        if let Some((node, _)) = n.split_once('=') {
                            return node.parse().ok();
                        }
                    }
                }
            }
        }
        None
    }

    /// Allocate a chunk with alignment, return (virt, phys).
    pub fn bump(&mut self, size: usize, align: usize) -> (*mut u8, usize) {
        let start = (self.off + align - 1) & !(align - 1);
        assert!(start + size <= self.size, "the hugepage is exhausted");
        self.off = start + size;
        unsafe { (self.virt.add(start), self.phys + start) }
    }
}

fn virt_to_phys(virt: usize) -> std::io::Result<usize> {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
    let f = File::open("/proc/self/pagemap")?;
    let mut buf = [0u8; 8];
    let offset = (virt / page * 8) as off_t;
    let n = unsafe {
        libc::pread(f.as_raw_fd(), buf.as_mut_ptr() as *mut c_void, 8, offset)
    };
    assert_eq!(n, 8, "pagemap read");
    let entry = u64::from_ne_bytes(buf);
    assert!(entry & (1 << 63) != 0, "the page is not present in pagemap");
    let pfn = (entry & 0x7f_ffff_ffff_ffff) as usize;
    Ok(pfn * page + virt % page)
}
