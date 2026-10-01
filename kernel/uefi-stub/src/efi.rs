//! The UEFI interface the stub uses: only the table tails it needs, no dependencies. Shared
//! by the two machines (`x86_64.rs`, `aarch64.rs`) — the firmware's ABI is the same,
//! `extern "efiapi"` is each target's own calling convention.
#![allow(dead_code)]
use core::ffi::c_void;

// ── UEFI FFI (only the needed table tails, no dependencies) ──
pub type Status = usize;
#[repr(C)]
pub struct SystemTable {
    pub _hdr: [u8; 24],
    pub _fw_vendor: *const u16,
    pub _fw_rev: u32,
    pub _con_in_handle: *mut c_void,
    pub _con_in: *mut c_void,
    pub _con_out_handle: *mut c_void,
    pub _con_out: *mut c_void,
    pub _stderr_handle: *mut c_void,
    pub _stderr: *mut c_void,
    pub _runtime: *mut c_void,
    pub boot: *mut BootServices,
    pub nr_config: usize,
    pub config: *mut ConfigEntry,
}
#[repr(C)]
pub struct ConfigEntry { pub guid: [u8; 16], pub table: *mut c_void }
#[repr(C)]
pub struct BootServices {
    pub _hdr: [u8; 24],
    pub _raise_tpl: usize,
    pub _restore_tpl: usize,
    pub allocate_pages: extern "efiapi" fn(u32, u32, usize, *mut u64) -> Status,
    pub _free_pages: usize,
    pub get_memory_map: extern "efiapi" fn(*mut usize, *mut u8, *mut usize, *mut usize, *mut u32) -> Status,
    pub _allocate_pool: usize,
    pub _free_pool: usize,
    pub _evt: [usize; 6],
    pub _install_pi: usize,
    pub _reinstall_pi: usize,
    pub _uninstall_pi: usize,
    pub handle_protocol: extern "efiapi" fn(*mut c_void, *const [u8; 16], *mut *mut c_void) -> Status,
    pub _reserved: usize,
    pub _register_pn: usize,
    pub _locate_handle: usize,
    pub _locate_dp: usize,
    pub _install_ct: usize,
    pub _load_image: usize,
    pub _start_image: usize,
    pub _exit: usize,
    pub _unload_image: usize,
    pub exit_boot_services: extern "efiapi" fn(*mut c_void, usize) -> Status,
}
#[repr(C)]
pub struct LoadedImage {
    pub _rev: u32,
    pub _pad: u32,
    pub _parent: *mut c_void,
    pub _st: *mut c_void,
    pub dev: *mut c_void,
    pub _fp: *mut c_void,
    pub _rsv: *mut c_void,
    pub load_options_size: u32,
    pub _pad2: u32,
    pub load_options: *mut u16,
    pub image_base: u64,
    pub image_size: u64,
}
pub const LOADED_IMAGE_GUID: [u8; 16] = [0xa1, 0x31, 0x1b, 0x5b, 0x62, 0x95, 0xd2, 0x11, 0x8e, 0x3f, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b];
pub const SFS_GUID: [u8; 16] = [0x22, 0x5b, 0x4e, 0x96, 0x59, 0x64, 0xd2, 0x11, 0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b];
#[repr(C)]
pub struct SimpleFs { pub _rev: u64, pub open_volume: extern "efiapi" fn(*mut SimpleFs, *mut *mut File) -> Status }
#[repr(C)]
pub struct File {
    pub _rev: u64,
    pub open: extern "efiapi" fn(*mut File, *mut *mut File, *const u16, u64, u64) -> Status,
    pub close: extern "efiapi" fn(*mut File) -> Status,
    pub _delete: usize,
    pub read: extern "efiapi" fn(*mut File, *mut usize, *mut c_void) -> Status,
}

/// cmdline from \talaria.cfg on the boot volume (booting from the ESP: nobody to give -append).
/// Returns the byte count (0 — no file / not readable).
pub fn read_cfg(dev: *mut c_void, bs: &BootServices, out: *mut u8) -> usize {
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
pub const ACPI2_GUID: [u8; 16] = [0x71, 0xe8, 0x68, 0x88, 0xf1, 0xe4, 0xd3, 0x11, 0xbc, 0x22, 0x00, 0x80, 0xc7, 0x3c, 0x88, 0x81];
pub const ACPI1_GUID: [u8; 16] = [0x30, 0x2d, 0x9d, 0xeb, 0x88, 0x2d, 0xd3, 0x11, 0x9a, 0x16, 0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d];
#[repr(C)]
pub struct EfiMemDesc { pub typ: u32, pub _pad: u32, pub phys: u64, pub _virt: u64, pub pages: u64, pub attr: u64 }


// ── what the arm64 half adds ──
/// The device tree a firmware publishes as a configuration table (edk2 under QEMU does;
/// a server does not): b1b621d5-f19c-41a5-830b-d9152c69aae0.
pub const DTB_GUID: [u8; 16] = [0xd5, 0x21, 0xb6, 0xb1, 0x9c, 0xf1, 0xa5, 0x41, 0x83, 0x0b, 0xd9, 0x15, 0x2c, 0x69, 0xaa, 0xe0];
/// `EFI_SIMPLE_TEXT_OUTPUT_PROTOCOL`: the firmware's console (`SystemTable._con_out`).
#[repr(C)]
pub struct TextOut {
    pub _reset: usize,
    pub output_string: extern "efiapi" fn(*mut TextOut, *const u16) -> Status,
}
/// `EFI_RUNTIME_SERVICES`: only `GetTime` (`SystemTable._runtime`).
#[repr(C)]
pub struct RuntimeServices {
    pub _hdr: [u8; 24],
    pub get_time: extern "efiapi" fn(*mut EfiTime, *mut c_void) -> Status,
}
#[repr(C)]
#[derive(Default)]
pub struct EfiTime {
    pub year: u16, pub month: u8, pub day: u8, pub hour: u8, pub minute: u8, pub second: u8, pub _pad1: u8,
    pub nanosecond: u32, pub timezone: i16, pub daylight: u8, pub _pad2: u8,
}
