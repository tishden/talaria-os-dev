//! The Talaria UEFI stub: a PE application (x86_64-unknown-uefi) without
//! dependencies that carries the kernel ELF inside (include_bytes!). Flow:
//!   1) LoadedImage.LoadOptions -> cmdline (OVMF passes -append via fw_cfg);
//!   2) RSDP from the ConfigurationTable (ACPI 2.0, fallback 1.0) -> start_info.rsdp_paddr
//!      (the kernel uses it for MADT/topology without kvmclock);
//!   3) a bounce buffer below 4 GB (AllocateMaxAddress): the kernel ELF goes there
//!      plus a start_info/memmap/cmdline page — the stub and its statics may live
//!      anywhere, but the 32-bit trampoline needs addresses below 4 GB;
//!   4) GetMemoryMap -> ExitBootServices (retry when map_key changes);
//!   5) the EFI memory map -> the PVH hvm_start_info format (the one mem::init already reads);
//!   6) drop from long mode to protected mode, PAGING OFF, and only now copy the
//!      kernel segment to its physical address (2 MB+): that range is occupied by
//!      UEFI structures (possibly including live page tables — copying over them
//!      with paging on is impossible, and an AllocateAddress reservation fails);
//!      with paging off they are no longer needed;
//!   7) jump to the existing 32-bit pvh_start entry (%ebx = start_info) —
//!      the kernel is unchanged.
//! Debugging goes to COM1 (0x3F8) directly: it works after ExitBootServices too.
#![no_std]
#![no_main]

use core::ffi::c_void;
use core::ptr::{addr_of, addr_of_mut};

static KERNEL: &[u8] = include_bytes!("../../target/x86_64-talaria/release/talaria");

// ── UEFI FFI (only the needed table tails, no dependencies) ──
type Status = usize;
#[repr(C)]
struct SystemTable {
    _hdr: [u8; 24],
    _fw_vendor: *const u16,
    _fw_rev: u32,
    _con_in_handle: *mut c_void,
    _con_in: *mut c_void,
    _con_out_handle: *mut c_void,
    _con_out: *mut c_void,
    _stderr_handle: *mut c_void,
    _stderr: *mut c_void,
    _runtime: *mut c_void,
    boot: *mut BootServices,
    nr_config: usize,
    config: *mut ConfigEntry,
}
#[repr(C)]
struct ConfigEntry { guid: [u8; 16], table: *mut c_void }
#[repr(C)]
struct BootServices {
    _hdr: [u8; 24],
    _raise_tpl: usize,
    _restore_tpl: usize,
    allocate_pages: extern "efiapi" fn(u32, u32, usize, *mut u64) -> Status,
    _free_pages: usize,
    get_memory_map: extern "efiapi" fn(*mut usize, *mut u8, *mut usize, *mut usize, *mut u32) -> Status,
    _allocate_pool: usize,
    _free_pool: usize,
    _evt: [usize; 6],
    _install_pi: usize,
    _reinstall_pi: usize,
    _uninstall_pi: usize,
    handle_protocol: extern "efiapi" fn(*mut c_void, *const [u8; 16], *mut *mut c_void) -> Status,
    _reserved: usize,
    _register_pn: usize,
    _locate_handle: usize,
    _locate_dp: usize,
    _install_ct: usize,
    _load_image: usize,
    _start_image: usize,
    _exit: usize,
    _unload_image: usize,
    exit_boot_services: extern "efiapi" fn(*mut c_void, usize) -> Status,
}
#[repr(C)]
struct LoadedImage {
    _rev: u32,
    _pad: u32,
    _parent: *mut c_void,
    _st: *mut c_void,
    dev: *mut c_void,
    _fp: *mut c_void,
    _rsv: *mut c_void,
    load_options_size: u32,
    _pad2: u32,
    load_options: *mut u16,
    image_base: u64,
    image_size: u64,
}
const LOADED_IMAGE_GUID: [u8; 16] = [0xa1, 0x31, 0x1b, 0x5b, 0x62, 0x95, 0xd2, 0x11, 0x8e, 0x3f, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b];
const SFS_GUID: [u8; 16] = [0x22, 0x5b, 0x4e, 0x96, 0x59, 0x64, 0xd2, 0x11, 0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b];
#[repr(C)]
struct SimpleFs { _rev: u64, open_volume: extern "efiapi" fn(*mut SimpleFs, *mut *mut File) -> Status }
#[repr(C)]
struct File {
    _rev: u64,
    open: extern "efiapi" fn(*mut File, *mut *mut File, *const u16, u64, u64) -> Status,
    close: extern "efiapi" fn(*mut File) -> Status,
    _delete: usize,
    read: extern "efiapi" fn(*mut File, *mut usize, *mut c_void) -> Status,
}

/// cmdline from \talaria.cfg on the boot volume (booting from the ESP: nobody to give -append).
/// Returns the byte count (0 — no file / not readable).
fn read_cfg(dev: *mut c_void, bs: &BootServices, out: *mut u8) -> usize {
    let mut sfs: *mut c_void = core::ptr::null_mut();
    if (bs.handle_protocol)(dev, &SFS_GUID, &mut sfs) != 0 || sfs.is_null() { return 0; }
    let sfs = sfs as *mut SimpleFs;
    let mut root: *mut File = core::ptr::null_mut();
    if unsafe { ((*sfs).open_volume)(sfs, &mut root) } != 0 || root.is_null() { return 0; }
    // "talaria.cfg" in UCS-2
    let mut name = [0u16; 12];
    for (i, b) in b"talaria.cfg".iter().enumerate() { name[i] = *b as u16; }
    let mut f: *mut File = core::ptr::null_mut();
    let r = unsafe { ((*root).open)(root, &mut f, name.as_ptr(), 1 /* READ */, 0) };
    if r != 0 || f.is_null() { unsafe { ((*root).close)(root) }; return 0; }
    let mut n = 511usize;
    let r = unsafe { ((*f).read)(f, &mut n, out as *mut c_void) };
    unsafe { ((*f).close)(f); ((*root).close)(root); }
    if r != 0 { return 0; }
    // cut at \0/\n and garbage characters
    let mut w = 0;
    while w < n {
        let c = unsafe { *out.add(w) };
        if c == 0 || c == b'\n' || c == b'\r' { break; }
        w += 1;
    }
    w
}
const ACPI2_GUID: [u8; 16] = [0x71, 0xe8, 0x68, 0x88, 0xf1, 0xe4, 0xd3, 0x11, 0xbc, 0x22, 0x00, 0x80, 0xc7, 0x3c, 0x88, 0x81];
const ACPI1_GUID: [u8; 16] = [0x30, 0x2d, 0x9d, 0xeb, 0x88, 0x2d, 0xd3, 0x11, 0x9a, 0x16, 0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d];
#[repr(C)]
struct EfiMemDesc { typ: u32, _pad: u32, phys: u64, _virt: u64, pages: u64, attr: u64 }

// ── PVH start_info (the format read by mem::init and main.rs) ──
#[repr(C)]
struct StartInfo {
    magic: u32, version: u32, flags: u32, nr_modules: u32,
    modlist_paddr: u64, cmdline_paddr: u64, rsdp_paddr: u64,
    memmap_paddr: u64, memmap_entries: u32, _reserved: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct MemEntry { addr: u64, size: u64, typ: u32, _r: u32 }

// Layout of the info pages in the bounce buffer (after the kernel image). The offsets
// are duplicated as literals in the asm trampoline below — change them together!
const SI_OFF: usize = 0;            // StartInfo (56 bytes)
const MM_OFF: usize = 64;           // MemEntry[MM_MAX]
const MM_MAX: usize = 256;          // 256*24 = 6144
const CL_OFF: usize = MM_OFF + MM_MAX * 24; // 6208: cmdline (512)
const GDT_OFF: usize = 6720;        // trampoline GDT (3 descriptors)
const GDTD_OFF: usize = 6752;       // GDTR (limit+base)
const PB_OFF: usize = 6784;         // parameters: entry,src,dst,filesz,zerosz (u64 x5)
const TR_OFF: usize = 6912;         // copy of the 32-bit trampoline (<=256 bytes)
const INFO_PAGES: u64 = 2;

static mut MAPBUF: [u8; 32768] = [0; 32768];

// ── COM1 directly (works before and after ExitBootServices) ──
fn outb(port: u16, v: u8) { unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") v, options(nostack, nomem)) } }
fn sputc(c: u8) { outb(0x3F8, c) }
fn sputs(s: &str) { for b in s.bytes() { if b == b'\n' { sputc(b'\r'); } sputc(b); } }
fn sputhex(v: u64) {
    sputs("0x");
    for i in (0..16).rev() {
        let d = ((v >> (i * 4)) & 0xF) as u8;
        sputc(if d < 10 { b'0' + d } else { b'a' + d - 10 });
    }
}

fn fatal(msg: &str, st: Status) -> ! {
    sputs("uefi-stub: FATAL "); sputs(msg); sputs(" status="); sputhex(st as u64); sputs("\n");
    loop { unsafe { core::arch::asm!("hlt") } }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! { fatal("panic", 0) }

// ── ELF64 (a mini PT_LOAD parser) ──
fn le16(b: &[u8], o: usize) -> u16 { u16::from_le_bytes([b[o], b[o + 1]]) }
fn le32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o+1], b[o+2], b[o+3]]) }
fn le64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes([b[o], b[o+1], b[o+2], b[o+3], b[o+4], b[o+5], b[o+6], b[o+7]]) }

extern "sysv64" {
    // the only argument is the info page base (< 4 GB); parameters at PB_OFF.
    // IMPORTANT: on x86_64-unknown-uefi extern "C" = win64 (arg1 in RCX) — the asm
    // below reads RDI, hence the explicit sysv64 convention
    fn drop_to_pvh(info: u64) -> !;
    static tramp32_start: u8;
    static tramp32_end: u8;
}

core::arch::global_asm!(r#"
.section .text
.global drop_to_pvh
/* rdi = info (< 4 GB). The stub may be loaded above 4 GB, so the GDT and the
   32-bit code run from a COPY in the info page (placed by efi_main):
   GDT@+6720, GDTR@+6752, parameters@+6784, trampoline@+6912. Only the 64-bit
   part is here: lgdt on the copy and a far return into the trampoline copy. */
drop_to_pvh:
    cli
    mov dx, 0x3F8
    mov al, 0x41                /* 'A': entered drop_to_pvh */
    out dx, al
    lea rax, [rdi + 6752]
    lgdt [rax]
    mov al, 0x42                /* 'B': GDT loaded, far return into the trampoline */
    out dx, al
    mov rbp, rdi                /* info: base for the 32-bit code */
    lea rax, [rdi + 6912]
    push 0x08
    push rax
    retfq

/* The 32-bit position-independent trampoline: copied to info+6912.
   ebp = info. Paging off -> copy the kernel to its physical address -> pvh_start. */
.global tramp32_start
.global tramp32_end
.code32
tramp32_start:
    mov dx, 0x3F8               /* debugging: trampoline stages to COM1 */
    mov al, 0x31                /* '1': 32-bit CS active */
    out dx, al
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax
    mov eax, cr0
    and eax, 0x7FFFFFFF         /* PG=0 -> leave long mode */
    mov cr0, eax
    mov ecx, 0xC0000080         /* EFER.LME=0 */
    rdmsr
    and eax, 0xFFFFFEFF
    wrmsr
    mov dx, 0x3F8
    mov al, 0x32                /* '2': paging off, long mode left */
    out dx, al
    mov esi, [ebp + 6792]       /* src (bounce) */
    mov edi, [ebp + 6800]       /* dst (p_paddr) */
    mov ecx, [ebp + 6808]       /* filesz */
    cld
    rep movsb                   /* kernel: bounce -> PT_LOAD physical address */
    mov ecx, [ebp + 6816]       /* zerosz */
    xor eax, eax
    rep stosb                   /* .bss */
    mov dx, 0x3F8
    mov al, 0x33                /* '3': kernel copied */
    out dx, al
    mov eax, [ebp + 6784]
    mov ebx, ebp                /* start_info = info+0 (PVH contract: %ebx) */
    jmp eax                     /* pvh_start */
tramp32_end:
.code64
"#);

#[no_mangle]
pub extern "efiapi" fn efi_main(image: *mut c_void, st: *mut SystemTable) -> Status {
    sputs("uefi-stub: Talaria UEFI boot, kernel ");
    sputhex(KERNEL.len() as u64); sputs(" bytes\n");
    let bs = unsafe { &*(*st).boot };

    // ELF: a single PT_LOAD (linker.ld: PHDRS text+note) and the entry
    if &KERNEL[..4] != b"\x7fELF" { fatal("not an ELF", 0); }
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
    if nload != 1 { fatal("expected a single PT_LOAD (see linker.ld PHDRS)", nload); }
    if entry != paddr || paddr + memsz >= 0x1_0000_0000 { fatal("kernel entry/range", 0); }
    sputs("uefi-stub: kernel phys "); sputhex(paddr); sputs(".."); sputhex(paddr + memsz);
    sputs(" entry="); sputhex(entry); sputs("\n");

    // A bounce buffer < 4 GB: the ELF image + info pages (start_info/memmap/cmdline).
    // The stub may have been loaded above 4 GB (metal with lots of RAM) — the 32-bit
    // trampoline needs src/start_info below it; AllocateMaxAddress (1) guarantees that.
    let kpages = ((filesz + 0xFFF) / 4096) as usize;
    let mut bounce: u64 = 0xFFFF_F000;
    // type EfiLoaderCode (1): OVMF with DXE protection sets NX on Data pages,
    // and the 32-bit trampoline executes from the info page
    let s = (bs.allocate_pages)(1, 1, kpages + INFO_PAGES as usize, &mut bounce);
    if s != 0 { fatal("AllocatePages bounce", s); }
    unsafe { core::ptr::copy_nonoverlapping(KERNEL.as_ptr().add(off), bounce as *mut u8, filesz as usize); }
    let info = bounce + (kpages as u64) * 4096;

    // cmdline from LoadOptions (UCS-2 -> ASCII) -> the info page
    let cl = (info as usize + CL_OFF) as *mut u8;
    let mut li: *mut c_void = core::ptr::null_mut();
    let mut w = 0usize;
    let mut dev: *mut c_void = core::ptr::null_mut();
    if (bs.handle_protocol)(image, &LOADED_IMAGE_GUID, &mut li) == 0 && !li.is_null() {
        let li = unsafe { &*(li as *const LoadedImage) };
        dev = li.dev;
        let n = (li.load_options_size as usize) / 2;
        for i in 0..n {
            let c = unsafe { *li.load_options.add(i) };
            if c == 0 { break; }
            if w < 511 { unsafe { *cl.add(w) = if c < 0x80 { c as u8 } else { b'?' }; } w += 1; }
        }
    }
    // booting from the ESP (no fw_cfg -append): cmdline from \talaria.cfg on the boot volume
    if w == 0 && !dev.is_null() { w = read_cfg(dev, bs, cl); }
    if w == 0 { for (i, b) in b"console=ttyS0".iter().enumerate() { unsafe { *cl.add(i) = *b; } w = 13; } }
    unsafe { *cl.add(w) = 0; }
    sputs("uefi-stub: cmdline=\"");
    for i in 0..w { unsafe { sputc(*cl.add(i)); } }
    sputs("\"\n");

    // RSDP from the UEFI configuration tables (ACPI 2.0, fallback 1.0)
    let mut rsdp = 0u64;
    unsafe {
        for i in 0..(*st).nr_config {
            let e = &*(*st).config.add(i);
            if e.guid == ACPI2_GUID { rsdp = e.table as u64; break; }
            if e.guid == ACPI1_GUID && rsdp == 0 { rsdp = e.table as u64; }
        }
    }
    sputs("uefi-stub: rsdp="); sputhex(rsdp); sputs("\n");

    // GetMemoryMap -> ExitBootServices (retry on a map_key race)
    let (mut msize, mut mkey, mut dsize, mut dver) = (0usize, 0usize, 0usize, 0u32);
    let mut tries = 0;
    loop {
        msize = unsafe { MAPBUF.len() };
        let s = (bs.get_memory_map)(&mut msize, unsafe { addr_of_mut!(MAPBUF) as *mut u8 }, &mut mkey, &mut dsize, &mut dver);
        if s != 0 { fatal("GetMemoryMap", s); }
        let s = (bs.exit_boot_services)(image, mkey);
        if s == 0 { break; }
        tries += 1;
        if tries > 8 { fatal("ExitBootServices", s); }
    }
    // ── BootServices are dead: only COM1 directly and our own memory ──

    // EFI memory map -> PVH MemEntry (RAM: Loader*/BootServices*/Conventional;
    // ACPI reclaim -> 3, NVS -> 4, the rest -> reserved); adjacent entries of one type merged
    let ndesc = msize / dsize;
    let mm = (info as usize + MM_OFF) as *mut MemEntry;
    let mut n = 0usize;
    for i in 0..ndesc {
        let d = unsafe { &*((addr_of!(MAPBUF) as *const u8).add(i * dsize) as *const EfiMemDesc) };
        let typ = match d.typ { 1 | 2 | 3 | 4 | 7 => 1u32, 9 => 3, 10 => 4, _ => 2 };
        let (a, sz) = (d.phys, d.pages * 4096);
        unsafe {
            if n > 0 && (*mm.add(n-1)).typ == typ && (*mm.add(n-1)).addr + (*mm.add(n-1)).size == a {
                (*mm.add(n-1)).size += sz;
            } else if n < MM_MAX {
                *mm.add(n) = MemEntry { addr: a, size: sz, typ, _r: 0 };
                n += 1;
            }
        }
    }
    let si = (info as usize + SI_OFF) as *mut StartInfo;
    unsafe {
        *si = StartInfo {
            magic: 0x336E_C578, version: 1, flags: 0, nr_modules: 0, modlist_paddr: 0,
            cmdline_paddr: info + CL_OFF as u64, rsdp_paddr: rsdp,
            memmap_paddr: info + MM_OFF as u64, memmap_entries: n as u32, _reserved: 0,
        };
    }
    // GDT/GDTR/parameters/the copy of the 32-bit trampoline go to the info page (<4 GB):
    // the stub itself may live above 4 GB, and 32-bit code cannot execute from there
    unsafe {
        let gdt = (info as usize + GDT_OFF) as *mut u64;
        *gdt = 0; *gdt.add(1) = 0x00CF9A000000FFFF; *gdt.add(2) = 0x00CF92000000FFFF;
        let gdtd = (info as usize + GDTD_OFF) as *mut u8;
        *(gdtd as *mut u16) = 23;
        *(gdtd.add(2) as *mut u64) = info + GDT_OFF as u64;
        let pb = (info as usize + PB_OFF) as *mut u64;
        *pb = entry; *pb.add(1) = bounce; *pb.add(2) = paddr;
        *pb.add(3) = filesz; *pb.add(4) = memsz - filesz;
        let t0 = addr_of!(tramp32_start) as *const u8;
        let tlen = (addr_of!(tramp32_end) as usize) - (t0 as usize);
        if tlen > 256 { fatal("trampoline larger than 256 bytes", tlen); }
        core::ptr::copy_nonoverlapping(t0, (info as usize + TR_OFF) as *mut u8, tlen);
        sputs("uefi-stub: tramp len="); sputhex(tlen as u64);
        sputs(" head="); sputhex(*((info as usize + TR_OFF) as *const u64));
        sputs(" gdtd="); sputhex(*((info as usize + GDTD_OFF + 2) as *const u64)); sputs("\n");
    }
    sputs("uefi-stub: bounce="); sputhex(bounce); sputs(" info="); sputhex(info); sputs("\n");
    sputs("uefi-stub: ExitBootServices OK, memmap entries=");
    sputhex(n as u64); sputs(", jumping to pvh_start\n");
    unsafe { drop_to_pvh(info + SI_OFF as u64) }
}
