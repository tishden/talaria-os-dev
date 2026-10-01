//! The arm64 half of the Talaria UEFI stub (aarch64-unknown-uefi) — the product path of
//! docs/ARM64-PLAN.md §4.2: an EC2 Graviton instance and `edk2` under QEMU both start the
//! machine this way. The flow:
//!   1) LoadedImage.LoadOptions -> the command line (QEMU passes `-append` so; booting
//!      from the ESP reads `\talaria.cfg`);
//!   2) the RSDP and, when the firmware published one, the device tree, from the
//!      configuration tables; the firmware's clock (`GetTime`) — there is no RTC the
//!      kernel could read on a machine described by ACPI alone;
//!   3) a bounce buffer with the kernel's one PT_LOAD and two pages for the boot block,
//!      the memory map and the command line;
//!   4) GetMemoryMap -> ExitBootServices (retried when the map's key changes); the EFI map
//!      -> the PVH `MemEntry` format the kernel's `mem::init` reads;
//!   5) with the firmware gone: the caches are cleaned over everything the next step
//!      reads or writes, the MMU is turned OFF at the current exception level, the
//!      segment is copied to its physical address (the range may hold the firmware's own
//!      page tables — with the MMU off they are no longer needed; the same contract as
//!      x86's "paging off, then copy to 2M"), and the Image is entered with `x0` = the
//!      boot block — MMU off, interrupts masked, as `booting.rst` asks.
//! The kernel is linked at a fixed physical address (RAM base + 512 KiB on `virt` and on
//! Graviton: `0x4008_0000`). A machine whose memory map has no RAM there, or keeps ACPI
//! tables there, is REFUSED with a message — never overwritten.
//!
//! The debugging output goes through the firmware's console until ExitBootServices; after
//! it the stub has no channel (the PL011's address is the kernel's to find), so the last
//! line printed says what will happen next.

use core::ffi::c_void;
use core::ptr::{addr_of, addr_of_mut};
use crate::efi::*;

static KERNEL: &[u8] = include_bytes!("../../target/aarch64-talaria/release/talaria");

/// The block the kernel finds at `x0` (`kernel/src/arch/aarch64/kmain.rs: BootBlock` —
/// change them together).
#[repr(C)]
struct BootBlock {
    magic: u64, rsdp: u64, dtb: u64, cmdline: u64, memmap: u64, memmap_entries: u64, epoch_s: u64,
}
const BOOT_MAGIC: u64 = 0x3154_4F4F_424C_4154; // "TALBOOT1"
#[repr(C)]
#[derive(Clone, Copy)]
struct MemEntry { addr: u64, size: u64, typ: u32, _r: u32 }

// The info pages: the block, the map, the command line.
const MM_OFF: usize = 64;
const MM_MAX: usize = 256;                    // 256 * 24 = 6144
const CL_OFF: usize = MM_OFF + MM_MAX * 24;   // 6208
const CL_MAX: usize = 1024;
const INFO_PAGES: usize = 2;

static mut MAPBUF: [u8; 65536] = [0; 65536];
static mut CON: *mut TextOut = core::ptr::null_mut();

fn puts(s: &str) {
    let con = unsafe { CON };
    if con.is_null() { return; }
    // The firmware's console takes UCS-2, NUL-terminated: in pieces of a line.
    let mut buf = [0u16; 82];
    let mut n = 0;
    for b in s.bytes() {
        if b == b'\n' { buf[n] = b'\r' as u16; n += 1; }
        buf[n] = b as u16; n += 1;
        if n >= 80 { buf[n] = 0; unsafe { ((*con).output_string)(con, buf.as_ptr()); } n = 0; }
    }
    if n > 0 { buf[n] = 0; unsafe { ((*con).output_string)(con, buf.as_ptr()); } }
}
fn puthex(v: u64) {
    let mut b = [0u8; 18];
    b[0] = b'0'; b[1] = b'x';
    for i in 0..16 {
        let d = ((v >> ((15 - i) * 4)) & 0xF) as u8;
        b[2 + i] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
    }
    puts(unsafe { core::str::from_utf8_unchecked(&b) });
}
fn fatal(msg: &str, st: Status) -> ! {
    puts("uefi-stub: FATAL "); puts(msg); puts(" status="); puthex(st as u64); puts("\n");
    loop { unsafe { core::arch::asm!("wfi") } }
}
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { fatal("panic", 0) }

fn le16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) }
fn le32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o+1], b[o+2], b[o+3]]) }
fn le64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes([b[o], b[o+1], b[o+2], b[o+3], b[o+4], b[o+5], b[o+6], b[o+7]]) }

/// Days since 1970-01-01 of a civil date (the proleptic Gregorian calendar).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

extern "C" {
    /// The firmware is gone: clean the caches, turn the MMU off, copy the segment, enter
    /// the Image. Never returns. `p` points at `Handoff`.
    fn tal_enter_kernel(p: *const Handoff) -> !;
}
/// What the trampoline needs, in the order its loads read (offsets 0, 8, ...).
#[repr(C)]
struct Handoff {
    info: u64,        // 0: the boot block -> x0 of the kernel
    src: u64,         // 8: the bounce copy of the segment
    dst: u64,         // 16: its physical address
    filesz: u64,      // 24: bytes to copy (a multiple of 8)
    memsz: u64,       // 32: the whole range the kernel will own (.bss included)
    entry: u64,       // 40
    stub_lo: u64,     // 48: the stub's own image — its code must be in RAM, not in a cache
    stub_hi: u64,     // 56
    info_hi: u64,     // 64: the end of the info pages
}
static mut HANDOFF: Handoff = Handoff { info: 0, src: 0, dst: 0, filesz: 0, memsz: 0, entry: 0, stub_lo: 0, stub_hi: 0, info_hi: 0 };

core::arch::global_asm!(r#"
.section .text
.global tal_enter_kernel
/* x0 = &Handoff. No stack and no memory but the Handoff's loads (done first, MMU on):
   everything after the MMU goes off works on registers. */
tal_enter_kernel:
    msr     daifset, #0xf
    ldr     x19, [x0, #0]           /* info */
    ldr     x20, [x0, #8]           /* src */
    ldr     x21, [x0, #16]          /* dst */
    ldr     x22, [x0, #24]          /* filesz */
    ldr     x23, [x0, #32]          /* memsz */
    ldr     x24, [x0, #40]          /* entry */
    ldr     x25, [x0, #48]          /* stub_lo */
    ldr     x26, [x0, #56]          /* stub_hi */
    ldr     x27, [x0, #64]          /* info_hi */
    /* The D-cache line: CTR_EL0.DminLine (log2 of the words). */
    mrs     x9, ctr_el0
    ubfx    x9, x9, #16, #4
    mov     x10, #4
    lsl     x10, x10, x9            /* the line in bytes */
    sub     x11, x10, #1
    /* Clean and invalidate, by address, four ranges: the bounce copy (it is read with the
       MMU off, straight from RAM), the destination (no stale line of the firmware's may
       come back over what is copied there), the info pages (the kernel reads them with
       its MMU off), the stub itself (this code is fetched from RAM from now on). */
    mov     x12, x20
    add     x13, x20, x22
    bl      1f
    mov     x12, x21
    add     x13, x21, x23
    bl      1f
    mov     x12, x19
    mov     x13, x27
    bl      1f
    mov     x12, x25
    mov     x13, x26
    bl      1f
    b       3f
1:  bic     x12, x12, x11
2:  dc      civac, x12
    add     x12, x12, x10
    cmp     x12, x13
    b.lo    2b
    dsb     sy
    ret
3:
    /* The MMU and the D-cache off at the level the firmware left us at. */
    mrs     x9, CurrentEL
    lsr     x9, x9, #2
    cmp     x9, #2
    b.eq    4f
    mrs     x9, sctlr_el1
    bic     x9, x9, #(1 << 0)       /* M */
    bic     x9, x9, #(1 << 2)       /* C */
    msr     sctlr_el1, x9
    b       5f
4:  mrs     x9, sctlr_el2
    bic     x9, x9, #(1 << 0)
    bic     x9, x9, #(1 << 2)
    msr     sctlr_el2, x9
5:  isb
    /* The copy, eight bytes at a time (filesz is rounded up by the caller). */
    mov     x12, x20
    mov     x13, x21
    add     x14, x20, x22
6:  cmp     x12, x14
    b.hs    7f
    ldr     x9, [x12], #8
    str     x9, [x13], #8
    b       6b
7:  dsb     sy
    ic      iallu                   /* the code just written is what is fetched */
    dsb     sy
    isb
    mov     x0, x19
    mov     x1, xzr
    mov     x2, xzr
    mov     x3, xzr
    br      x24
"#);

#[no_mangle]
pub extern "efiapi" fn efi_main(image: *mut c_void, st: *mut SystemTable) -> Status {
    unsafe { CON = (*st)._con_out as *mut TextOut; }
    puts("uefi-stub: Talaria UEFI boot (aarch64), kernel ");
    puthex(KERNEL.len() as u64); puts(" bytes\n");
    let bs = unsafe { &*(*st).boot };

    // The ELF: one PT_LOAD, the entry at its start (the Image header's branch).
    if &KERNEL[..4] != b"\x7fELF" || le16(KERNEL, 18) != 0xB7 { fatal("the kernel is not an AArch64 ELF", 0); }
    let entry = le64(KERNEL, 24);
    let phoff = le64(KERNEL, 32) as usize;
    let phentsize = le16(KERNEL, 54) as usize;
    let phnum = le16(KERNEL, 56) as usize;
    let (mut off, mut paddr, mut filesz, mut memsz, mut nload) = (0usize, 0u64, 0u64, 0u64, 0);
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        if le32(KERNEL, p) != 1 || le64(KERNEL, p + 40) == 0 { continue; } // PT_LOAD
        off = le64(KERNEL, p + 8) as usize;
        paddr = le64(KERNEL, p + 24);
        filesz = le64(KERNEL, p + 32);
        memsz = le64(KERNEL, p + 40);
        nload += 1;
    }
    if nload != 1 { fatal("expected a single PT_LOAD", nload); }
    if entry != paddr { fatal("the entry is not the segment's start", 0); }
    puts("uefi-stub: kernel phys "); puthex(paddr); puts(".."); puthex(paddr + memsz); puts("\n");

    // The bounce buffer (the segment) and the info pages, wherever the firmware has room.
    let copysz = (filesz + 7) & !7;
    let kpages = ((copysz + 0xFFF) / 4096) as usize;
    let mut bounce: u64 = 0;
    let s = (bs.allocate_pages)(0, 2, kpages + INFO_PAGES, &mut bounce);   // AllocateAnyPages, EfiLoaderData
    if s != 0 { fatal("AllocatePages bounce", s); }
    let info = bounce + (kpages as u64) * 4096;
    let (klo, khi) = (paddr, paddr + memsz);
    if bounce < khi && info + (INFO_PAGES as u64) * 4096 > klo { fatal("the bounce buffer overlaps the kernel's range", bounce as usize); }
    unsafe {
        core::ptr::write_bytes(bounce as *mut u8, 0, (kpages + INFO_PAGES) * 4096);
        core::ptr::copy_nonoverlapping(KERNEL.as_ptr().add(off), bounce as *mut u8, filesz as usize);
    }

    // The command line: LoadOptions (UCS-2 -> ASCII), else \talaria.cfg on the boot volume.
    let cl = (info as usize + CL_OFF) as *mut u8;
    let mut li: *mut c_void = core::ptr::null_mut();
    let mut w = 0usize;
    let mut dev: *mut c_void = core::ptr::null_mut();
    let (mut stub_lo, mut stub_hi) = (0u64, 0u64);
    if (bs.handle_protocol)(image, &LOADED_IMAGE_GUID, &mut li) == 0 && !li.is_null() {
        let li = unsafe { &*(li as *const LoadedImage) };
        dev = li.dev;
        stub_lo = li.image_base; stub_hi = li.image_base + li.image_size;
        let n = (li.load_options_size as usize) / 2;
        for i in 0..n {
            let c = unsafe { *li.load_options.add(i) };
            if c == 0 { break; }
            if w < CL_MAX - 1 { unsafe { *cl.add(w) = if c < 0x80 { c as u8 } else { b'?' }; } w += 1; }
        }
    }
    if w == 0 && !dev.is_null() { w = read_cfg(dev, bs, cl); }
    unsafe { *cl.add(w) = 0; }
    puts("uefi-stub: cmdline=\"");
    puts(unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(cl, w)) });
    puts("\"\n");
    if stub_hi == 0 { fatal("no LoadedImage: the stub does not know where it is", 0); }
    if stub_lo < khi && stub_hi > klo { fatal("the stub itself is loaded inside the kernel's range", stub_lo as usize); }

    // The RSDP and the firmware's device tree (edk2 under QEMU publishes one; a server none).
    let (mut rsdp, mut dtb) = (0u64, 0u64);
    unsafe {
        for i in 0..(*st).nr_config {
            let e = &*(*st).config.add(i);
            if e.guid == ACPI2_GUID { rsdp = e.table as u64; }
            if e.guid == ACPI1_GUID && rsdp == 0 { rsdp = e.table as u64; }
            if e.guid == DTB_GUID { dtb = e.table as u64; }
        }
    }
    puts("uefi-stub: rsdp="); puthex(rsdp); puts(" dtb="); puthex(dtb); puts("\n");

    // The firmware's clock, as seconds since the epoch (0 — it has none or would not say).
    let mut epoch = 0u64;
    let rt = unsafe { (*st)._runtime as *const RuntimeServices };
    if !rt.is_null() {
        let mut t = EfiTime::default();
        if unsafe { ((*rt).get_time)(&mut t, core::ptr::null_mut()) } == 0 && t.year >= 2020 {
            let days = days_from_civil(t.year as i64, t.month as i64, t.day as i64);
            let mut secs = days * 86400 + t.hour as i64 * 3600 + t.minute as i64 * 60 + t.second as i64;
            if t.timezone != 2047 { secs -= t.timezone as i64 * 60; }   // 2047: unspecified, taken as UTC
            if secs > 0 { epoch = secs as u64; }
        }
    }

    // GetMemoryMap -> ExitBootServices (retry on a map_key race). Nothing is printed between
    // the two: a console call may allocate and change the key.
    puts("uefi-stub: leaving the firmware (the next line is the kernel's)\n");
    let (mut msize, mut mkey, mut dsize, mut dver);
    let mut tries = 0;
    loop {
        (mkey, dsize, dver) = (0usize, 0usize, 0u32);
        msize = unsafe { (*addr_of!(MAPBUF)).len() };
        let s = (bs.get_memory_map)(&mut msize, addr_of_mut!(MAPBUF) as *mut u8, &mut mkey, &mut dsize, &mut dver);
        if s != 0 { fatal("GetMemoryMap", s); }
        // What the kernel's range holds, judged on the map that is about to become final.
        let ndesc = msize / dsize;
        let (mut ram, mut acpi) = (0u64, false);
        for i in 0..ndesc {
            let d = unsafe { &*((addr_of!(MAPBUF) as *const u8).add(i * dsize) as *const EfiMemDesc) };
            let (a, e) = (d.phys, d.phys + d.pages * 4096);
            if a >= khi || e <= klo { continue; }
            let part = e.min(khi) - a.max(klo);
            match d.typ { 1 | 2 | 3 | 4 | 7 => ram += part, 9 | 10 => acpi = true, _ => {} }
        }
        if acpi { fatal("ACPI tables live inside the kernel's range — it cannot be loaded there", klo as usize); }
        if ram < memsz { fatal("the kernel's range is not RAM on this machine (covered bytes follow)", ram as usize); }
        let s = (bs.exit_boot_services)(image, mkey);
        if s == 0 { break; }
        tries += 1;
        if tries > 8 { fatal("ExitBootServices", s); }
    }
    // ── the firmware is gone: no console, only our own memory ──

    // The EFI map -> PVH MemEntry (RAM: Loader*/BootServices*/Conventional; ACPI reclaim -> 3,
    // NVS -> 4, the rest reserved); adjacent entries of one type are merged.
    let ndesc = msize / dsize;
    let mm = (info as usize + MM_OFF) as *mut MemEntry;
    let mut n = 0usize;
    for i in 0..ndesc {
        let d = unsafe { &*((addr_of!(MAPBUF) as *const u8).add(i * dsize) as *const EfiMemDesc) };
        let typ = match d.typ { 1 | 2 | 3 | 4 | 7 => 1u32, 9 => 3, 10 => 4, _ => 2 };
        let (a, sz) = (d.phys, d.pages * 4096);
        unsafe {
            if n > 0 && (*mm.add(n - 1)).typ == typ && (*mm.add(n - 1)).addr + (*mm.add(n - 1)).size == a {
                (*mm.add(n - 1)).size += sz;
            } else if n < MM_MAX {
                *mm.add(n) = MemEntry { addr: a, size: sz, typ, _r: 0 };
                n += 1;
            }
        }
    }
    unsafe {
        *(info as *mut BootBlock) = BootBlock {
            magic: BOOT_MAGIC, rsdp, dtb, cmdline: info + CL_OFF as u64,
            memmap: info + MM_OFF as u64, memmap_entries: n as u64, epoch_s: epoch,
        };
        let h = addr_of_mut!(HANDOFF);
        *h = Handoff {
            info, src: bounce, dst: paddr, filesz: copysz, memsz, entry,
            stub_lo, stub_hi, info_hi: info + (INFO_PAGES as u64) * 4096,
        };
        tal_enter_kernel(h)
    }
}
