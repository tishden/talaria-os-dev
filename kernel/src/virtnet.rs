//! The virtio-net driver (the MMIO transport — QEMU/microvm) as a `NetDev`.
//!
//! Here too lives the bookkeeping that used to lie in the common `NicQ` and had
//! nothing to do with it: virtio gives out a TOKEN for every laid-out buffer, and the inverse
//! correspondence "a token -> a buffer's slot" is needed only by this card. Now it has it —
//! and at the same time `NicQ` has stopped carrying it on the cell kernel's stack.

use core::ptr::NonNull;
use virtio_drivers::device::net::VirtIONetRaw;
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};
use virtio_drivers::transport::{DeviceType, Transport};

use crate::hal::TalariaHal;
use crate::netdev::{Bind, Driver, NetDev, FRAME, MAXTX};
use crate::pci::PciDev;
use crate::QS;

pub type Net = VirtIONetRaw<TalariaHal, MmioTransport<'static>, QS>;

/// The virtio-mmio window: the address and the size. It is filled in at the parsing of the cmdline or
/// by a scan of the microvm window (`virtnet::scan_mmio_window`) BEFORE the driver is bound.
pub static MMIO_ADDR: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static MMIO_SIZE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub struct VirtioNic {
    dev: Net,
    rx_slot: [u16; QS],   // a token -> a buffer's slot
    rx_addr: [u64; QS],   // a token -> the buffer's address
    tx_slot: [u16; QS],
    tx_len: [u16; QS],
    tx_addr: [u64; QS],
    tx_free: [bool; MAXTX],
    notify: bool,
    /// The window's address (`intr_regs`).
    mmio: usize,
}

#[inline]
unsafe fn frame(addr: u64) -> &'static mut [u8; FRAME] { &mut *(addr as *mut [u8; FRAME]) }

impl NetDev for VirtioNic {
    fn name(&self) -> &'static str { "virtio-net" }
    fn mac(&self) -> [u8; 6] { self.dev.mac_address() }

    /// The ceiling is the virtqueue's size: the card simply has no more descriptors.
    fn rx_capacity(&self, want: usize) -> usize { want.min(QS) }

    fn rx_post(&mut self, slot: usize, addr: u64) {
        let tok = unsafe { self.dev.receive_begin_no_notify(frame(addr)).expect("rx post") };
        self.rx_slot[tok as usize] = slot as u16;
        self.rx_addr[tok as usize] = addr;
        self.notify = true;
    }

    fn rx_poll(&mut self) -> Option<(usize, usize, usize)> {
        let tok = self.dev.poll_receive()?;
        let slot = self.rx_slot[tok as usize] as usize;
        let addr = self.rx_addr[tok as usize];
        let (hdr, len) = unsafe { self.dev.receive_complete(tok, frame(addr)).ok()? };
        Some((slot, hdr, len))
    }

    fn tx_alloc(&mut self, ntx: usize) -> Option<usize> {
        self.tx_free[..ntx.min(MAXTX)].iter().position(|&x| x)
    }

    /// virtio requires its own header before a frame; its length is the payload's offset.
    fn tx_hdr(&mut self, _slot: usize, addr: u64) -> usize {
        unsafe { self.dev.fill_buffer_header(frame(addr)).expect("hdr") }
    }

    fn tx_post(&mut self, slot: usize, addr: u64, n: usize) {
        let tok = unsafe { self.dev.transmit_begin_no_notify(&frame(addr)[..n]).expect("tx") };
        self.tx_slot[tok as usize] = slot as u16;
        self.tx_len[tok as usize] = n as u16;
        self.tx_addr[tok as usize] = addr;
        self.tx_free[slot] = false;
        self.notify = true;
    }

    fn tx_reap(&mut self) {
        while let Some(tok) = self.dev.poll_transmit() {
            let slot = self.tx_slot[tok as usize] as usize;
            let n = self.tx_len[tok as usize] as usize;
            let addr = self.tx_addr[tok as usize];
            unsafe { self.dev.transmit_complete(tok, &frame(addr)[..n]).expect("txc"); }
            if slot < MAXTX { self.tx_free[slot] = true; }
        }
    }

    fn flush(&mut self) -> bool {
        if !self.notify { return false; }
        let kicked = self.dev.transmit_notify();
        self.dev.receive_notify();
        self.notify = false;
        kicked
    }

    /// §1.2: "to allow" in virtio is to clear the NO_INTERRUPT in the avail ring: under
    /// a hypervisor without MONITOR/MWAIT there is otherwise nothing to wake a sleeper with.
    fn arm_intr(&mut self) { self.dev.enable_recv_interrupts(); }

    fn ack_intr(&mut self) { self.dev.ack_interrupt(); }

    /// InterruptStatus (0x60) and InterruptACK (0x64) of the window: the two registers have
    /// nothing to do with the queues, so the line's handler may clear the status while the
    /// interrupted code is in the middle of a queue operation.
    fn intr_regs(&self) -> Option<(usize, usize)> { Some((self.mmio + 0x60, self.mmio + 0x64)) }

    /// The word (flags,idx) of the receive used ring: the device writes there when completing
    /// a buffer, and that DMA write wakes an MWAIT without an interrupt and without a vmexit.
    fn idle_watch(&self) -> Option<(*const u32, u32)> { Some(self.dev.rx_used_watch()) }
}

// ── the binding ─────────────────────────────────────────────────────────────
/// The slots of the live cards. A card lives until the reboot and travels to the cell by
/// reference (§1.1), so "several cards" means several static slots
/// rather than copies.
const NSLOT: usize = 4;
static mut SLOT: [Option<VirtioNic>; NSLOT] = [None, None, None, None];

/// The virtio-mmio window for the card number `inst`.
///
/// Under a microvm the devices stand in a row with a step of 512 bytes, and the second card is
/// simply the next window with device_id=1. The window of the FIRST card may be set
/// from the outside (`MMIO_ADDR`: the parsing of the cmdline or the common scan in `main`) — then it
/// is considered the first, and the scan gives out the rest, skipping it.
fn find_mmio(inst: usize) -> Option<(usize, usize)> {
    let a0 = MMIO_ADDR.load(core::sync::atomic::Ordering::SeqCst) as usize;
    let sz0 = MMIO_SIZE.load(core::sync::atomic::Ordering::SeqCst) as usize;
    let mut list = [(0usize, 0usize); 8];
    let mut n = 0usize;
    if a0 != 0 { list[0] = (a0, if sz0 != 0 { sz0 } else { 512 }); n = 1; }
    for i in 0..crate::arch::VIRTIO_MMIO_SLOTS {
        if n >= list.len() { break; }
        let Some((addr, size)) = crate::arch::virtio_mmio_slot(i) else { continue };
        let magic = unsafe { core::ptr::read_volatile(addr as *const u32) };
        if magic != 0x7472_6976 { continue; }
        if addr == a0 { continue; }
        let header = match NonNull::new(addr as *mut VirtIOHeader) { Some(h) => h, None => continue };
        if let Ok(t) = unsafe { MmioTransport::new(header, size) } {
            use virtio_drivers::transport::{DeviceType, Transport};
            let net = t.device_type() == DeviceType::Network;
            // IMPORTANT: `MmioTransport` at its destruction RESETS the device
            // (`Drop` writes status=0). Here we only LOOK at what lies in
            // the window — and we have no right to reset a foreign card.
            //
            // How that shoots. The walk "bring all the cards up" calls probe until
            // the first refusal, that is, ONE attempt MORE than there are cards.
            // The last attempt scans the windows anew and with its Drop puts out the card
            // that is already up and working. The window of the first card is protected from that
            // (it is skipped as `a0`), while the second one dies silently:
            // from the outside — "the interface is UP, there is an address, 0 frames received".
            core::mem::forget(t);
            if net { list[n] = (addr, 512); n += 1; }
        }
    }
    if inst >= n { return None; }
    Some(list[inst])
}

fn probe(_: Option<&PciDev>, inst: usize) -> Option<&'static mut dyn NetDev> {
    if inst >= NSLOT { return None; }
    let (a, sz) = find_mmio(inst)?;
    crate::println!("virtio-net: the instance {} — the window {:#x} ({} B)", inst, a, sz);
    let header = NonNull::new(a as *mut VirtIOHeader)?;
    let transport = unsafe { MmioTransport::new(header, sz) }.ok()?;
    let dev: Net = VirtIONetRaw::new(transport).ok()?;
    let nic = VirtioNic {
        dev,
        rx_slot: [0; QS], rx_addr: [0; QS],
        tx_slot: [0; QS], tx_len: [0; QS], tx_addr: [0; QS],
        tx_free: [true; MAXTX], notify: false, mmio: a,
    };
    unsafe {
        let p = core::ptr::addr_of_mut!(SLOT[inst]);
        core::ptr::write(p, Some(nic));
        Some((*p).as_mut().unwrap() as &mut dyn NetDev)
    }
}

pub static DRIVER: Driver = Driver {
    name: "virtio-net",
    // Not PCI: under a microvm there may be no bus at all, the window comes from the cmdline
    // or from a scan (`virtnet::scan_mmio_window`).
    bind: &[Bind::Platform],
    probe,
};

// ── The search for the virtio-mmio window ─────────────────────────────────
//
// IMPORTANT: `MmioTransport` at a `Drop` RESETS the device (`set_status(0)`).
// An inspection is not entitled to put out a foreign card, so both the functions below give the
// transport away to `core::mem::forget` — the price of a debugging session in v1.11, when a scan of the windows
// put out a card that was already working.

/// The virtio-mmio window from `virtio_mmio.device=<size>@<address>:<irq>` in the cmdline.
/// It returns the first window behind which there stands a NETWORK device.
pub fn mmio_window_from_cmdline(cmdline: &str) -> Option<(usize, usize)> {
    for tok in cmdline.split_whitespace() {
        let Some(rest) = tok.strip_prefix("virtio_mmio.device=") else { continue };
        let (size_s, rest) = rest.split_once('@')?;
        let (addr_s, _irq) = rest.split_once(':')?;
        let size = parse_size(size_s)?;
        let addr = parse_num(addr_s)?;
        let header = NonNull::new(addr as *mut VirtIOHeader)?;
        if let Ok(t) = unsafe { MmioTransport::new(header, size) } {
            let net = t.device_type() == DeviceType::Network;
            core::mem::forget(t); // a Drop resets the device — an inspection does not put out a foreign one
            if net { return Some((addr, size)); }
        }
    }
    None
}

/// The virtio-mmio window by an enumeration of the machine's slots (`arch::virtio_mmio_slot`:
/// the microvm's fixed grid at 0xFEB0_0000+512*i on x86, the device tree's windows on arm64),
/// when the firmware has not put a `virtio_mmio.device=` into the cmdline.
pub fn scan_mmio_window() -> Option<(usize, usize)> {
    for i in 0..crate::arch::VIRTIO_MMIO_SLOTS {
        let Some((addr, size)) = crate::arch::virtio_mmio_slot(i) else { continue };
        let magic = unsafe { core::ptr::read_volatile(addr as *const u32) };
        let dev_id = unsafe { core::ptr::read_volatile((addr + 8) as *const u32) };
        if i < 4 || (magic == 0x7472_6976 && dev_id != 0) {
            println!("talaria: mmio slot {} @ {:#x}: magic={:#x} dev_id={}", i, addr, magic, dev_id);
        }
        if magic != 0x7472_6976 { continue; }
        let header = NonNull::new(addr as *mut VirtIOHeader)?;
        if let Ok(t) = unsafe { MmioTransport::new(header, size) } {
            let net = t.device_type() == DeviceType::Network;
            core::mem::forget(t); // a Drop resets the device — an inspection does not put out a foreign one
            if net { return Some((addr, size)); }
        }
    }
    None
}

/// The SLOT NUMBER of the network card on the microvm's grid — from it the number of the interrupt
/// line is derived (see `netsys::netirq_setup`). Separately from `scan_mmio_window`,
/// because that one needs the address, and this one the ordinal number.
pub fn mmio_slot_max() -> usize {
    let mut m = 0;
    for i in 0..crate::arch::VIRTIO_MMIO_SLOTS {
        let Some((addr, _)) = crate::arch::virtio_mmio_slot(i) else { continue };
        if unsafe { core::ptr::read_volatile(addr as *const u32) } == 0x7472_6976 { m = i; }
    }
    m
}

pub fn mmio_slot() -> Option<usize> {
    for i in 0..crate::arch::VIRTIO_MMIO_SLOTS {
        let Some((addr, size)) = crate::arch::virtio_mmio_slot(i) else { continue };
        let magic = unsafe { core::ptr::read_volatile(addr as *const u32) };
        if magic != 0x7472_6976 { continue; }
        let header = NonNull::new(addr as *mut VirtIOHeader)?;
        if let Ok(t) = unsafe { MmioTransport::new(header, size) } {
            let net = t.device_type() == DeviceType::Network;
            core::mem::forget(t); // a Drop resets the device — an inspection does not put out a foreign one
            if net { return Some(i); }
        }
    }
    None
}

/// `<number>` or `<number>K` (as QEMU writes them in `virtio_mmio.device=`).
fn parse_size(s: &str) -> Option<usize> {
    match s.strip_suffix(['K', 'k']) {
        Some(k) => Some(parse_num(k)? * 1024),
        None => parse_num(s),
    }
}

/// Decimal or `0x`-hexadecimal.
fn parse_num(s: &str) -> Option<usize> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => usize::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}
