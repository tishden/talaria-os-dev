//! virtio-blk + a minimal read-only FS: TFS1 (flat, names of 24 bytes, 16 files) and
//! TFS2 (paths up to 83 bytes, directories, up to 128 entries — for production workloads with a tree
//! of the workload's configs/secrets). Plus the kernel's virtual files (/etc/resolv.conf,
//! /etc/hosts — they are generated from the network configuration). The file syscalls
//! open/read/lseek/fstat/stat/close.
//!
//! TFS1: block 0 = the magic u32 "TFS1", count u32, count*{name[24], lba u32, len u32}.
//! TFS2: block 0 = the magic u32 "TFS2", count u32, dirsec u32 (the directory's sectors, including
//!       block 0); the entries {name[84], lba u32, len u32, flags u32} of 96 bytes in a row from
//!       byte 16 of block 0 through the sectors 0..dirsec-1; flags bit0 = a directory. The files go from LBA dirsec.
//!
//! # Safety
//!
//! This module hands buffers to a DEVICE and takes them back. A mistake does not
//! show up as a crash but as the wrong bytes: a slot reused before its command
//! completed returns the PREVIOUS command's status (an "OK" for a read that never
//! happened), and a buffer freed while the device still owns it is written into
//! from outside the kernel's control. The `unsafe` rests on four invariants, named
//! D1..D4 at the sites.
//!
//! - **D1. The ring and the slot table are taken under `ring_lock`.** The driver
//!   object and the table of in-flight commands (`IOS`) are one per machine;
//!   `slot_take`/`reap_locked` are declared "only under the lock", and the
//!   functions that wait (`slot_wait`) deliberately do NOT hold it, so the
//!   neighbours can submit and reap while we wait.
//! - **D2. A slot's buffer belongs to the DEVICE until the command completes.** A
//!   slot is released only when its completion has been seen (`done`), and a
//!   command that has not completed in ten seconds takes its slot OUT of
//!   circulation instead of freeing it: the device may still be writing there, and
//!   the honest answer to the caller is a refusal, not a stale status.
//! - **D3. A physical address handed to the device is a frame the kernel owns.**
//!   The buffers come from the block cache or from the journal's staging pages, the
//!   kernel identity-maps them, and nothing else uses them for the length of the
//!   command.
//! - **D4. `static mut` here is the device layer's own state** (the driver, the
//!   slots, the partition base): written on the initialisation path or under
//!   `ring_lock`, read through `addr_of!` so that no reference to it is ever
//!   formed.
use core::ptr::{addr_of_mut, NonNull};
use virtio_drivers::device::blk::{VirtIOBlk, SECTOR_SIZE};
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};
use virtio_drivers::transport::{DeviceType, Transport};
use crate::hal::TalariaHal;
#[allow(unused_imports)]
use crate::serial;

type VBlk = VirtIOBlk<TalariaHal, MmioTransport<'static>>;
/// The disk's block device: virtio-blk (QEMU microvm) or NVMe (EC2/EBS, §4.5).
/// A single Result interface — the callers do not know what is under them.
enum Blk { Virtio(VBlk), Nvme(crate::nvme::Nvme) }
impl Blk {
    fn read_blocks(&mut self, lba: usize, buf: &mut [u8]) -> Result<(), ()> {
        let lba = lba + part_base();
        self.read_raw(lba, buf)
    }
    /// A read WITHOUT the partition's offset (the GPT headers).
    fn read_raw(&mut self, lba: usize, buf: &mut [u8]) -> Result<(), ()> {
        match self {
            Blk::Virtio(b) => b.read_blocks(lba, buf).map_err(|_| ()),
            // NVMe goes through the same queue as everything else: its own CID,
            // a short lock of the ring, a wait for ITS OWN completion.
            Blk::Nvme(n) => {
                let dev: *mut crate::nvme::Nvme = n;
                // SAFETY: D1 + D2 — the ring and the slot table under `ring_lock`; a slot is reused
                // only after its completion has been seen.
                if unsafe { nvme_rw_queued(dev, false, lba, buf.as_mut_ptr() as u64, buf.len()) } { Ok(()) } else { Err(()) }
            }
        }
    }
    /// A write WITHOUT the partition's offset (the trace of `talaria.trace=` in the gap before the kernel).
    fn write_raw(&mut self, lba: usize, buf: &[u8]) -> Result<(), ()> {
        match self {
            Blk::Virtio(b) => b.write_blocks(lba, buf).map_err(|_| ()),
            Blk::Nvme(n) => {
                let dev: *mut crate::nvme::Nvme = n;
                // SAFETY: D1 + D2 — the ring and the slot table under `ring_lock`; a slot is reused
                // only after its completion has been seen.
                if unsafe { nvme_rw_queued(dev, true, lba, buf.as_ptr() as u64, buf.len()) } { Ok(()) } else { Err(()) }
            }
        }
    }
    fn flush(&mut self) -> Result<(), ()> {
        match self {
            Blk::Virtio(b) => b.flush().map_err(|_| ()),
            Blk::Nvme(n) => {
                let dev: *mut crate::nvme::Nvme = n;
                // SAFETY: D1 + D2 — the ring and the slot table under `ring_lock`; a slot is reused
                // only after its completion has been seen.
                if unsafe { nvme_flush_queued(dev) } { Ok(()) } else { Err(()) }
            }
        }
    }
}
static mut BLK: Option<Blk> = None;

/// The single point of access to the block device.
///
/// Through `addr_of_mut!` rather than `&mut BLK`: a reference to a `static mut` is UB if
/// another one lives next to it, and since edition 2024 that is already a compilation error. Previously
/// there were thirteen such borrows across the file; now there is one, and next to it
/// it is written what keeps the invariant "the reference is one".
///
/// The invariant is kept by `Busy`: the driver is not reentrant (a second command on top of
/// the first hangs both), so every operation takes the busy flag, and everything
/// that may cut in — the trace to the disk first of all — respects it.
#[inline]
// SAFETY: D1 + D2 — the ring and the slot table under `ring_lock`; a slot is reused only after its
// completion has been seen.
fn blk() -> Option<&'static mut Blk> { unsafe { (*core::ptr::addr_of_mut!(BLK)).as_mut() } }
/// Is the FS mounted. Separately from `BLK`: the device may be brought up before the
/// mounting (the early trace of `talaria.trace=`), and then `init` must go through
/// the partition parsing rather than leave by "it is already mounted".
static mut MOUNTED: bool = false;
// The base of the data partition (LBA): 0 — a raw disk (microvm), with GPT (the mk-ami image:
// an ESP + the data) — the start of the SECOND partition. It is applied in the blk wrappers below.
static mut PART_BASE: usize = 0;
// SAFETY: D3 — the buffer handed to the device is a frame the kernel owns (a block-cache slot or a
// staging page), untouched for the length of the command.
fn part_base() -> usize { unsafe { PART_BASE } }

const MAXF: usize = 128;
pub use crate::tfs::NAMELEN;
use crate::tfs::{Entry, E0, normalize};
const FD_BASE: i64 = 300;
static mut DIR: [Entry; MAXF] = [E0; MAXF];
static mut NFILES: usize = 0;
const MAXOPEN: usize = 64;
static mut OF_DIR: [i32; MAXOPEN] = [-1; MAXOPEN]; // the index in DIR (or VF_BASE+k for a virtual one) per open descriptor, -1 = free
static mut OF_OFF: [u32; MAXOPEN] = [0; MAXOPEN];  // the descriptor's read offset (fd = FD_BASE+h)
static mut DIRBUF: [u8; 32 * SECTOR_SIZE] = [0; 32 * SECTOR_SIZE];

// ── the kernel's virtual files (the content is generated at init) ─────────────
const VF_N: usize = 3;
const VF_BASE: i32 = 10000;
const VF_MAX: usize = 512;
static VF_NAME: [&[u8]; VF_N] = [b"etc/resolv.conf", b"etc/hosts", b"proc/self/cmdline"];
static mut VF_DATA: [[u8; VF_MAX]; VF_N] = [[0; VF_MAX]; VF_N];
static mut VF_LEN: [usize; VF_N] = [0; VF_N];
fn vf_set(k: usize, parts: &[&[u8]]) {
    // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
    // through `addr_of!` so that no reference to it is formed.
    unsafe {
        let mut n = 0;
        for p in parts { let m = p.len().min(VF_MAX - n); VF_DATA[k][n..n + m].copy_from_slice(&p[..m]); n += m; }
        VF_LEN[k] = n;
    }
}
fn dec(mut v: u8, out: &mut [u8; 3]) -> usize { // u8 -> a decimal string
    let mut n = 0; let mut tmp = [0u8; 3];
    if v == 0 { out[0] = b'0'; return 1; }
    while v > 0 { tmp[n] = b'0' + v % 10; v /= 10; n += 1; }
    for i in 0..n { out[i] = tmp[n - 1 - i]; }
    n
}
/// Generate /etc/resolv.conf and /etc/hosts from the kernel's network configuration.
pub fn vf_init_net(dns: [u8; 4]) {
    let mut ip = [0u8; 16]; let mut n = 0;
    for (i, o) in dns.iter().enumerate() { let mut d = [0u8; 3]; let m = dec(*o, &mut d); ip[n..n + m].copy_from_slice(&d[..m]); n += m; if i < 3 {
        ip[n] = b'.'; n += 1; } }
    vf_set(0, &[b"nameserver ", &ip[..n], b"\noptions timeout:3 attempts:3\n"]);
    vf_set(1, &[b"127.0.0.1 localhost\n"]);
    vf_set(2, &[b"talaria\0"]);
}

fn scan_blk() -> Option<(usize, usize)> {
    for i in 0..32usize {
        let addr = 0xFEB0_0000 + i * 512;
        // SAFETY: D3 — the buffer handed to the device is a frame the kernel owns (a block-cache slot
        // or a staging page), untouched for the length of the command.
        let magic = unsafe { core::ptr::read_volatile(addr as *const u32) };
        if magic != 0x7472_6976 { continue; }
        let header = NonNull::new(addr as *mut VirtIOHeader)?;
        // SAFETY: D3 — the buffer handed to the device is a frame the kernel owns (a block-cache slot
        // or a staging page), untouched for the length of the command.
        if let Ok(t) = unsafe { MmioTransport::new(header, 512) } {
            let blk = t.device_type() == DeviceType::Block;
            // MmioTransport's `Drop` resets the device. Scanning the windows is
            // AN INSPECTION, and extinguishing live devices with it (the network cards stand in
            // the same windows) is not allowed: on a machine where the disk lies after the cards, the walk
            // would still extinguish the network when the FS was mounted.
            core::mem::forget(t);
            if blk { return Some((addr, 512)); }
        }
    }
    None
}

/// Is the block device brought up.
pub fn blk_ready() -> bool { blk().is_some() }

/// Bring up the block device itself (without parsing the partitions and mounting).
/// Separately from `init` for the sake of the early trace of `talaria.trace=`: on metal one must be able
/// to write to the disk BEFORE it is known whether there is a file system on it.
pub fn blk_open() -> bool {
    if blk().is_some() { return true; }
    // virtio-mmio (microvm) -> NVMe over PCIe (EC2/EBS, OVMF q35; talaria.pci=1 is needed
    // or a call of pci::init() before fs::init — otherwise there are no devices in the table)
    if let Some((addr, sz)) = scan_blk() {
        let header = NonNull::new(addr as *mut VirtIOHeader).unwrap();
        // SAFETY: D1 + D2 — the ring and the slot table under `ring_lock`; a slot is reused only after
        // its completion has been seen.
        let transport = unsafe { MmioTransport::new(header, sz).unwrap() };
        match VirtIOBlk::new(transport) {
            // SAFETY: D1 + D2 — the ring and the slot table under `ring_lock`; a slot is reused only
            // after its completion has been seen.
            Ok(b) => unsafe { BLK = Some(Blk::Virtio(b)) },
            Err(_) => { println!("talaria: virtio-blk init fail"); return false; }
        }
    } else if !open_nvme_boot() {
        println!("talaria: the disk was not found (virtio-blk/nvme)"); return false;
    }
    true
}

/// Choose the BOOT NVMe controller (A9).
///
/// On Nitro guests and under QEMU there is one controller, and "the first one that turns up" was always
/// the right one. On bare metal there are several of them (with a Nitro metal every EBS volume is its own
/// controller), and the first one by the order of the buses may not be the one we were
/// booted from: then the kernel dutifully writes the trace onto A FOREIGN disk, while from the outside that is
/// indistinguishable from "the kernel is silent".
///
/// The rule of the choice: we read sector 0 and look for our bootloader's signature (`TBOT` at
/// offset 4). Found it — this is our disk. Found it at nobody's — we take the first one whose
/// sector 0 at least reads and ends with 0x55AA (the GPT images
/// from mk-ami), otherwise simply the first live one.
fn open_nvme_boot() -> bool {
    let total = crate::pci::count_class(0x010802);
    if total > 1 { println!("talaria: there are {} NVMe controllers — looking for the one we booted from", total); }
    let mut fallback: Option<crate::nvme::Nvme> = None;
    for i in 0..total.max(1) {
        let mut n = match crate::nvme::probe_nth(i) { Some(n) => n, None => continue };
        let mut sec = [0u8; SECTOR_SIZE];
        if !n.read_blocks_boot(0, &mut sec) { println!("talaria: NVMe #{}: sector 0 does not read", i); continue; }
        // We look for the signature THROUGHOUT the code of sector 0: in stage1 "TBOT" is an immediate
        // operand of the instruction that verifies stage2's signature, and it lies at offset
        // 0x23 rather than at the start (the first version of the check looked at bytes 4..8 and
        // found nothing even locally).
        if sec[..446].windows(4).any(|w| w == b"TBOT") {
            println!("talaria: NVMe #{} is the boot one (the TBOT signature in sector 0)", i);
            // SAFETY: D1 + D2 — the ring and the slot table under `ring_lock`; a slot is reused only
            // after its completion has been seen.
            unsafe { BLK = Some(Blk::Nvme(n)); }
            return true;
        }
        let mbr = sec[510] == 0x55 && sec[511] == 0xAA;
        if fallback.is_none() || mbr { fallback = Some(n); }
    }
    match fallback {
        Some(n) => { println!("talaria: there is no bootloader signature on any NVMe — taking the first suitable one");
                     // SAFETY: D1 + D2 — the ring and the slot table under `ring_lock`; a slot is
                     // reused only after its completion has been seen.
                     unsafe { BLK = Some(Blk::Nvme(n)); } true }
        None => false,
    }
}

pub fn init() -> bool {
    // SAFETY: D3 — the buffer handed to the device is a frame the kernel owns (a block-cache slot or a
    // staging page), untouched for the length of the command.
    if unsafe { MOUNTED } { return true; } // it is already mounted
    if !blk_open() { return false; }
    // The data partition: GPT (the mk-ami/hybrid images) or MBR (the mk-bios image, §3.2).
    // In GPT we prefer the partition BY THE NAME "talaria-data" — in a hybrid image it
    // is no longer the second one (a BIOS boot and an ESP are before it); the fallback is the second partition, as it was.
    // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
    // through `addr_of!` so that no reference to it is formed.
    unsafe {
        PART_BASE = 0;
        let blk = blk().unwrap();
        let hdr = &mut *addr_of_mut!(SECTBUF);
        let found = crate::tfs::data_partition(|lba, buf| {
            if blk.read_raw(lba, &mut hdr[..]).is_err() { return false; }
            buf.copy_from_slice(&hdr[..crate::tfs::SECTOR]);
            true
        });
        if let Some((first, from)) = found {
            PART_BASE = first;
            match from {
                crate::tfs::PartFrom::GptNamed => println!("talaria: GPT, the data partition \"talaria-data\" from LBA {}", first),
                crate::tfs::PartFrom::GptSecond => println!("talaria: GPT, the data partition (the second one) from LBA {}", first),
                crate::tfs::PartFrom::Mbr => println!("talaria: MBR, the data partition from LBA {}", first),
            }
        }
    }
    let blk = blk().unwrap();
    // SAFETY: D1 + D2 — the ring and the slot table under `ring_lock`; a slot is reused only after its
    // completion has been seen.
    let db = unsafe { &mut *addr_of_mut!(DIRBUF) };
    if blk.read_blocks(0, &mut db[..SECTOR_SIZE]).is_err() { println!("talaria: blk read0 fail"); return false; }
    let magic = u32::from_le_bytes([db[0], db[1], db[2], db[3]]);
    let mut n = 0;
    match crate::tfs::tfs_header(&db[..SECTOR_SIZE]) {
        Some((kind, count)) => {
            if let crate::tfs::Tfs::V2 { dirsec } = kind {
                for sct in 1..dirsec {
                    if blk.read_blocks(sct, &mut db[sct * SECTOR_SIZE..(sct + 1) * SECTOR_SIZE]).is_err() { println!("talaria: blk read dir fail");
                        return false; }
                }
            }
            for i in 0..count.min(MAXF) {
                match crate::tfs::tfs_entry(&db[..], kind, i) {
                    // SAFETY: D4 — this module's own state (the driver, the slots, the partition
                    // base), reached through `addr_of!` so that no reference to it is formed.
                    Some(e) => { unsafe { (*addr_of_mut!(DIR))[i] = e; } n = i + 1; }
                    None => break,
                }
            }
        }
        None if crate::ext2::mount() => {
            // SAFETY: D1 + D2 — the ring and the slot table under `ring_lock`; a slot is reused only
            // after its completion has been seen.
            unsafe { MOUNTED = true; }
            return true; // ext2 RW: the file operations go through ext2.rs
        }
        None => {
            // SAFETY: D1 + D2 — the ring and the slot table under `ring_lock`; a slot is reused only
            // after its completion has been seen.
            println!("talaria: there is no TFS1/TFS2/ext2 (magic={:#x})", magic); unsafe { BLK = None; } return false;
        }
    }
    // SAFETY: D3 — the buffer handed to the device is a frame the kernel owns (a block-cache slot or a
    // staging page), untouched for the length of the command.
    unsafe { NFILES = n; MOUNTED = true; }
    println!("talaria: {} is mounted — {} files on virtio-blk", if magic == 0x3153_4654 { "TFS1" } else { "TFS2" }, n);
    true
}

/// A12: the block layer IS BUSY (somebody is already submitting a command and waiting for it). The driver
/// is not reentrant: a second command submitted from an interrupt handler on top of
/// the first hangs both. The flag is raised for the duration of every operation, and the trace to the disk
/// respects it (`trace::maybe_flush`). That way the diagnostics do not break the object
/// of observation — on `c6i.metal` that cost the production mode and several runs.
static BLK_BUSY: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
pub fn blk_busy() -> bool { BLK_BUSY.load(core::sync::atomic::Ordering::Relaxed) != 0 }
/// The busy guard: it raises the flag for the duration of an operation.
struct Busy;
impl Busy {
    fn new() -> Self { BLK_BUSY.fetch_add(1, core::sync::atomic::Ordering::Relaxed); Busy }
}
impl Drop for Busy {
    fn drop(&mut self) { BLK_BUSY.fetch_sub(1, core::sync::atomic::Ordering::Relaxed); }
}

// ── THE QUEUE'S DEPTH: SEVERAL COMMANDS IN FLIGHT ───────────────────────────
//
// WHAT FOR. The block layer worked with a queue OF DEPTH ONE: `add_notify_wait_pop`
// submits a command and spins until it completes. While a barrier is going (22-46 ms on
// this stand: for the host that is an fdatasync over the image), the queue is busy entirely, and not
// a single read gets past it — however much one divides the locks. Hence the tail of
// a mongodb insert: 136 ms at p99.9 against Linux's 0.5 ms on the same disk.
//
// HOW IT IS ARRANGED. The ring is touched only under A SHORT lock (submit a command or
// take the ready completions off), while EVERYONE WAITS for their own completion WITHOUT the lock. The one who took
// a completion off is not necessarily its owner: `pop_any` returns a token, and the one who
// waits for it recognises its own by the table of slots. That way one's barrier and another's read
// go simultaneously, while the order "my writes -> my barrier -> my commit"
// is preserved by everyone waiting for THEIR OWN completions.
//
// The depth is chosen by the size of the device's queue (16 with virtio-blk in this
// build): more cannot be submitted anyway, `add` will return QueueFull.
const QD: usize = 16;
/// The slot of a command in flight: the request/response buffers live HERE rather than on the stack —
/// the device writes into them after the caller has already left its frame
/// (unlike a queue of depth one, where the wait ended on the same frame).
struct IoSlot {
    req: virtio_drivers::device::blk::BlkReq,
    resp: virtio_drivers::device::blk::BlkResp,
    token: u16,
    busy: bool,
    done: bool,
}
static mut IOS: [Option<IoSlot>; QD] = [const { None }; QD];
/// A short lock OF THE RING (the submission and the taking of completions). It is held for nanoseconds —
/// unlike the device's lock, which is held for the whole command.
static RING_OWNER: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(-1);
/// The waits for the ring longer than a second (a diagnosis into the console) and the bypasses from a panic.
/// The same store as at `dev_lock`: previously after a second a guard was returned WITHOUT
/// the lock, and its Drop released A FOREIGN ring lock — two cores submitted into
/// the virtqueue simultaneously, without a single counter.
pub static RING_TIMEOUT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static RING_BYPASS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Only for the gate `ringlockdiag`: hold the ring's lock from another core.
pub fn ring_hold_for_test() -> impl Drop { ring_lock() }
struct RingGuard(bool);
impl Drop for RingGuard {
    fn drop(&mut self) { if self.0 { RING_OWNER.store(-1, core::sync::atomic::Ordering::SeqCst); } }
}
fn ring_lock() -> RingGuard {
    use core::sync::atomic::Ordering;
    let me = crate::percpu::cpu_id() as i32;
    let hz = crate::tsc::hz();
    let limit = if hz != 0 { hz } else { 3_000_000_000 };
    let start = crate::tsc::rdtsc();
    let mut next_report = limit;
    let mut reports = 0u32;
    loop {
        if RING_OWNER.compare_exchange(-1, me, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            return RingGuard(true);
        }
        crate::paging::tlb_service();
        core::hint::spin_loop();
        let waited = crate::tsc::rdtsc().wrapping_sub(start);
        if waited > next_report {
            if reports == 0 { RING_TIMEOUT.fetch_add(1, Ordering::Relaxed); }
            if crate::panicking() { RING_BYPASS.fetch_add(1, Ordering::Relaxed); return RingGuard(false); }
            crate::println!("fs: the ring's lock did not give in within {} s: core {} holds it (we are core {}); we keep waiting (give-ups 0)",
                waited / hz.max(1), RING_OWNER.load(Ordering::SeqCst), me);
            reports += 1;
            next_report = waited + limit * 10;
        }
    }
}
/// Take off all the ready completions and mark their slots. It is called ONLY under
/// `ring_lock`.
///
/// # Safety
/// D1 + D2: the caller holds `ring_lock` (the name says so), so the ring and the slot
/// table are ours; a slot is marked `done` only for the token the device returned.
unsafe fn reap_locked(b: &mut VBlk) {
    while let Some(tok) = b.pop_any() {
        for sl in (*core::ptr::addr_of_mut!(IOS)).iter_mut().flatten() { if sl.busy && sl.token == tok { sl.done = true; } }
    }
}
/// How many commands were submitted and how many times a submission ran into a full queue.
pub static IO_SUBMITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static IO_QFULL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The greatest number of commands that were simultaneously in flight (a measure of the fact that the
/// depth is used at all: a one would mean that nothing has changed).
pub static IO_DEPTH_MAX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Occupy a slot for a command (under `ring_lock`).
///
/// # Safety
/// D1 + D2 + D4: under `ring_lock`; only a slot that is not `busy` is handed out, so a
/// buffer the device may still own is never reused.
unsafe fn slot_take() -> Option<usize> {
    let ios = &mut *core::ptr::addr_of_mut!(IOS);
    let mut inflight = 0u64;
    let mut free = None;
    for (i, s) in ios.iter_mut().enumerate() {
        match s {
            Some(sl) if sl.busy => inflight += 1,
            _ => if free.is_none() { free = Some(i); },
        }
    }
    let i = free?;
    if ios[i].is_none() {
        ios[i] = Some(IoSlot {
            req: Default::default(),
            resp: Default::default(),
            token: 0, busy: false, done: false,
        });
    }
    let sl = ios[i].as_mut().unwrap();
    sl.busy = true; sl.done = false; sl.token = 0;
    inflight += 1;
    if inflight > IO_DEPTH_MAX.load(core::sync::atomic::Ordering::Relaxed) {
        IO_DEPTH_MAX.store(inflight, core::sync::atomic::Ordering::Relaxed);
    }
    Some(i)
}
/// Wait for the completion of ONE'S OWN command without holding the ring: while we wait, the neighbours
/// submit and take off their own.
/// The commands that did not complete within 10 s (the slot drops out of circulation: the device may still
/// be writing into its buffer, while the answer is A REFUSAL rather than a "success" with an old status).
pub static IO_LOST: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// `false` — the command is lost: the caller must consider the I/O failed.
/// Previously the slot was freed silently, and the caller read the `resp.status()` of
/// THE PREVIOUS command of the same slot — that is, an "OK" for a read that never happened.
/// # Safety
/// D2: `i` is a slot THIS caller took. The wait deliberately does not hold `ring_lock`
/// (the neighbours must be able to submit and reap), and a command that does not complete
/// within ten seconds leaves its slot out of circulation rather than freeing it — the
/// device may still be writing into that buffer.
unsafe fn slot_wait(i: usize) -> bool {
    let hz = crate::tsc::hz();
    let limit = if hz != 0 { hz * 10 } else { 30_000_000_000 };
    let start = crate::tsc::rdtsc();
    let mut spins = 0u32;
    loop {
        {
            let _r = ring_lock();
            if let Some(Blk::Virtio(b)) = blk() { reap_locked(b); }
            let ios = &mut *core::ptr::addr_of_mut!(IOS);
            if let Some(sl) = ios[i].as_mut() {
                if sl.done { sl.busy = false; return true; }
            } else { return false; }
        }
        crate::paging::tlb_service();
        io_wait_step(&mut spins);
        if crate::tsc::rdtsc().wrapping_sub(start) > limit {
            IO_LOST.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::println!("fs: a disk command did not complete within 10 s — slot {} has DROPPED OUT of circulation, the I/O is a refusal", i);
            return false;
        }
    }
}

/// ONE STEP OF WAITING FOR A COMMAND IN FLIGHT: spin a little, and if the command is a barrier of
/// tens of milliseconds (on this stand up to 2 s), GIVE THE CORE to a ready neighbour.
///
/// WHAT FOR. The one waiting for a barrier spun the core for the whole barrier: with 534 barriers of 62 ms over
/// an 80 s run that is 33 s of processor burnt in vain — and everything that was
/// ready on that core waited together with it (the metric "ready->cpu":
/// the worst 3.7 s, for WTJourn.Flusher 6.3 s over a run). The core may be given up only
/// to one who owns nothing: the FS lock and the device's lock belong TO THE CORE, and
/// an owner that moved to another core would lose them (or, worse, they would go
/// to whoever runs next on this core). From a tick yielding is not allowed at all.
#[inline]
pub fn io_wait_step(spins: &mut u32) {
    *spins += 1;
    if *spins > 64 {
        *spins = 0;
        if io_may_yield() {
            // For the metric "ready->cpu" this is a wait for THE DEVICE rather than for the core.
            crate::sched::io_yield_begin();
            crate::sched::yield_origin(crate::sched::YO_FS); crate::sched::yield_now();
            crate::sched::io_yield_end();
            return;
        }
        // Yielding is not allowed (we hold the FS, or there is nobody to give the core to) — but that is no
        // reason to keep the core CLOSED TO THE TICK for the whole disk command.
        if io_window() { return; }
    }
    core::hint::spin_loop();
}

// ── THE INTERRUPT WINDOW WHILE WAITING FOR THE DEVICE (`talaria.iowin=`) ────
//
// WHAT FOR. The syscalls are executed with IF=0, while the one waiting for a disk command, which may not
// give up the core (`io_may_yield` — we hold the FS lock or there is nobody to give it to),
// spun for THE WHOLE FLIGHT OF THE COMMAND. The tick does not arrive at all during that time: the instrument
// "the tick's gap" (session 34) named `#18 pwrite64` with gaps of 16-18 ms — 786 and 421
// gaps >=4 ms per boot with ZERO host time, that is, it is our code rather than
// a preemption of the vCPU. Inside such a gap stand both the timers and everything that on this core
// was ready to run; in the scoreboard's numbers that is the tail of `mongodb/insert` p99.9 518-662
// against Linux's 158-441 with a won p50.
//
// Releasing the lock here, as `ext2::write_window` does, is not allowed: the waiter
// came here FROM THE MIDDLE of a replacement of a cache slot (the eviction of a dirty block) or
// of a read of metadata, where the state above is alive. So A WINDOW is opened:
// `sti` — a few cycles — `cli`. The lock stays ours meanwhile, the state
// is touched by nobody (the tick does not go to ext2), while the tick reaches the timers.
//
// THE MODES (the knob `talaria.iowin=`):
//   0 — the former spin with IF=0 (a rollback, for an A/B);
//   1 — there is a window, but the tick DOES NOT CARRY the lock's holder AWAY (`stay_for_locks`):
//       only "the tick arrives" is guaranteed;
//   2 — there is a window, and the tick is free to preempt the holder: while the command flies, the core
//       goes to a ready thread. The recognition of the lock's owner is BY THE TASK (session
//       27), so a move of the owner is lawful (`FS_RECUR_MIGRATED`,
//       `DEV_RECUR_MIGRATED` count it).
//
// THE DEFAULT IS 2, AND THAT IS MEASURED RATHER THAN CHOSEN. The mongodb/insert stand, one
// run (`win35-20260916-090750`), the ladder 1961-17649, the phases by one knob:
//
//   phase | mode                   | p50 against the base | p99.9 against the base
//   B/E   | 0 (as it was) — the base | 1.00               | 1.00
//   C     | 2                      | 0.77-0.90            | 0.13-1.04 (17 points out of 20 better)
//   D     | 1 (a window without preemption)| 0.82-0.93    | 2.3-6.7 WORSE
//
// That is, "the tick arrives" by itself is WORSE than the former spin: the lock's holder
// still gives the core to nobody, while the tick's price inside every disk command
// is added to the holding of the lock. The only thing that wins is what the window was
// made for — A READY THREAD GETS THE CORE while we wait for the disk.
//
// The window is NOT opened where the interrupts are allowed anyway (otherwise the `cli` would close
// them for the one who called us), in a tick and before the scheduler.
pub static IO_WIN_MODE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(2);
/// The windows opened; the refusals to open (a spin with IF=0, as it was).
pub static IO_WIN_N: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static IO_WIN_SPIN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub fn iowin_from_cmdline(cmdline: &str) {
    if let Some(n) = crate::cmdline::num(cmdline, "talaria.iowin") {
        IO_WIN_MODE.store((n as u32).min(2), core::sync::atomic::Ordering::SeqCst);
        crate::println!("fs: the interrupt window while waiting for the device — mode {} ({})", n.min(2),
            match n.min(2) { 0 => "off, a spin with IF=0", 1 => "the tick arrives, we do not carry the lock's holder away", _ => "the tick arrives and is free \
                to preempt" });
    }
}
/// Open the interrupt window. `true` — the window was opened (a step of the wait was made).
/// It is called from the outside too — by the waiters FOR LOCKS (the FS lock, the device's lock, the journal):
/// they hold nothing, and the window is all the more lawful for them, while their tick's gap is exactly
/// the same as that of the one waiting for a command.
#[inline]
pub fn io_window() -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    if IO_WIN_MODE.load(Relaxed) == 0 { return false; }
    if crate::console::in_tick() { return false; }
    if !crate::syscall::THREADED.load(Relaxed) { return false; }
    let flags: u64;
    // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
    // through `addr_of!` so that no reference to it is formed.
    unsafe { core::arch::asm!("pushfq; pop {}", out(reg) flags, options(nomem, preserves_flags)); }
    if flags & 0x200 != 0 { IO_WIN_SPIN.fetch_add(1, Relaxed); return false; } // the interrupts are open anyway
    IO_WIN_N.fetch_add(1, Relaxed);
    // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
    // through `addr_of!` so that no reference to it is formed.
    unsafe {
        core::arch::asm!("sti", options(nostack, preserves_flags));
        for _ in 0..64 { core::hint::spin_loop(); }
        core::arch::asm!("cli", options(nostack, preserves_flags));
    }
    true
}
/// DO NOT PREEMPT THE HOLDER OF THE FS/DEVICE LOCK (the mode `talaria.iowin=1`).
///
/// The tick asks: the interrupt window is opened by a lock's owner, and in mode 1 we
/// promised it that the tick would come but would not take the core. The question is about THE TASK (and not
/// about the core): the owner could have yielded the core to a neighbour, and there is no reason to pin the neighbour to
/// the core for the sake of somebody else's lock.
pub fn stay_for_locks(t: usize) -> bool {
    use core::sync::atomic::Ordering::{Relaxed, SeqCst};
    if IO_WIN_MODE.load(Relaxed) != 1 { return false; }
    if DEV_OWNER_REAL.load(SeqCst) && DEV_OWNER_TASK.load(SeqCst) == t as i64
        // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
        // through `addr_of!` so that no reference to it is formed.
        && unsafe { core::ptr::read_volatile(core::ptr::addr_of!(DEV_DEPTH)) } > 0 { return true; }
    crate::ext2::fs_owner_task_is(t)
}
/// May the one waiting for the device give up the core: a task's context (not a tick), this
/// core owns neither the FS nor the device, and there is somebody to run on it.
pub fn io_may_yield() -> bool {
    if crate::console::in_tick() { return false; }
    let cpu = crate::percpu::cpu_id();
    // The device's lock MAY be yielded by its owner: the recursion is recognised by the task
    // (`dev_held_by_me`), and a neighbour on this core will not take it for its own. It is not allowed
    // only where the owner is recorded by the core (there is no scheduler on the core).
    if DEV_OWNER.load(core::sync::atomic::Ordering::SeqCst) == cpu as i32
        && !DEV_OWNER_REAL.load(core::sync::atomic::Ordering::SeqCst) { return false; }
    if crate::ext2::fs_owned_here() { return false; }
    crate::sched::cpu_has_other_runnable(cpu, crate::sched::current_id())
}

// ── THE QUEUE'S DEPTH FOR NVMe (EBS) ────────────────────────────────────────
//
// The same separation as with virtio above, but the table of the commands in flight lives IN
// THE DRIVER: with NVMe we set the command's identifier (the CID) ourselves, so the slot and
// the CID are one and the same, and no extra tables are needed.
//
// The device is touched ONLY under `ring_lock` and always in short pieces
// ("submit a batch and ring" or "take the completions off"); the wait for ONE'S OWN
// completion goes WITHOUT the lock. The pointer to the driver is passed raw: a reference
// cannot be held across a release of the lock, while `BLK` after the mounting does not move.

// ── §8.13 A DEFERRED BATCH: an fdatasync IN TWO DISK ROUND TRIPS INSTEAD OF THREE ──
//
// WHAT FOR. An fsync in ordered mode made three full round trips of EBS in a row: the file's
// data (submitted — waited), the descriptor with the copies of the metadata (submitted — waited),
// the commit (FUA). The second round trip does not depend on the first: the commit must land AFTER
// the data and the copies, while the data and the copies do not wait for each other (with jbd2 it is the same:
// the data and the log blocks fly together, they are waited for at once, then the commit). Session 31,
// the mongodb stand: "the data" 3.5 ms per fsync with 6.5 ms in total — a third of the price.
//
// HOW. The last portion of the data IS SUBMITTED and not waited for: its CIDs stay in
// `DEFER_MASK`, and they are waited for by THE FIRST NEXT WRITE on the device (the descriptor's
// batch — in the ordinary case; a foreign write — if it managed to come earlier; an explicit
// `blk_defer_wait` — if there will be no transaction). Reads do not wait for the deferred one,
// but they take its ready completions off in passing so that the queue does not stand still.
// The order "the data are durable before the commit" is kept by the same thing as before: the
// device has no volatile cache, a completion = it is on the medium, while the commit is submitted
// only after somebody has waited for the mask. A barrier (where there is a cache)
// first waits for the deferred one, then flies itself.
//
// The deferred batch is ONE per device (a second submission with a non-empty mask goes
// synchronously): its buffer is a separate one (`ext2::DEFER_BUF`), and the portion's cache channels
// stay "in flight" (CWB) until `wb_defer_finish`.
static DEFER_MASK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static DEFER_ERR: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// WHO IS THE CRITICAL PATH IN THE JOURNAL'S BATCH — THE DATA OR THE COPIES (the instrument of session 38).
///
/// The descriptor's batch waits for TWO at once: the deferred DATA (§8.13, submitted
/// earlier) and THE COPIES of its own transaction. In the breakdown of the phases that is one figure
/// ("the descriptor+the copies", 874 us), and by it one cannot judge the change from plan 38:
/// `journal_async_commit` takes THE COPIES off the sequential chain (the commit
/// is submitted without waiting for them), but it cannot take THE DATA off — a checksum
/// does not replace the ordered semantics, and the commit must land after them.
///
/// So the change's gain is not "the whole commit" but `min(commit, the copies - the data)`:
/// if the data are acknowledged last, the change will give NOTHING. Here it is
/// timed when the absorbed deferred ones (the data) were acknowledged in the batch and
/// when its own ones (the copies) were — from the start of the wait.
pub static ABSW_N: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static ABSW_DATA_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static ABSW_OWN_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static ABSW_DATA_MAX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static ABSW_OWN_MAX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The batches deferred; of them the ones waited for by a foreign/next write; by an explicit wait.
pub static DEFER_N: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DEFER_ABSORBED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DEFER_WAITED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Take the ready completions off and pick OUR OWN out of them. Only under `ring_lock`.
/// The ready completions OF THE DEFERRED batch are taken off too (the queue's slots
/// return into circulation), waiting for them is the writes' business (see `nvme_batch`).
/// # Safety
/// D1 + D2 + D4: `dev` points at the one NVMe driver object of this machine (a `static mut`
/// reached by `addr_of_mut!`), and the completions taken off here are the ones the device
/// posted; `mine` is this caller's mask of outstanding commands.
unsafe fn nvme_harvest(dev: *mut crate::nvme::Nvme, mine: &mut u64, ok: &mut bool) {
    use core::sync::atomic::Ordering::SeqCst;
    let d = &mut *dev;
    d.reap();
    let mut m = *mine;
    while m != 0 {
        let cid = m.trailing_zeros();
        m &= m - 1;
        if let Some(good) = d.take_done(cid as u16) {
            *mine &= !(1u64 << cid);
            if !good { *ok = false; }
        }
    }
    let mut m = DEFER_MASK.load(SeqCst);
    while m != 0 {
        let cid = m.trailing_zeros();
        m &= m - 1;
        if let Some(good) = d.take_done(cid as u16) {
            DEFER_MASK.fetch_and(!(1u64 << cid), SeqCst);
            if !good { DEFER_ERR.store(true, SeqCst); }
        }
    }
}

/// A batch of transfers `(lba, the address, the number of 512-blocks)` through the queue: we submit
/// as much as the device accepts, ring once, wait for OUR OWN completions.
/// A neighbour's barrier flies past meanwhile — it no longer occupies the queue.
///
/// A WRITE, besides its own, waits for THE DEFERRED batch too (§8.13): the mask
/// is taken under the ring's lock, and from that moment its CIDs are ours.
///
/// # Safety
/// D1 + D2 + D3: `dev` is the machine's one NVMe object, and every `(lba, address,
/// sectors)` must name a frame the kernel owns and will not touch until the command
/// completes (a block-cache slot or a journal staging page). The wait is for OUR CIDs
/// only — the mask taken under the ring's lock is what makes them ours.
unsafe fn nvme_batch(dev: *mut crate::nvme::Nvme, write: bool, reqs: &[(usize, u64, u16)], fua: bool) -> bool {
    use core::sync::atomic::Ordering::{Relaxed, SeqCst};
    if reqs.is_empty() && !(write && DEFER_MASK.load(SeqCst) != 0) { return true; }
    let mut mine: u64 = 0;
    let mut absorbed: u64 = 0;
    let mut ok = true;
    let mut i = 0usize;
    let mut off: u16 = 0;
    let hz = crate::tsc::hz();
    let limit = if hz != 0 { hz * 10 } else { 30_000_000_000 }; // 10 s — the insurance
    let mut start = crate::tsc::rdtsc();
    // The instrument "the data or the copies" (see ABSW_*): its own reference point, because
    // `start` is moved at every successful submission (that is the timeout's insurance).
    let t_enter = start;
    let mut t_abs = 0u64;   // when THE ABSORBED ones (the deferred data) were acknowledged
    let mut t_own = 0u64;   // when OUR OWN ones (the transaction's copies) were acknowledged
    let mut spins = 0u32;
    loop {
        let mut pushed = false;
        {
            let _r = ring_lock();
            if write && absorbed == 0 {
                absorbed = DEFER_MASK.swap(0, SeqCst);
                if absorbed != 0 { mine |= absorbed; DEFER_ABSORBED.fetch_add(1, Relaxed); }
            }
            nvme_harvest(dev, &mut mine, &mut ok);
            if absorbed != 0 && t_abs == 0 && mine & absorbed == 0 { t_abs = crate::tsc::rdtsc(); }
            if t_own == 0 && i >= reqs.len() && mine & !absorbed == 0 { t_own = crate::tsc::rdtsc(); }
            let d = &mut *dev;
            let maxb = d.max_blocks();
            while i < reqs.len() {
                let (lba, addr, nblk) = reqs[i];
                if off >= nblk { i += 1; off = 0; continue; }
                let nb = (nblk - off).min(maxb);
                match d.push_rw_ex(write, (lba + off as usize) as u64, nb, addr + off as u64 * 512, fua) {
                    Some(cid) => {
                        mine |= 1u64 << cid; off += nb; pushed = true;
                        IO_SUBMITS.fetch_add(1, Relaxed);
                    }
                    None => { IO_QFULL.fetch_add(1, Relaxed); break; }
                }
            }
            if pushed {
                d.kick();
                let depth = d.inflight() as u64;
                if depth > IO_DEPTH_MAX.load(Relaxed) { IO_DEPTH_MAX.store(depth, Relaxed); }
            }
        }
        if pushed { start = crate::tsc::rdtsc(); }
        if i >= reqs.len() && mine == 0 {
            if absorbed != 0 && !ok { DEFER_ERR.store(true, SeqCst); }
            if absorbed != 0 {
                // The batch waited for both — record which of them turned out to be the last.
                let now = crate::tsc::rdtsc();
                if t_abs == 0 { t_abs = now; }
                if t_own == 0 { t_own = now; }
                let (dd, dw) = (t_abs.wrapping_sub(t_enter), t_own.wrapping_sub(t_enter));
                ABSW_N.fetch_add(1, Relaxed);
                ABSW_DATA_TSC.fetch_add(dd, Relaxed); ABSW_DATA_MAX.fetch_max(dd, Relaxed);
                ABSW_OWN_TSC.fetch_add(dw, Relaxed); ABSW_OWN_MAX.fetch_max(dw, Relaxed);
            }
            return ok;
        }
        crate::paging::tlb_service();
        // The wait for OUR OWN completions is with a yield of the core (`io_wait_step`) rather than
        // a pure spin: writing a batch to EBS is milliseconds (136 KB of WiredTiger's journal
        // data = 2.3 ms on gp3), and all that time the core was
        // busy with the wait, while the ready threads on it stood still (see the header of
        // the device's lock). The yield is allowed only to the one that holds neither
        // the FS nor a tick — the same rule as at the barrier.
        io_wait_step(&mut spins);
        if crate::tsc::rdtsc().wrapping_sub(start) > limit {
            let n = mine.count_ones();
            let _r = ring_lock();
            let d = &mut *dev;
            let mut m = mine;
            while m != 0 { let c = m.trailing_zeros(); m &= m - 1; d.abandon(c as u16); }
            crate::println!("nvme: a command did not complete within 10 s — {} slot(s) have dropped out of circulation", n);
            if absorbed != 0 { DEFER_ERR.store(true, SeqCst); }
            return false;
        }
    }
}

/// §8.13: SUBMIT a batch of writes and DO NOT wait: the CIDs stay in `DEFER_MASK`. If
/// submitting it entirely did not work out (the queue is busy) — the remainder is written synchronously and
/// it is the one that waits for what was submitted; then there is no deferred one. Only under `ring_lock`.
/// `None` — the batch is deferred; `Some(ok)` — it is written synchronously.
///
/// # Safety
/// D1 + D2 + D3, as `nvme_batch`, with one difference that matters: the submission
/// returns BEFORE the completions, so those buffers stay the device's property until
/// `nvme_flush_queued` has seen them. Only under `ring_lock`.
unsafe fn nvme_submit_deferred(dev: *mut crate::nvme::Nvme, reqs: &[(usize, u64, u16)]) -> Option<bool> {
    use core::sync::atomic::Ordering::{Relaxed, SeqCst};
    let mut mask: u64 = 0;
    let mut i = 0usize;
    let mut off: u16 = 0;
    {
        let _r = ring_lock();
        let (mut dummy, mut dok) = (0u64, true);
        nvme_harvest(dev, &mut dummy, &mut dok);
        let d = &mut *dev;
        let maxb = d.max_blocks();
        'outer: while i < reqs.len() {
            let (lba, addr, nblk) = reqs[i];
            while off < nblk {
                let nb = (nblk - off).min(maxb);
                match d.push_rw_ex(true, (lba + off as usize) as u64, nb, addr + off as u64 * 512, false) {
                    Some(cid) => { mask |= 1u64 << cid; off += nb; IO_SUBMITS.fetch_add(1, Relaxed); }
                    None => { IO_QFULL.fetch_add(1, Relaxed); break 'outer; }
                }
            }
            if off >= nblk { i += 1; off = 0; }
        }
        if mask != 0 {
            d.kick();
            let depth = d.inflight() as u64;
            if depth > IO_DEPTH_MAX.load(Relaxed) { IO_DEPTH_MAX.store(depth, Relaxed); }
        }
        if i >= reqs.len() {
            DEFER_MASK.store(mask, SeqCst);
            DEFER_N.fetch_add(1, Relaxed);
            return None;
        }
        // it did not fit: the remainder goes synchronously, together with what has already been submitted
        DEFER_MASK.store(mask, SeqCst);
    }
    let mut rest: [(usize, u64, u16); 64] = [(0, 0, 0); 64];
    let mut k = 0usize;
    let (lba, addr, nblk) = reqs[i];
    if off < nblk { rest[k] = (lba + off as usize, addr + off as u64 * 512, nblk - off); k += 1; }
    for r in &reqs[i + 1..] { if k < rest.len() { rest[k] = *r; k += 1; } }
    Some(nvme_batch(dev, true, &rest[..k], false))   // it will absorb DEFER_MASK
}

/// One transfer of an arbitrary length (it is cut inside by the ceiling of one command).
///
/// # Safety
/// D1 + D2 + D3, as `nvme_batch`, for one transfer: `buf` is a physical address of a
/// frame the kernel owns, `len` bytes of it.
unsafe fn nvme_rw_queued(dev: *mut crate::nvme::Nvme, write: bool, lba: usize, buf: u64, len: usize) -> bool {
    nvme_rw_queued_ex(dev, write, lba, buf, len, false)
}
/// # Safety
/// As `nvme_rw_queued`; `fua` only tells the device not to lie about durability.
unsafe fn nvme_rw_queued_ex(dev: *mut crate::nvme::Nvme, write: bool, lba: usize, buf: u64, len: usize, fua: bool) -> bool {
    let _g = Busy::new();
    let mut done = 0usize;
    while done < len {
        // 32768 sectors = 16 MB: that is how much fits into a batch's `nblk` field (u16);
        // `nvme_batch` will cut the batch into shorter commands by the MDTS.
        let take = (len - done).min(16 * 1024 * 1024);
        let req = (lba + done / 512, buf + done as u64, (take / 512).max(1) as u16);
        if !nvme_batch(dev, write, &[req], fua) { return false; }
        done += take;
    }
    true
}

/// A barrier through the queue: while it flies (on EBS that is tens of milliseconds), the ring
/// stays free, and foreign reads go past it rather than behind it.
///
/// # Safety
/// D1 + D2: it waits for the DEFERRED commands of this caller; until it returns, their
/// buffers belong to the device, and a caller that skips it and reuses a buffer gets the
/// device writing into live memory.
unsafe fn nvme_flush_queued(dev: *mut crate::nvme::Nvme) -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    let _g = Busy::new();
    let mut mine: u64 = 0;
    let mut ok = true;
    // §8.13: the deferred batch goes BEFORE the barrier, otherwise it will not cover it.
    if DEFER_MASK.load(core::sync::atomic::Ordering::SeqCst) != 0 { ok = nvme_batch(dev, true, &[], false); }
    let hz = crate::tsc::hz();
    let limit = if hz != 0 { hz * 30 } else { 90_000_000_000 }; // a barrier is entitled to be long
    let start = crate::tsc::rdtsc();
    let cid = loop {
        let got = {
            let _r = ring_lock();
            nvme_harvest(dev, &mut mine, &mut ok);
            let d = &mut *dev;
            match d.push_flush() { Some(c) => { d.kick(); Some(c) } None => None }
        };
        match got {
            Some(c) => { IO_SUBMITS.fetch_add(1, Relaxed); break c; }
            None => {
                IO_QFULL.fetch_add(1, Relaxed);
                crate::paging::tlb_service();
                core::hint::spin_loop();
                if crate::tsc::rdtsc().wrapping_sub(start) > limit { return false; }
            }
        }
    };
    mine = 1u64 << cid;
    let mut spins = 0u32;
    loop {
        {
            let _r = ring_lock();
            nvme_harvest(dev, &mut mine, &mut ok);
        }
        if mine == 0 { return ok; }
        crate::paging::tlb_service();
        io_wait_step(&mut spins);
        if crate::tsc::rdtsc().wrapping_sub(start) > limit {
            let _r = ring_lock();
            (&mut *dev).abandon(cid);
            crate::println!("nvme: a barrier did not complete within 30 s — the slot has dropped out of circulation");
            return false;
        }
    }
}

// ── THE BARRIER'S LOCK (separately from the device's lock) ───────────────────
//
// WHAT FOR. The barrier went under the DEVICE's lock, and for the whole time of its flight (36-48 ms
// on this stand, up to 2 s in the worst case) the device was closed to everyone: a foreign
// write of data (WiredTiger's log writer's `dirty_balance`) stood behind
// the journal's transaction for two barriers in a row (~90 ms) 13 times a second — and that
// was the tail of the inserts. While the barrier needed one thing from the device's lock: that
// two barriers should not fly simultaneously and that the second could MERGE with the first (the one all of whose data
// have already been given to the device before the start of a foreign barrier is covered by it). That is given by its own
// lock, while the device stays free: while the barrier flies, the neighbours write.
//
// The lock is NOT by the core: the one waiting for a barrier yields the core (`io_wait_step`) and may
// come back on another one. It is released by the same guard that took it — by value.
static FLUSH_BUSY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
pub static FLUSH_WAITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The waits for a barrier longer than 30 s (a diagnosis into the console) and the bypasses from a panic. As
/// at `ring_lock`: previously after 30 s a guard was returned WITHOUT the lock and released
/// a foreign one — two barriers flew simultaneously, and "covered by a foreign barrier"
/// (`DEV_FL_MERGED`) stopped being true.
pub static FLUSH_TIMEOUT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static FLUSH_BYPASS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
struct FlushGuard(bool);
impl Drop for FlushGuard { fn drop(&mut self) { if self.0 { FLUSH_BUSY.store(false, core::sync::atomic::Ordering::SeqCst); } } }
fn flush_lock() -> FlushGuard {
    use core::sync::atomic::Ordering;
    let hz = crate::tsc::hz();
    let limit = if hz != 0 { hz * 30 } else { 90_000_000_000 }; // the diagnosis's term is like the barrier's own
    let start = crate::tsc::rdtsc();
    let mut spins = 0u32;
    let mut waited = false;
    let mut next_report = limit;
    let mut reports = 0u32;
    while FLUSH_BUSY.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        if !waited { waited = true; FLUSH_WAITS.fetch_add(1, Ordering::Relaxed); }
        crate::paging::tlb_service();
        io_wait_step(&mut spins);
        let w = crate::tsc::rdtsc().wrapping_sub(start);
        if w > next_report {
            if reports == 0 { FLUSH_TIMEOUT.fetch_add(1, Ordering::Relaxed); }
            if crate::panicking() { FLUSH_BYPASS.fetch_add(1, Ordering::Relaxed); return FlushGuard(false); }
            crate::println!("fs: a barrier did not give in within {} s (a foreign barrier was started {} ms ago); we keep waiting (give-ups 0)",
                w / hz.max(1), crate::tsc::rdtsc().wrapping_sub(FLUSH_STARTED_TSC.load(Ordering::SeqCst)) / (hz / 1000).max(1));
            reports += 1;
            next_report = w + limit;
        }
    }
    FlushGuard(true)
}

// ── THE DEVICE'S LOCK (separately from the FS lock) ─────────────────────────
//
// WHY A SEPARATE ONE. The FS lock protects THE STATE in memory (the inodes, the directories, the block
// cache), while waiting for the device has nothing to do with the state — but previously it was
// held under the same lock, and that is why the device's barrier stopped the whole
// file system. The mongodb measurement named the price: 422 barriers of 46 ms each,
// 19.4 seconds out of the 20.0 of the total holding of the FS lock. While the database's background thread does
// an fsync of the journal, even the HITS in the block cache stood still for us (and they are 90 %), and
// because of that an insert's p99.9 was 136 ms against Linux's 0.5 on the same disk.
//
// THE RULE OF ORDER on which everything rests: the one holding the DEVICE's lock never
// asks for the FS lock. So waiting for the device while holding the FS is safe — there is no
// cycle: the one inside the device waits for nobody but the hardware.
//
// THE MUTUAL EXCLUSION IS BY THE CORE (a CAS on `DEV_OWNER`), THE RECURSION IS BY THE TASK
// (`DEV_OWNER_TASK`), exactly as with the FS lock after session 27. While the recursion
// was recognised by the core, the owner was forbidden to give up the processor
// (`io_may_yield`): a neighbour on the same core would have taken the lock for its own. The price of the
// prohibition was A CORE BURNT BY A SPIN FOR THE WHOLE WAIT FOR THE DISK: an fdatasync of
// WiredTiger's journal on EBS = ~4.4 ms of spin 10 times a second on two cores, and
// every connection thread that ended up on that core waited together with it —
// mongodb/insert p99.9 4.3 ms against Linux's 0.3 (work stealing took
// such threads as "having left 4.5-4.9 ms ago"). FUA has nothing to do with it — it was checked
// with the knob `talaria.fua=` (session 31). The recognition by the task removes the prohibition:
// the one waiting for the device gives up the core (`io_wait_step`), like the one waiting for a barrier.
// A waiter that HOLDS the FS lock still spins (see `io_may_yield`).
static DEV_OWNER: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(-1);
static mut DEV_DEPTH: u32 = 0;
/// How many cores are waiting for the device from the outside right now (dev_wait_free). The one who
/// writes the disk in portions YIELDS to a waiter between the portions: otherwise it takes the
/// lock back earlier than the waiter manages to wake up, and an fsync of the workload's journal
/// stands behind the whole flushing of a foreign file (hundreds of milliseconds).
static DEV_WAITERS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static DEV_HANDOFF: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub fn dev_handoffs() -> u64 { DEV_HANDOFF.load(core::sync::atomic::Ordering::Relaxed) }
/// The takes of the device's lock, of them with a wait, and how much we waited (the diagnostics:
/// it is exactly here that the wait which used to stand on the FS lock has moved).
pub static DEV_TAKES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DEV_CONT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DEV_WAIT_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many times the wait for the device went over a second (each one with a diagnosis
/// into the console: who holds it and for how long). Zero in a healthy system.
pub static DEV_TIMEOUT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many times the lock was BYPASSED — only from a panic (the trace on the disk is more important than
/// the order of the writes for a machine that is going down anyway).
pub static DEV_BYPASS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Who holds it: the task and the TSC of the take — for the diagnosis at a drawn-out wait.
static DEV_OWNER_TASK: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(-1);
static DEV_HELD_SINCE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Is a real task recorded as the owner (ITS OWN core knows it). Before the
/// scheduler `sched::current_id()` returns the common `CURRENT`, the same for all the
/// cores, and comparing by it is not allowed — see the same reservation at the FS lock.
static DEV_OWNER_REAL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// A FOREIGN TASK ON THE OWNER'S CORE asks for the lock. Previously (with the recognition by the core)
/// that was an entry to the device WITHOUT the lock — a latent defect that was held back
/// only by the prohibition to yield; now such a task honestly WAITS (the owner yielded
/// it the core and will come back). The counter is kept: it says how many times the owner
/// was preempted in the middle of a section.
pub static DEV_REENTER_ALIEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The owner of the device's lock turned up on ANOTHER core (it yielded and moved).
pub static DEV_RECUR_MIGRATED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// How many times the one waiting for the device's lock gave up the core instead of spinning.
pub static DEV_LOCK_YIELDS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Does THE ONE WHO ASKS hold the device's lock (the task — if it is a real one, otherwise the core).
#[inline] fn dev_held_by_me() -> bool {
    use core::sync::atomic::Ordering::SeqCst;
    // SAFETY: D3 — the buffer handed to the device is a frame the kernel owns (a block-cache slot or a
    // staging page), untouched for the length of the command.
    if unsafe { DEV_DEPTH } == 0 || DEV_OWNER.load(SeqCst) < 0 { return false; }
    match (DEV_OWNER_REAL.load(SeqCst), dev_real_task()) {
        (true, Some(t)) => DEV_OWNER_TASK.load(SeqCst) == t as i64,
        _ => DEV_OWNER.load(SeqCst) == crate::percpu::cpu_id() as i32,
    }
}
/// The recursion is OURS: if the owner's core is no longer ours — the owner yielded and moved.
#[inline] fn dev_recur(me: i32) {
    use core::sync::atomic::Ordering::SeqCst;
    if DEV_OWNER.load(SeqCst) != me { DEV_RECUR_MIGRATED.fetch_add(1, SeqCst); DEV_OWNER.store(me, SeqCst); }
    // SAFETY: D3 — the buffer handed to the device is a frame the kernel owns (a block-cache slot or a
    // staging page), untouched for the length of the command.
    unsafe { DEV_DEPTH += 1; }
}
#[inline] fn dev_real_task() -> Option<usize> {
    let t = crate::percpu::cur_task();
    if t == u64::MAX || t as usize >= crate::sched::NTASKS { None } else { Some(t as usize) }
}
#[inline] fn dev_note_owner() {
    use core::sync::atomic::Ordering::Relaxed;
    let rt = dev_real_task();
    DEV_OWNER_REAL.store(rt.is_some(), Relaxed);
    DEV_OWNER_TASK.store(rt.unwrap_or_else(crate::sched::current_id) as i64, Relaxed);
    DEV_HELD_SINCE.store(crate::tsc::rdtsc(), Relaxed);
}
/// The diagnosis of a drawn-out wait: it is printed ONCE per event (the first
/// second), after that once every ten seconds, so that the console does not become the load.
fn dev_wait_report(waited: u64, reports: u32, what: &str) {
    use core::sync::atomic::Ordering::SeqCst;
    let hz = crate::tsc::hz().max(1);
    let held = crate::tsc::rdtsc().wrapping_sub(DEV_HELD_SINCE.load(SeqCst)) / (hz / 1000).max(1);
    crate::println!("fs: {} did not give in within {} s: core {} holds it (the task {}, already {} ms), waiters outside {}; we keep waiting (give-ups {})",
        what, waited / hz, DEV_OWNER.load(SeqCst), DEV_OWNER_TASK.load(SeqCst), held,
        DEV_WAITERS.load(SeqCst), reports);
}

pub struct DevGuard(bool);
impl DevGuard {
    /// The lock is really ours (`false` — only a bypass from a panic).
    pub fn held(&self) -> bool { self.0 }
}
impl Drop for DevGuard {
    fn drop(&mut self) {
        if !self.0 { return; }
        // SAFETY: D3 — the buffer handed to the device is a frame the kernel owns (a block-cache slot
        // or a staging page), untouched for the length of the command.
        unsafe {
            DEV_DEPTH -= 1;
            if DEV_DEPTH == 0 {
                // The task is removed FIRST (as with the FS lock): while `DEV_OWNER`
                // is occupied, nobody will take the lock and see a foreign task.
                DEV_OWNER_REAL.store(false, core::sync::atomic::Ordering::SeqCst);
                DEV_OWNER_TASK.store(-1, core::sync::atomic::Ordering::SeqCst);
                DEV_OWNER.store(-1, core::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}

/// Take the device's lock (recursively by the core).
///
/// The wait DOES NOT GIVE UP. Previously after a second the lock silently returned a `DevGuard(false)`,
/// and the caller went to the device without it: on a gp3-class disk a barrier is sometimes
/// 1.7-1.9 s, which means the "insurance" fired under an honest load and broke
/// exactly what the lock exists for — the order "the copies in the journal -> the commit ->
/// the write in place". From the outside that is indistinguishable from an fsync's lie, while the bypass had no
/// counter. Now after a second A DIAGNOSIS is printed (who holds it, from which task,
/// for how long) and the wait continues: a machine hanging with the culprit's name is
/// better than a quietly corrupted FS. The only bypass is a panic: the trace on the disk
/// (`talaria.trace=`) is written when a neighbouring core could already have stood still for good.
pub fn dev_lock() -> DevGuard {
    use core::sync::atomic::Ordering;
    let me = crate::percpu::cpu_id() as i32;
    if dev_held_by_me() { dev_recur(me); return DevGuard(true); }
    // The core says "the lock is mine", while the task is not the right one: the owner yielded us the core
    // in the middle of a section. Previously that was an entry without the lock; now it is a wait.
    if DEV_OWNER.load(Ordering::SeqCst) == me { DEV_REENTER_ALIEN.fetch_add(1, Ordering::Relaxed); }
    DEV_TAKES.fetch_add(1, Ordering::Relaxed);
    let hz = crate::tsc::hz();
    let limit = if hz != 0 { hz } else { 3_000_000_000 }; // ~1 s until the first diagnosis
    let start = crate::tsc::rdtsc();
    let mut spun = false;
    let mut spins = 0u32;
    let mut reports = 0u32;
    let mut next_report = limit;
    loop {
        if DEV_OWNER.compare_exchange(-1, me, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            // SAFETY: D3 — the buffer handed to the device is a frame the kernel owns (a block-cache
            // slot or a staging page), untouched for the length of the command.
            unsafe { DEV_DEPTH = 1; }
            dev_note_owner();
            if spun {
                DEV_WAITERS.fetch_sub(1, Ordering::SeqCst);
                DEV_CONT.fetch_add(1, Ordering::Relaxed);
                DEV_WAIT_TSC.fetch_add(crate::tsc::rdtsc().wrapping_sub(start), Ordering::Relaxed);
            }
            return DevGuard(true);
        }
        if !spun { DEV_WAITERS.fetch_add(1, Ordering::SeqCst); } // dev_handoff sees this waiter too
        spun = true;
        crate::paging::tlb_service(); // a neighbour in munmap waits for our acknowledgement
        // The owner could have yielded the core precisely to us: to keep spinning means not
        // to let it back until the tick. We give up the core if there is somebody to give it to (and if we ourselves
        // own nothing — see `io_may_yield`).
        spins += 1;
        if spins > 256 {
            spins = 0;
            if io_may_yield() { DEV_LOCK_YIELDS.fetch_add(1, Ordering::Relaxed); crate::sched::yield_origin(crate::sched::YO_FS); crate::sched::yield_now(); }
            else { io_window(); }   // there is nobody to give it to — but the tick must arrive
        }
        core::hint::spin_loop();
        let waited = crate::tsc::rdtsc().wrapping_sub(start);
        if waited > next_report {
            if reports == 0 { DEV_TIMEOUT.fetch_add(1, Ordering::Relaxed); }
            if crate::panicking() {
                // The machine is going down; it no longer needs the order of the writes, it needs the trace.
                DEV_BYPASS.fetch_add(1, Ordering::Relaxed);
                DEV_WAITERS.fetch_sub(1, Ordering::SeqCst);
                return DevGuard(false);
            }
            dev_wait_report(waited, reports, "the device's lock");
            reports += 1;
            next_report = waited + limit * 10;
        }
    }
}

/// Take the device's lock if it is free (or already ours) — without waiting.
/// It is needed by the one holding the FS lock: waiting for the device UNDER it is allowed but
/// expensive — the whole FS stands still; it is better to release the FS and wait it out from the outside.
pub fn dev_trylock() -> Option<DevGuard> {
    use core::sync::atomic::Ordering;
    let me = crate::percpu::cpu_id() as i32;
    if dev_held_by_me() { dev_recur(me); return Some(DevGuard(true)); }
    DEV_TAKES.fetch_add(1, Ordering::Relaxed);
    if DEV_OWNER.compare_exchange(-1, me, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
        // SAFETY: D3 — the buffer handed to the device is a frame the kernel owns (a block-cache slot
        // or a staging page), untouched for the length of the command.
        unsafe { DEV_DEPTH = 1; }
        dev_note_owner();
        return Some(DevGuard(true));
    }
    None
}
/// Wait until the device's lock is released (WITHOUT taking it). It is called without
/// the FS lock. It is NOT bounded in time (see `dev_lock`): to come back earlier would
/// mean taking the FS lock and standing under it waiting for the device, that is, exactly what
/// this path goes away from; after a second there is a diagnosis, and we keep waiting.
pub fn dev_wait_free() {
    use core::sync::atomic::Ordering;
    let hz = crate::tsc::hz();
    let limit = if hz != 0 { hz } else { 3_000_000_000 };
    let start = crate::tsc::rdtsc();
    let mut spins = 0u32;
    let mut reports = 0u32;
    let mut next_report = limit;
    struct W; impl Drop for W { fn drop(&mut self) { DEV_WAITERS.fetch_sub(1, core::sync::atomic::Ordering::SeqCst); } }
    DEV_WAITERS.fetch_add(1, Ordering::SeqCst);
    let _w = W;
    while DEV_OWNER.load(Ordering::SeqCst) != -1 {
        // We wait for the device owning nothing — so we may give up the core
        // to a neighbour too (like the one waiting for the FS lock): a busy device is tens
        // of milliseconds of a barrier rather than hundreds of cycles.
        spins += 1;
        if spins > 256 {
            spins = 0;
            let cpu = crate::percpu::cpu_id();
            if crate::sched::cpu_has_other_runnable(cpu, crate::sched::current_id()) { crate::sched::yield_origin(crate::sched::YO_FS);
                crate::sched::yield_now(); continue; }
        }
        crate::paging::tlb_service();
        core::hint::spin_loop();
        let waited = crate::tsc::rdtsc().wrapping_sub(start);
        if waited > next_report {
            if reports == 0 { DEV_TIMEOUT.fetch_add(1, Ordering::Relaxed); }
            if crate::panicking() { return; }
            dev_wait_report(waited, reports, "the device (a wait from the outside)");
            reports += 1;
            next_report = waited + limit * 10;
        }
    }
}

/// Yield the device to a waiter: it is called right after the lock is released
/// between the portions of a long write. If somebody is waiting — we do not take the device
/// back until it takes it (or until ~200 us have expired: the waiter
/// might have just yielded the core).
pub fn dev_handoff() {
    use core::sync::atomic::Ordering;
    if DEV_WAITERS.load(Ordering::SeqCst) == 0 { return; }
    DEV_HANDOFF.fetch_add(1, Ordering::Relaxed);
    let hz = crate::tsc::hz();
    let limit = if hz != 0 { hz / 5000 } else { 600_000 };
    let start = crate::tsc::rdtsc();
    while DEV_OWNER.load(Ordering::SeqCst) == -1 && DEV_WAITERS.load(Ordering::SeqCst) != 0 {
        crate::paging::tlb_service();
        core::hint::spin_loop();
        if crate::tsc::rdtsc().wrapping_sub(start) > limit { return; }
    }
}

/// THE PRICE OF ACCESSING THE DISK, BROKEN DOWN BY KIND.
///
/// The counters "how many blocks were read/written" are not enough for the main
/// question: the FS lock is held for 42 seconds over a run with 7435 takes (5.7 ms per
/// take) — and one needs to know WHAT exactly those milliseconds are. The wait for the disk goes
/// SYNCHRONOUSLY and UNDER THE LOCK, so its breakdown by kind (a read / a write /
/// a barrier) directly answers what to reduce.
pub static DEV_RD_N: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DEV_RD_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DEV_WR_N: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DEV_WR_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DEV_FL_N: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DEV_FL_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DEV_FL_MAX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
#[inline]
fn dev_note(n: &core::sync::atomic::AtomicU64, t: &core::sync::atomic::AtomicU64, t0: u64) -> u64 {
    use core::sync::atomic::Ordering::Relaxed;
    let d = crate::tsc::rdtsc().wrapping_sub(t0);
    n.fetch_add(1, Relaxed); t.fetch_add(d, Relaxed);
    d
}

/// Low-level access to virtio-blk for ext2: reading/writing sectors (buf is a multiple of 512), flush.
pub fn blk_read(lba: usize, buf: &mut [u8]) -> bool {
    let t0 = crate::tsc::rdtsc();
    let r = io_one(lba, buf.as_mut_ptr(), buf.len(), false);
    dev_note(&DEV_RD_N, &DEV_RD_TSC, t0); r
}
/// THE REFUSALS OF A WRITE AND OF A BARRIER ARE COUNTED RATHER THAN SWALLOWED. Previously `blk_write`
/// returned false, ext2 printed a line and went on, while fsync answered 0:
/// "written" for data that are not on the disk. The counter grows at every
/// refusal (a write, a batch, a barrier — lost or rejected by the device);
/// the FS takes it off before and after an operation and answers EIO if it has grown.
pub static WRITE_FAILED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The gate `ioerr`: substitute a refusing device — all the writes and barriers
/// return a refusal without reaching the ring (the reads go as usual).
pub static FAIL_WRITES: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
fn write_failed() -> bool { WRITE_FAILED.fetch_add(1, core::sync::atomic::Ordering::SeqCst); false }
pub fn blk_write(lba: usize, buf: &[u8]) -> bool {
    if FAIL_WRITES.load(core::sync::atomic::Ordering::Relaxed) { return write_failed(); }
    let t0 = crate::tsc::rdtsc();
    let r = io_one(lba, buf.as_ptr() as *mut u8, buf.len(), true);
    dev_note(&DEV_WR_N, &DEV_WR_TSC, t0);
    if !r { return write_failed(); }
    r
}

/// A DURABLE WRITE OF ONE BLOCK: it returned — the block IS ON THE MEDIUM.
///
/// The journal's transaction ended with the pair "a write of the commit block + a barrier", that is,
/// with TWO barriers per transaction (the counter `barriers: the transaction` is exactly twice
/// the number of transactions). The second of them is needed for the sake of one block — and it is exactly
/// for that that NVMe has FUA: the command completes when the write is on the medium.
/// jbd2 does the same thing, and for the same reason.
///
/// Where there is no FUA (the local stand's virtio-blk), the behaviour is the former one: a write,
/// then a barrier. So the caller does not need to know what device is under
/// it — but it must keep the barrier counter by `fua_write()`, otherwise the number
/// of barriers will diverge from the number of requests.
pub static DEV_FUA_N: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DEV_FUA_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DEV_FUA_MAX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The durable writes made BY AN ORDINARY write: the device has no volatile
/// cache (VWC=0), and a write is durable on the command's completion — FUA is superfluous.
pub static DEV_DUR_PLAIN_N: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// FUA WITH VWC=0 IS SUPERFLUOUS, AND IT IS NOT FREE.
///
/// By the NVMe specification a write on a controller without a volatile cache completes
/// when the data are on the medium — exactly what FUA promises. While we set FUA
/// on the commit block ALWAYS. Session 30 suspected the tail of mongodb/insert in that
/// ("a durable one is ~4 ms against ~2 for an ordinary one"); session 31 CHECKED it with a knob —
/// without FUA the tail is the same (4.3 ms): the culprit was the core's spin on waiting for the disk
/// rather than a bit of the command. The change is kept as an honest one: Linux does not set FUA
/// on such a device (REQ_FUA is removed when the queue is declared without a write-back
/// cache — `blk_queue_write_cache`), and on devices where FUA is emulated
/// with a barrier it is cheaper.
/// The knob `talaria.fua=`: 1 — FUA always (the former behaviour), 0 — never (with VWC=1
/// a write + a barrier), no knob — by the VWC. The durability must be one and the
/// same in all three modes; it is checked by the power-cut gate.
pub static FUA_MODE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
pub fn fua_from_cmdline(cmdline: &str) {
    if let Some(n) = crate::cmdline::num(cmdline, "talaria.fua") {
        FUA_MODE.store(if n == 0 { 2 } else { 1 }, core::sync::atomic::Ordering::SeqCst);
        crate::println!("fs: a durable write — {}", if n == 0 { "without FUA (a write, with VWC=1 a barrier)" } else { "always FUA" });
    }
}
/// Whether to set FUA on a durable write right now.
fn fua_now() -> bool {
    match FUA_MODE.load(core::sync::atomic::Ordering::Relaxed) { 1 => true, 2 => false, _ => flush_needed() }
}
/// Can the device do a durable write without a separate barrier.
pub fn fua_write() -> bool { matches!(blk(), Some(Blk::Nvme(_))) }
pub fn blk_write_durable(lba: usize, buf: &[u8]) -> bool {
    if FAIL_WRITES.load(core::sync::atomic::Ordering::Relaxed) { return write_failed(); }
    if !fua_write() {
        if !blk_write(lba, buf) { return false; }
        return blk_flush();
    }
    if !fua_now() {
        // VWC=0 (or the knob): an ordinary write is already durable; `blk_flush` with
        // VWC=0 does nothing, with VWC=1 (the knob fua=0) it is an honest barrier.
        DEV_DUR_PLAIN_N.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if !blk_write(lba, buf) { return false; }
        return blk_flush();
    }
    let t0 = crate::tsc::rdtsc();
    // SAFETY: D1 + D2 — the ring and the slot table under `ring_lock`; a slot is reused only after its
    // completion has been seen.
    let ok = unsafe {
        let Some(Blk::Nvme(n)) = blk() else { return false };
        let dev: *mut crate::nvme::Nvme = n;
        // The partition's base — as in `io_one`: from the outside THE PARTITION is addressed, not the disk.
        nvme_rw_queued_ex(dev, true, lba + part_base(), buf.as_ptr() as u64, buf.len(), true)
    };
    let dt = dev_note(&DEV_WR_N, &DEV_WR_TSC, t0);
    DEV_FUA_N.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    DEV_FUA_TSC.fetch_add(dt, core::sync::atomic::Ordering::Relaxed);
    DEV_FUA_MAX.fetch_max(dt, core::sync::atomic::Ordering::Relaxed);
    if !ok { return write_failed(); }
    ok
}

/// ONE COMMAND THROUGH THE QUEUE: submit it under a short lock of the ring, wait for one's own
/// completion without it. NVMe goes along the former synchronous path — it has its own
/// queue and its own batch (`rw_many`).
fn io_one(lba: usize, buf: *mut u8, len: usize, write: bool) -> bool {
    let _g = Busy::new();
    // SAFETY: D3 — the buffer handed to the device is a frame the kernel owns (a block-cache slot or a
    // staging page), untouched for the length of the command.
    unsafe {
        match blk() {
            Some(Blk::Virtio(_)) => {}
            // NVMe (EBS): the same scheme as with virtio — a short lock of the ring and
            // a wait for ONE'S OWN completion. The DEVICE's lock is not taken here
            // on purpose: otherwise a read would again stand behind a foreign barrier.
            Some(Blk::Nvme(n)) => {
                let dev: *mut crate::nvme::Nvme = n;
                return nvme_rw_queued(dev, write, lba + part_base(), buf as u64, len);
            }
            None => return false,
        }
        let lba = lba + part_base();
        let slot;
        loop {
            let done = {
                let _r = ring_lock();
                let Some(Blk::Virtio(b)) = blk() else { return false };
                reap_locked(b);
                match slot_take() {
                    None => None,
                    Some(i) => {
                        let ios = &mut *core::ptr::addr_of_mut!(IOS);
                        let sl = ios[i].as_mut().unwrap();
                        let req: *mut _ = &mut sl.req;
                        let resp: *mut _ = &mut sl.resp;
                        let tok = if write {
                            b.write_blocks_nb(lba, &mut *req, core::slice::from_raw_parts(buf, len), &mut *resp)
                        } else {
                            b.read_blocks_nb(lba, &mut *req, core::slice::from_raw_parts_mut(buf, len), &mut *resp)
                        };
                        match tok {
                            Ok(t) => { ios[i].as_mut().unwrap().token = t; Some(i) }
                            Err(_) => { ios[i].as_mut().unwrap().busy = false; None }
                        }
                    }
                }
            };
            match done {
                Some(i) => { IO_SUBMITS.fetch_add(1, core::sync::atomic::Ordering::Relaxed); slot = i; break; }
                None => { IO_QFULL.fetch_add(1, core::sync::atomic::Ordering::Relaxed); core::hint::spin_loop(); }
            }
        }
        if !slot_wait(slot) { return false; }
        let ios = &*core::ptr::addr_of!(IOS);
        ios[slot].as_ref().is_some_and(|sl| sl.resp.status() == virtio_drivers::device::blk::RespStatus::OK)
    }
}
/// The moment of THE START of the last barrier and the number of merged requests (see `blk_flush`).
static FLUSH_STARTED_TSC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// The barriers whose command was lost (the disk did not answer within 10 s): the durability is
/// NOT promised. Until it is carried through to an EIO at fsync — it is open in the TODO; it is counted and printed.
pub static FLUSH_LOST: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DEV_FL_MERGED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// THE DEVICE'S BARRIER WITH A GROUP COMMIT.
///
/// A barrier costs tens of milliseconds (46 ms on this stand: for the host that is an
/// `fdatasync` over the image on a spinning disk), and the device physically does not do more than
/// ~45 of them a second. While the requests come two or three times as often:
/// in a database the journal's background thread and the request threads call `fsync`
/// alternately. The analysis showed exactly that: 1389 barriers, 30.5 s of pure waiting for the
/// device over a run of 60 seconds.
///
/// But a barrier is not a personal service. It makes durable EVERYTHING that has already been
/// given to the device, so a foreign barrier that STARTED LATER than our last
/// write covers us too: there is no point in waiting for our own. That is exactly what the
/// grouping does (in Linux it is the group commit of the jbd2 journal).
///
/// The correctness rests on the order: we take the timestamp BEFORE waiting for the device's
/// lock (our writes are already completed by that moment — they are synchronous), while we
/// compare AFTER taking it. Since the lock is ours, a foreign barrier is already completed
/// entirely; if it started later than our mark, our data were in it.
/// The barriers that were not needed: the device has no volatile write cache.
pub static DEV_FL_NOCACHE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Does this device need a barrier at all.
pub fn flush_needed() -> bool {
    match blk() { Some(Blk::Nvme(n)) => n.has_write_cache(), _ => true }
}
pub fn blk_flush() -> bool {
    if FAIL_WRITES.load(core::sync::atomic::Ordering::Relaxed) { return write_failed(); }
    // THE DEVICE HAS NO VOLATILE CACHE — A BARRIER DOES NOTHING.
    //
    // Our writes are synchronous: the command completed — that means it was accepted by the device,
    // and with VWC=0 "accepted" also means "on the medium". A flush in that case is
    // a command that is guaranteed to carry nothing, but on EBS it costs
    // milliseconds, and EVERY fsync of the workload pays them. Linux asks exactly
    // this and issues no barriers at all on such a device.
    if !flush_needed() {
        DEV_FL_NOCACHE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return true;
    }
    let t0 = crate::tsc::rdtsc();          // everything of ours has already been given to the device
    let _f = flush_lock();
    if FLUSH_STARTED_TSC.load(core::sync::atomic::Ordering::SeqCst) > t0 {
        DEV_FL_MERGED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return true;                       // we are covered by a foreign barrier
    }
    let _g = Busy::new();
    let t1 = crate::tsc::rdtsc();
    FLUSH_STARTED_TSC.store(t1, core::sync::atomic::Ordering::SeqCst);
    // THE BARRIER GOES THROUGH THE QUEUE TOO. That is what everything was done for: while it flies
    // (tens of milliseconds), the ring stays free, and foreign reads
    // go past it rather than behind it.
    let mut queued = false;
    let mut ok = true;
    // SAFETY: D1 + D2 — the ring and the slot table under `ring_lock`; a slot is reused only after its
    // completion has been seen.
    unsafe {
        if let Some(Blk::Nvme(n)) = blk() {
            let dev: *mut crate::nvme::Nvme = n;
            ok = nvme_flush_queued(dev);
            queued = true;
        } else if let Some(Blk::Virtio(_)) = blk() {
            let slot = loop {
                let got = {
                    let _r = ring_lock();
                    let Some(Blk::Virtio(b)) = blk() else { break usize::MAX };
                    reap_locked(b);
                    match slot_take() {
                        None => None,
                        Some(i) => {
                            let ios = &mut *core::ptr::addr_of_mut!(IOS);
                            let sl = ios[i].as_mut().unwrap();
                            let req: *mut _ = &mut sl.req;
                            let resp: *mut _ = &mut sl.resp;
                            match b.flush_nb(&mut *req, &mut *resp) {
                                Ok(Some(t)) => { ios[i].as_mut().unwrap().token = t; Some(i) }
                                // The device cannot do a barrier — and there is nothing to wait for.
                                Ok(None) => { ios[i].as_mut().unwrap().busy = false; break usize::MAX }
                                Err(_) => { ios[i].as_mut().unwrap().busy = false; None }
                            }
                        }
                    }
                };
                match got {
                    Some(i) => { IO_SUBMITS.fetch_add(1, core::sync::atomic::Ordering::Relaxed); break i; }
                    None => { IO_QFULL.fetch_add(1, core::sync::atomic::Ordering::Relaxed); core::hint::spin_loop(); }
                }
            };
            if slot != usize::MAX {
                if !slot_wait(slot) { FLUSH_LOST.fetch_add(1, core::sync::atomic::Ordering::Relaxed); ok = false; }
                else {
                    // The barrier's status was previously not read at all: the device could
                    // answer IOERR, while we considered the data durable.
                    let ios = &*core::ptr::addr_of!(IOS);
                    ok = ios[slot].as_ref().is_some_and(|sl| sl.resp.status() == virtio_drivers::device::blk::RespStatus::OK);
                }
            }
            queued = true;
        }
    }
    if !queued { if let Some(b) = blk() { ok = b.flush().is_ok(); } }
    let d = dev_note(&DEV_FL_N, &DEV_FL_TSC, t1);
    if d > DEV_FL_MAX.load(core::sync::atomic::Ordering::Relaxed) {
        DEV_FL_MAX.store(d, core::sync::atomic::Ordering::Relaxed);
    }
    if !ok { return write_failed(); }
    true
}
/// A write by an ABSOLUTE LBA, past the partition's offset (`talaria.trace=`: the trace
/// lands in the gap between the bootloader and the kernel, where there is no partition at all).
pub fn blk_write_raw(lba: usize, buf: &[u8]) -> bool {
    let _d = dev_lock(); let _g = Busy::new();
    match blk() { Some(b) => b.write_raw(lba, buf).is_ok(), None => false }
}
/// A read by an ABSOLUTE LBA (`talaria.suite=`: the list of the modes and the counter).
pub fn blk_read_raw(lba: usize, buf: &mut [u8]) -> bool {
    let _d = dev_lock(); let _g = Busy::new();
    match blk() { Some(b) => b.read_raw(lba, buf).is_ok(), None => false }
}
/// §8.1/§8.2: a batch of writes `(lba, the buffer's address, the number of 512-sectors)` in one
/// pipeline. On NVMe it goes as a batch of commands with one ring; on virtio-blk —
/// sequentially (there the queue is cheap anyway, the vmexit is amortised by vhost).
pub fn blk_write_many(reqs: &[(usize, u64, u16)]) -> bool {
    if FAIL_WRITES.load(core::sync::atomic::Ordering::Relaxed) { return write_failed(); }
    let _d = dev_lock(); let _g = Busy::new();
    let t0 = crate::tsc::rdtsc();
    let r = blk_write_many_inner(reqs);
    if !r { write_failed(); }
    // A batch counts as ONE write: the question the counter answers is
    // how much time the datapath stood waiting for the disk rather than how many
    // commands there were (their number is printed next to it, in "the ranges").
    dev_note(&DEV_WR_N, &DEV_WR_TSC, t0);
    r
}
/// §8.13: the same batch, but SUBMIT IT AND DO NOT WAIT (see `DEFER_MASK`). It returns
/// `true` if the batch is deferred — then its buffer and the cache channels are occupied until
/// `blk_defer_wait`; `false` — it is written synchronously (virtio, a busy queue,
/// there is already a deferred one), like `blk_write_many`.
pub fn blk_write_many_defer(reqs: &[(usize, u64, u16)]) -> bool {
    if FAIL_WRITES.load(core::sync::atomic::Ordering::Relaxed) { write_failed(); return false; }
    if reqs.is_empty() { return false; }
    let _d = dev_lock(); let _g = Busy::new();
    let t0 = crate::tsc::rdtsc();
    // SAFETY: D3 — the buffer handed to the device is a frame the kernel owns (a block-cache slot or a
    // staging page), untouched for the length of the command.
    let r = unsafe {
        match blk() {
            Some(Blk::Nvme(n)) if reqs.len() <= 64 && DEFER_MASK.load(core::sync::atomic::Ordering::SeqCst) == 0 => {
                let dev: *mut crate::nvme::Nvme = n;
                let mut sh: [(usize, u64, u16); 64] = [(0, 0, 0); 64];
                for (k, &(lba, a, nb)) in reqs.iter().enumerate() { sh[k] = (lba + part_base(), a, nb); }
                match nvme_submit_deferred(dev, &sh[..reqs.len()]) { None => return true, Some(ok) => ok }
            }
            _ => blk_write_many_inner(reqs),
        }
    };
    if !r { write_failed(); }
    dev_note(&DEV_WR_N, &DEV_WR_TSC, t0);
    false
}
/// §8.13 + STEP 2 OF SESSION 40: wait for A FOREIGN deferred batch WITHOUT TOUCHING the sign
/// of its error — it is removed by THE OWNER in `blk_defer_wait`, and stealing it would mean
/// returning a success to a foreign `fsync` with a failed write (that very class of lie
/// because of which the FS and device locks were rewritten).
///
/// Why wait for a foreign one. The journal is no longer occupied for the time of the data flush
/// (`talaria.jrnfree=1`), which means that next to our commit THE LAST
/// portion of a neighbour's data may be flying. With VWC=0 (EBS) the barrier before the commit does
/// nothing and does not cover it, while ALL the dirty metadata of the cache get into our
/// transaction — including those of its file. We wait here: this is one portion
/// (≤ 1 MB) rather than a foreign data phase in its entirety.
pub fn blk_defer_absorb() {
    use core::sync::atomic::Ordering::SeqCst;
    if DEFER_MASK.load(SeqCst) == 0 { return; }
    let _d = dev_lock(); let _g = Busy::new();
    let t0 = crate::tsc::rdtsc();
    // SAFETY: D1 + D2 — the ring and the slot table under `ring_lock`; a slot is reused only after its
    // completion has been seen.
    unsafe {
        if let Some(Blk::Nvme(n)) = blk() {
            let dev: *mut crate::nvme::Nvme = n;
            DEFER_WAITED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if !nvme_batch(dev, true, &[], false) { write_failed(); }
        }
    }
    dev_note(&DEV_WR_N, &DEV_WR_TSC, t0);
}
/// §8.13: wait for the deferred batch (if nobody has waited for it yet) and
/// report whether it landed without an error.
pub fn blk_defer_wait() -> bool {
    use core::sync::atomic::Ordering::SeqCst;
    if DEFER_MASK.load(SeqCst) != 0 {
        let _d = dev_lock(); let _g = Busy::new();
        let t0 = crate::tsc::rdtsc();
        // SAFETY: D1 + D2 — the ring and the slot table under `ring_lock`; a slot is reused only after
        // its completion has been seen.
        unsafe {
            if let Some(Blk::Nvme(n)) = blk() {
                let dev: *mut crate::nvme::Nvme = n;
                DEFER_WAITED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                if !nvme_batch(dev, true, &[], false) { write_failed(); }
            }
        }
        dev_note(&DEV_WR_N, &DEV_WR_TSC, t0);
    }
    !DEFER_ERR.swap(false, SeqCst)
}
fn blk_write_many_inner(reqs: &[(usize, u64, u16)]) -> bool {
    // SAFETY: D3 — the buffer handed to the device is a frame the kernel owns (a block-cache slot or a
    // staging page), untouched for the length of the command.
    unsafe {
        match blk() {
            Some(Blk::Nvme(n)) => {
                let dev: *mut crate::nvme::Nvme = n;
                let mut sh: [(usize, u64, u16); 64] = [(0, 0, 0); 64];
                let mut k = 0;
                for &(lba, a, nb) in reqs {
                    sh[k] = (lba + part_base(), a, nb); k += 1;
                    if k == sh.len() { if !nvme_batch(dev, true, &sh[..k], false) { return false; } k = 0; }
                }
                if k > 0 && !nvme_batch(dev, true, &sh[..k], false) { return false; }
                true
            }
            // A BATCH GOES AS A BATCH RATHER THAN ONE BY ONE. Previously there was a loop of
            // synchronous writes here: 1655 ranges per writeback pass is
            // 1655 full round trips of "submitted — waited", of 40 us each.
            // Now we submit as much as the queue accepts and wait for the
            // completions all at once.
            Some(Blk::Virtio(_)) => {
                let mut ok = true;
                let mut pending: [usize; QD] = [usize::MAX; QD];
                let mut np = 0usize;
                let mut i = 0usize;
                while i < reqs.len() || np > 0 {
                    // submit while there is something and somewhere
                    while i < reqs.len() && np < QD {
                        let (lba, a, nb) = reqs[i];
                        let got = {
                            let _r = ring_lock();
                            let Some(Blk::Virtio(b)) = blk() else { return false };
                            reap_locked(b);
                            match slot_take() {
                                None => None,
                                Some(si) => {
                                    let ios = &mut *core::ptr::addr_of_mut!(IOS);
                                    let sl = ios[si].as_mut().unwrap();
                                    let req: *mut _ = &mut sl.req;
                                    let resp: *mut _ = &mut sl.resp;
                                    let buf = core::slice::from_raw_parts(a as *const u8, nb as usize * 512);
                                    match b.write_blocks_nb(lba + part_base(), &mut *req, buf, &mut *resp) {
                                        Ok(t) => { ios[si].as_mut().unwrap().token = t; Some(si) }
                                        Err(_) => { ios[si].as_mut().unwrap().busy = false; None }
                                    }
                                }
                            }
                        };
                        match got {
                            Some(si) => { IO_SUBMITS.fetch_add(1, core::sync::atomic::Ordering::Relaxed); pending[np] = si; np += 1; i += 1; }
                            None => { IO_QFULL.fetch_add(1, core::sync::atomic::Ordering::Relaxed); break; }
                        }
                    }
                    // collect what was submitted
                    for k in 0..np {
                        let si = pending[k];
                        if !slot_wait(si) { ok = false; continue; }
                        let ios = &*core::ptr::addr_of!(IOS);
                        if !ios[si].as_ref().is_some_and(|sl| sl.resp.status() == virtio_drivers::device::blk::RespStatus::OK) { ok = false; }
                    }
                    np = 0;
                }
                ok
            }
            None => false,
        }
    }
}
/// How many I/O commands the block driver issued (the fsync diagnostics, §8.7).
pub fn blk_cmds() -> u64 { match blk() { Some(Blk::Nvme(n)) => n.cmds, _ => 0 } }

fn name_eq(e: &Entry, norm: &[u8]) -> bool { e.name() == norm }
/// Find an entry by path: Ok(the DIR index) | Err(VF_BASE+k, a virtual one) | None.
fn lookup(path: *const u8) -> Option<i32> {
    // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
    // through `addr_of!` so that no reference to it is formed.
    let path = unsafe { core::slice::from_raw_parts(path, 256) };
    let mut nb = [0u8; NAMELEN];
    let n = normalize(path, &mut nb);
    let norm = &nb[..n];
    // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
    // through `addr_of!` so that no reference to it is formed.
    unsafe { for i in 0..NFILES { if name_eq(&(*addr_of_mut!(DIR))[i], norm) { return Some(i as i32); } } }
    for k in 0..VF_N { if VF_NAME[k] == norm { return Some(VF_BASE + k as i32); } }
    None
}
pub fn exists(path: *const u8) -> bool { if path.is_null() { return false; } lookup(path).is_some() }

/// Is the FS mounted (any: TFS1/TFS2 through the DIR table or ext2).
// SAFETY: D3 — the buffer handed to the device is a frame the kernel owns (a block-cache slot or a
// staging page), untouched for the length of the command.
pub fn mounted() -> bool { unsafe { MOUNTED } }
/// How many entries there are in the TFS directory (for the operator's shell; ext2 has its own tree).
// SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached through
// `addr_of!` so that no reference to it is formed.
pub fn nfiles() -> usize { unsafe { NFILES } }
/// The directory's entry i: (the name, the name's length, the size, whether it is a directory).
pub fn entry(i: usize) -> Option<([u8; NAMELEN], usize, u32, bool)> {
    // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
    // through `addr_of!` so that no reference to it is formed.
    unsafe {
        if i >= NFILES { return None; }
        let e = (*addr_of_mut!(DIR))[i];
        Some((e.name, e.name().len(), e.len, e.dir))
    }
}

pub fn open(path: *const u8) -> i64 {
    let diridx = match lookup(path) { Some(i) => i, None => return -2 }; // ENOENT
    // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
    // through `addr_of!` so that no reference to it is formed.
    unsafe {
        for h in 0..MAXOPEN { // a separate descriptor -> a double open of one file does not alias
            if OF_DIR[h] < 0 { OF_DIR[h] = diridx; OF_OFF[h] = 0; return FD_BASE + h as i64; }
        }
        -24 // EMFILE
    }
}
/// Reset the table of open descriptors (a restart of the workload).
// SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached through
// `addr_of!` so that no reference to it is formed.
pub fn reset_open() { unsafe { for h in 0..MAXOPEN { OF_DIR[h] = -1; OF_OFF[h] = 0; } } }

fn idx(fd: i64) -> Option<usize> { // it returns the handle h
    let h = (fd - FD_BASE) as usize;
    // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
    // through `addr_of!` so that no reference to it is formed.
    if fd >= FD_BASE && h < MAXOPEN && unsafe { core::ptr::read_volatile(core::ptr::addr_of!(OF_DIR[h])) >= 0 } { Some(h) } else { None }
}
// SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached through
// `addr_of!` so that no reference to it is formed.
fn dir_of(h: usize) -> i32 { unsafe { core::ptr::read_volatile(core::ptr::addr_of!(OF_DIR[h])) } }
// SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached through
// `addr_of!` so that no reference to it is formed.
fn entry_len(d: i32) -> u32 { if d >= VF_BASE { unsafe { VF_LEN[(d - VF_BASE) as usize] as u32 } } else { unsafe { (*addr_of_mut!(DIR))[d as usize].len } } }

static mut SECTBUF: [u8; SECTOR_SIZE] = [0; SECTOR_SIZE];
pub fn read(fd: i64, buf: *mut u8, len: usize) -> i64 {
    let i = match idx(fd) { Some(x) => x, None => return -9 };
    // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
    // through `addr_of!` so that no reference to it is formed.
    unsafe {
        let d = dir_of(i);
        let off = OF_OFF[i];
        let flen = entry_len(d);
        if off >= flen { return 0; }
        let want = ((flen - off) as usize).min(len);
        let dst = core::slice::from_raw_parts_mut(buf, want);
        if d >= VF_BASE { // a virtual file
            let k = (d - VF_BASE) as usize;
            dst.copy_from_slice(&VF_DATA[k][off as usize..off as usize + want]);
            OF_OFF[i] = off + want as u32;
            return want as i64;
        }
        let e = (*addr_of_mut!(DIR))[d as usize];
        if e.dir { return -21; } // EISDIR
        let blk = blk().unwrap();
        let mut done = 0usize;
        // A static sector buffer: an identity address -> the virtio-blk DMA is correct even under
        // a non-identity (COW) address space of a process (execve). Reentrancy is not needed (1 core, the syscalls are not nested).
        let sect = &mut *core::ptr::addr_of_mut!(SECTBUF);
        while done < want {
            let abs = off as usize + done;
            let lba = e.lba as usize + abs / SECTOR_SIZE;
            let soff = abs % SECTOR_SIZE;
            if blk.read_blocks(lba, &mut sect[..]).is_err() { break; }
            let n = (SECTOR_SIZE - soff).min(want - done);
            dst[done..done + n].copy_from_slice(&sect[soff..soff + n]);
            done += n;
        }
        OF_OFF[i] = off + done as u32;
        done as i64
    }
}

pub fn lseek(fd: i64, off: i64, whence: i64) -> i64 {
    let i = match idx(fd) { Some(x) => x, None => return -9 };
    // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
    // through `addr_of!` so that no reference to it is formed.
    unsafe {
        let flen = entry_len(dir_of(i));
        let base = match whence { 0 => 0i64, 1 => OF_OFF[i] as i64, 2 => flen as i64, _ => return -22 };
        let n = (base + off).max(0).min(flen as i64);
        OF_OFF[i] = n as u32;
        n
    }
}

fn fill_stat(statbuf: *mut u8, len: u64, dir: bool) {
    // SAFETY: D3 — the buffer handed to the device is a frame the kernel owns (a block-cache slot or a
    // staging page), untouched for the length of the command.
    unsafe {
        core::ptr::write_bytes(statbuf, 0, 144);
        core::ptr::write_unaligned(statbuf.add(8) as *mut u64, 1);   // st_ino
        core::ptr::write_unaligned(statbuf.add(16) as *mut u64, 1);  // st_nlink
        core::ptr::write_unaligned(statbuf.add(24) as *mut u32, if dir { 0o040000 | 0o755 } else { 0o100000 | 0o644 }); // st_mode
        core::ptr::write_unaligned(statbuf.add(48) as *mut i64, len as i64);   // st_size
        core::ptr::write_unaligned(statbuf.add(56) as *mut i64, SECTOR_SIZE as i64); // st_blksize
        core::ptr::write_unaligned(statbuf.add(64) as *mut i64, len.div_ceil(512) as i64); // st_blocks
    }
}
pub fn fstat(fd: i64, statbuf: *mut u8) -> i64 {
    let i = match idx(fd) { Some(x) => x, None => return -9 };
    let d = dir_of(i);
    // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
    // through `addr_of!` so that no reference to it is formed.
    let dir = d < VF_BASE && unsafe { (*addr_of_mut!(DIR))[d as usize].dir };
    fill_stat(statbuf, entry_len(d) as u64, dir);
    0
}
/// A stat by path (without opening a descriptor). -ENOENT if there is none.
pub fn stat_path(path: *const u8, statbuf: *mut u8) -> i64 {
    if path.is_null() { return -14; }
    let d = match lookup(path) { Some(d) => d, None => return -2 };
    // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
    // through `addr_of!` so that no reference to it is formed.
    let dir = d < VF_BASE && unsafe { (*addr_of_mut!(DIR))[d as usize].dir };
    fill_stat(statbuf, entry_len(d) as u64, dir);
    0
}

pub fn close(fd: i64) -> i64 {
    // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
    // through `addr_of!` so that no reference to it is formed.
    if let Some(h) = idx(fd) { unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(OF_DIR[h]), -1); } }
    0
}

/// Read a whole file into a buffer (for loading a binary from the disk). It returns the length.
pub fn read_all(path: &[u8], dst: &mut [u8]) -> usize {
    if crate::ext2::active() { return crate::ext2::read_all(path, dst); }
    let fd = open(path.as_ptr());
    if fd < 0 { return 0; }
    let mut done = 0usize;
    while done < dst.len() {
        let n = read(fd, dst[done..].as_mut_ptr(), dst.len() - done);
        if n <= 0 { break; }
        done += n as usize;
    }
    close(fd);
    done
}

static mut EXE: [u8; NAMELEN + 1] = [0; NAMELEN + 1];
/// Remember the path of the launched binary (for /proc/self/exe).
pub fn set_exe(path: *const u8) {
    // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
    // through `addr_of!` so that no reference to it is formed.
    unsafe {
        let p = core::slice::from_raw_parts(path, NAMELEN);
        let n = p.iter().position(|&c| c == 0).unwrap_or(NAMELEN);
        EXE[..n].copy_from_slice(&p[..n]); EXE[n] = 0;
    }
}
pub fn exe_path(out: &mut [u8]) -> i64 {
    // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
    // through `addr_of!` so that no reference to it is formed.
    unsafe {
        let exe: &[u8] = &*core::ptr::addr_of!(EXE);
        let n = exe.iter().position(|&c| c == 0).unwrap_or(NAMELEN);
        if n == 0 || out.len() < n { return -1; }
        out[..n].copy_from_slice(&exe[..n]);
        n as i64
    }
}

/// For readlink("/proc/self/fd/N"): write "/<the file's path>" of the descriptor fd into out,
/// return the length (for musl's realpath). -1 if the fd is invalid.
pub fn readlink_fd(fd: i64, out: &mut [u8]) -> i64 {
    let h = match idx(fd) { Some(x) => x, None => return -1 };
    let d = dir_of(h);
    let name: &[u8] = if d >= VF_BASE { VF_NAME[(d - VF_BASE) as usize] } else {
        // SAFETY: D4 — this module's own state (the driver, the slots, the partition base), reached
        // through `addr_of!` so that no reference to it is formed.
        let e = unsafe { &(*addr_of_mut!(DIR))[d as usize] };
        let elen = e.name.iter().position(|&c| c == 0).unwrap_or(NAMELEN);
        &e.name[..elen]
    };
    if out.len() < name.len() + 1 { return -1; }
    out[0] = b'/';
    out[1..1 + name.len()].copy_from_slice(name);
    (1 + name.len()) as i64
}
