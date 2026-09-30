//! The vDSO: a process's clock without a syscall.
//!
//! A syscall on this machine costs 95 ns, `clock_gettime` — 172 ns, while the runtimes
//! ask for the time constantly: the JVM's `System.nanoTime()` is called on every
//! message in a queue, ClickHouse measures every stage of the pipeline. Linux solves
//! that by giving the process a page of code that reads the TSC and converts it
//! into nanoseconds itself. Here it is the same.
//!
//! The image (`user/vdso/vdso.S`, built by `build.rs` with the host's `cc`) is exactly two
//! pages: the code with the ELF headers and the data page. The data is written by the KERNEL at the
//! identity address, the process sees them read-only. The protocol is a seqlock:
//! an odd `seq` means "the kernel is correcting them, they must not be read".
//!
//! The address is given to the process through `AT_SYSINFO_EHDR` — by it libc (both musl and
//! glibc) finds the symbols `__vdso_clock_gettime` and its relatives. The symbols are
//! versioned (`LINUX_2.6`), because glibc takes only versioned ones.

use core::sync::atomic::{AtomicU64, Ordering};

static IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vdso.so"));

/// The vDSO's virtual address in a process's address space. It is chosen in the "hole"
/// between the private mmap region (0x6000_…) and the stack (0x7fff_…), so as not to
/// intersect with anything that `proc::mmap_alloc` hands out.
pub const VADDR: u64 = 0x7ffd_0000_0000;
const PAGES: u64 = 2;

/// The physical base of the image's copy (0 — the vDSO is not up).
static BASE: AtomicU64 = AtomicU64::new(0);

/// Copy the image into two pages of our own. It is called once at the bring-up.
pub fn init() {
    if BASE.load(Ordering::SeqCst) != 0 { return; }
    // We copy exactly the LOADABLE part (two pages). In the file after it there lie also
    // the section headers — the runtime does not need them, and there is no point in dragging them into memory.
    let load = (PAGES * 4096) as usize;
    if IMAGE.len() < load {
        crate::println!("vdso: the image of {} bytes is shorter than {} — the vDSO is switched off", IMAGE.len(), load);
        return;
    }
    // The image must export the clock call a libc will look for; an image that does not
    // (a wrong version script, a cross compiler that produced something else) is not a
    // vDSO, and a process would rather have none than one it cannot use.
    let Some(clock) = symbol(crate::arch::VDSO_CLOCK_SYMBOL) else {
        crate::println!("vdso: the image has no {} — the vDSO is switched off",
            core::str::from_utf8(crate::arch::VDSO_CLOCK_SYMBOL).unwrap_or("?"));
        return;
    };
    let fr = match crate::mem::alloc_contig(PAGES) { Some(f) => f, None => return };
    unsafe {
        core::ptr::write_bytes(fr as *mut u8, 0, load);
        core::ptr::copy_nonoverlapping(IMAGE.as_ptr(), fr as *mut u8, load);
        crate::arch::icache::sync(fr, load as u64);   // the trampolines and the clock calls are code
    }
    BASE.store(fr, Ordering::SeqCst);
    crate::tsc::publish_vdso_now(); // the clock's parameters go at once, before the first discipline
    crate::println!("vdso: the clock in user space is up (2 pages, the data at {:#x}, {} at +{:#x})", VADDR + 4096,
        core::str::from_utf8(crate::arch::VDSO_CLOCK_SYMBOL).unwrap_or("?"), clock);
}

/// The offset of an exported symbol in the image (`__vdso_clock_gettime` on x86,
/// `__kernel_clock_gettime` on arm64), from the `.dynsym`/`.dynstr` sections the linker
/// keeps in the file — a caller adds `VADDR`. `None` if the image has no such symbol
/// or is not the ELF we built. Used by the arm64 EL0 probe; a libc finds the same
/// symbols by itself through `AT_SYSINFO_EHDR`.
pub fn symbol(name: &[u8]) -> Option<u64> {
    let img = IMAGE;
    let rd16 = |o: usize| img.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let rd32 = |o: usize| img.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let rd64 = |o: usize| img.get(o..o + 8).map(|b| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]));
    if img.get(0..4)? != b"\x7fELF" { return None; }
    let shoff = rd64(0x28)? as usize;
    let shentsize = rd16(0x3A)? as usize;
    let shnum = rd16(0x3C)? as usize;
    const SHT_DYNSYM: u32 = 11;
    for i in 0..shnum {
        let sh = shoff + i * shentsize;
        if rd32(sh + 4)? != SHT_DYNSYM { continue; }
        let sym_off = rd64(sh + 0x18)? as usize;
        let sym_size = rd64(sh + 0x20)? as usize;
        let link = rd32(sh + 0x28)? as usize;
        let str_off = rd64(shoff + link * shentsize + 0x18)? as usize;
        let str_size = rd64(shoff + link * shentsize + 0x20)? as usize;
        let strtab = img.get(str_off..str_off + str_size)?;
        let mut o = sym_off;
        while o + 24 <= sym_off + sym_size {
            let st_name = rd32(o)? as usize;
            let st_value = rd64(o + 8)?;
            let n = strtab.get(st_name..)?;
            let n = &n[..n.iter().position(|&b| b == 0).unwrap_or(n.len())];
            if n == name && st_value != 0 { return Some(st_value); }
            o += 24;
        }
    }
    None
}

/// Map the vDSO into a process's address space: the code — R+X, the data — R.
/// It returns the address for `AT_SYSINFO_EHDR` (0 — there is no vDSO).
pub fn map_into(pml4: u64) -> u64 {
    let base = BASE.load(Ordering::SeqCst);
    if base == 0 { return 0; }
    // NOT private pages: the image is one for all the processes, and it must not be freed at
    // a process's exit (map_leaf would have marked them PRIV -> free_address_space
    // would have returned foreign frames into the allocator).
    crate::paging::map_leaf_shared(pml4, VADDR, base, false, true, true);              // the code
    crate::paging::map_leaf_shared(pml4, VADDR + 4096, base + 4096, false, true, false); // the data
    VADDR
}

/// Publish the clock's parameters for user space (a seqlock).
/// `hz` is the TSC's frequency, `wall_off` is how much to add to the monotonic one to
/// get the real time.
pub fn publish(tsc_base: u64, ns_base: u64, hz: u64, wall_off: u64) {
    let base = BASE.load(Ordering::SeqCst);
    if base == 0 || hz == 0 { return; }
    // ns = (dt * mult) >> 32; mult = 1e9 * 2^32 / hz
    let mult = ((1_000_000_000u128 << 32) / hz as u128) as u64;
    let d = (base + 4096) as *mut u64;
    unsafe {
        let seq = d as *mut u32;
        let s = core::ptr::read_volatile(seq);
        core::ptr::write_volatile(seq, s | 1); // odd — the data is being corrected
        core::sync::atomic::fence(Ordering::SeqCst);
        core::ptr::write_volatile(d.add(1), tsc_base);
        core::ptr::write_volatile(d.add(2), ns_base);
        core::ptr::write_volatile(d.add(3), mult);
        core::ptr::write_volatile(d.add(4), wall_off);
        core::sync::atomic::fence(Ordering::SeqCst);
        core::ptr::write_volatile(seq, (s | 1) + 1); // even again
    }
}

