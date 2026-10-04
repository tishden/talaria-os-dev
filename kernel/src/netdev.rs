//! The model of a network device: what the kernel needs from a card and how a new one is connected.
//!
//! WHY. Before this a card was the enumeration `l1::Nic { Virtio, Ena }`, and the whole
//! difference between the cards was twelve `match`es inside `NicQ`. A third card
//! meant editing every one of them — and a chance to make a mistake in every one. Worse
//! still, SOMEONE ELSE'S bookkeeping lived in `NicQ`: the tables "a virtqueue token -> a buffer
//! slot" are needed by virtio and not needed by the ENA, but lay in a common structure — on the stack
//! of the cell's kernel, because of which they had to be squeezed into `u16` so that the stack did not overflow.
//!
//! NOW. A driver implements `NetDev` and adds ONE line into `DRIVERS`. Its own
//! bookkeeping it keeps to itself. `NicQ`, `SmolDev`, `fastpath::run_cell` and `netsys`
//! no longer know anything about the cards.
//!
//! WHY `dyn` AND NOT A GENERIC. A generic is monomorphised and would cost zero, but
//! the card's type would have to be dragged into the static slot (`NIC_SLOT` — the card lives
//! until the reboot and is not copied on its way to the cell, §1.1) and then into everything that
//! touches it. It is exactly from that that the enumeration once appeared. The price of `dyn` is
//! one indirect call per operation, predictable (there is exactly one target in a run),
//! against an equally predictable jump in the `match` before it. It was measured on
//! a virtio echo: the rx->tx average did not change (`stats[l1]`, the gate `netidle`).
//! If it ever turns up in a profile — `run_cell` is monomorphised separately,
//! without touching the rest.
//!
//! WHAT IS NEEDED TO ADD A CARD (the full list):
//!   1. the file `kernel/src/<name>.rs`: the driver's structure + `impl NetDev for ...`;
//!   2. in it as well `pub static DRIVER: Driver` — the name, the binding rules, the probe;
//!   3. `mod <name>;` in `main.rs` and one line in `DRIVERS` below.
//! Nowhere else. The gate `talaria.test=nicdev` checks exactly that statement on
//! the loopback card (`nicloop.rs`), which is connected exactly this way and no other.

use crate::pci::PciDev;

/// A buffer for a frame (an MTU of 1500 + room for the card's header).
pub const FRAME: usize = 2048;
/// The ceiling on the driver's table of transmit slots.
pub const MAXTX: usize = 128;

/// What the kernel needs from a network card. Everything else — the frame buffers, the batching of the doorbells,
/// the receive counters — is done by `l1::NicQ` on top of this trait, the same way for all.
///
/// A slot is the index of a frame's buffer in the array that `NicQ` keeps. How the driver
/// maps a slot onto its own descriptor (virtio gives out a token, the ENA takes the
/// number itself) is its own business and its own memory.
pub trait NetDev {
    /// The driver's name. It is printed at the binding — the operator is bound to see WHAT
    /// came up, not only that "there is a network".
    fn name(&self) -> &'static str;
    fn mac(&self) -> [u8; 6];

    /// How many receive buffers it makes sense to lay out. The ceiling differs between cards
    /// (the virtqueue in virtio, the ring's depth minus one in the ENA — a full ring is
    /// indistinguishable from an empty one), and only the driver knows it.
    fn rx_capacity(&self, want: usize) -> usize { want }
    /// Lay the buffer `addr` out into the slot `slot` of the receive ring. The doorbell is deferred
    /// until `flush()`.
    fn rx_post(&mut self, slot: usize, addr: u64);
    /// A received frame: (the slot, the payload's offset in the buffer, the length). None — it is empty.
    fn rx_poll(&mut self) -> Option<(usize, usize, usize)>;

    /// A free transmit slot among the first `ntx`. None — all the frames are in flight.
    fn tx_alloc(&mut self, ntx: usize) -> Option<usize>;
    /// The card's header at the start of a transmit buffer; return the payload's offset.
    fn tx_hdr(&mut self, slot: usize, addr: u64) -> usize { let _ = (slot, addr); 0 }
    /// Put a frame (the first `n` bytes of the buffer `addr`) into the transmit queue. Without a doorbell.
    fn tx_post(&mut self, slot: usize, addr: u64, n: usize);
    /// Collect the completed transmissions (free the buffers' slots).
    fn tx_reap(&mut self);
    /// Collect the transmit completions OUTSIDE a pass (session 32: the completion's delay
    /// is measured with a precision of who read it). By default — nothing.
    fn tx_watch(&mut self) {}

    /// Ring the device about everything accumulated over the pass: the transmit tail,
    /// the replenishment of the receive ring, the heads of the completion queues. An MMIO write is
    /// the most expensive operation of the path, so there is one per batch, not per frame.
    /// true — the transmit doorbell really went out (the metric of the batch's amortisation).
    fn flush(&mut self) -> bool;

    /// Allow the receive interrupt (before going to sleep on the ring).
    fn arm_intr(&mut self) {}
    /// Clear the interrupt status at the device (the line's handler). Until it is cleared,
    /// virtio-mmio keeps the line raised, and the handler would be called endlessly.
    fn ack_intr(&mut self) {}
    /// The same clearing WITHOUT a borrow of the driver: the addresses of the device's
    /// interrupt-status register (read it) and of its acknowledge register (write what was
    /// read). The line's handler interrupts whoever is inside the driver on this core, so it
    /// cannot wait for the driver and must not skip the clearing either: a LEVEL line (the
    /// GIC's SPI of a virtio-mmio window) stays raised and the handler is entered again
    /// before the interrupted code runs one instruction. None — the driver's interrupt needs
    /// no clearing (MSI-X) or cannot be cleared this way.
    fn intr_regs(&self) -> Option<(usize, usize)> { None }
    /// The card's line by an MSI/MSI-X message (the ENA): ALLOW the device's interrupts
    /// and return the IDT vector they arrive on — the line's handler is put on it
    /// by `netsys::netirq_setup`. None — this card's line goes only through the
    /// I/O APIC (virtio-mmio) or it has none.
    fn irq_vector(&mut self) -> Option<u8> { None }
    /// What to sleep on while idle: (the word's address, the expected value). The device
    /// writes a completion there, and that DMA write wakes an MWAIT without an interrupt.
    fn idle_watch(&self) -> Option<(*const u32, u32)> { None }
    /// Whether there is a received frame RIGHT NOW (by a sign of the ring itself rather than
    /// by a comparison of values between passes). None — the driver cannot do it.
    fn rx_ready(&self) -> Option<bool> { None }
    /// How many MMIO doorbells the driver has made (None — it does not count).
    fn doorbells(&self) -> Option<u64> { None }
}

/// How a driver recognises its own device.
pub enum Bind {
    /// A vendor:device pair on the PCI bus.
    Pci(u16, u16),
    /// A PCI class (`class<<16 | subclass<<8 | progif`) by a mask: that is how the
    /// cards are bound for which not the vendor but the type of the function matters.
    PciClass(u32, u32),
    /// Not PCI at all: the device is declared by the platform. virtio-mmio on a microvm lives
    /// exactly like that — there may be no PCI bus there, the address comes from the cmdline or
    /// from a scan of a window.
    Platform,
    /// Only at the explicit request `talaria.nic=<name>`. The driver is not
    /// picked up by itself — for stub and debugging cards.
    OnDemand,
}

/// A line of the table of drivers.
pub struct Driver {
    pub name: &'static str,
    pub bind: &'static [Bind],
    /// Bring the device up. `dev` is the found PCI function (None for `Platform`
    /// and `OnDemand`), `inst` is WHICH instance of this driver
    /// is being brought up (a machine happens to have several identical cards: several ENIs on
    /// an instance, two virtio-nets under QEMU). It returns a reference to the driver's own STATIC
    /// slot: the card lives until the reboot and travels to the cell by
    /// reference rather than as a copy (§1.1 — otherwise an AP's stack would have to be kept at 128 KB).
    pub probe: fn(Option<&PciDev>, usize) -> Option<&'static mut dyn NetDev>,
}

/// THE TABLE OF DRIVERS. The order = the priority of binding.
pub static DRIVERS: &[&Driver] = &[
    &crate::virtnet::DRIVER,
    &crate::virtnet::PCI_DRIVER,
    &crate::ena::DRIVER,
    &crate::nicloop::DRIVER,
];

/// The name of the driver demanded by the operator (`talaria.nic=<name>`); empty — automatic.
static mut WANT: [u8; 16] = [0; 16];
static WANT_LEN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Demand a specific driver by name (the operator is always senior to the automatics).
pub fn force(name: &str) {
    let n = name.len().min(16);
    unsafe {
        let p = core::ptr::addr_of_mut!(WANT) as *mut u8;
        for i in 0..n { core::ptr::write(p.add(i), name.as_bytes()[i]); }
    }
    WANT_LEN.store(n, core::sync::atomic::Ordering::SeqCst);
}

/// Parse `talaria.nic=<name>` from the command line.
pub fn want_from_cmdline(cmdline: &str) {
    for tok in cmdline.split_whitespace() {
        if let Some(v) = tok.strip_prefix("talaria.nic=") {
            force(v);
            crate::println!("netdev: the driver '{}' has been demanded (talaria.nic=)", &v[..v.len().min(16)]);
        }
    }
}
fn want() -> Option<&'static str> {
    let n = WANT_LEN.load(core::sync::atomic::Ordering::SeqCst);
    if n == 0 { return None; }
    unsafe { core::str::from_utf8(&*core::ptr::addr_of!(WANT).cast::<[u8; 16]>()).ok().map(|s| &s[..n]) }
}

/// Whether a PCI device fits a binding rule. It is public: there is a gate on this function —
/// a mistake here does not crash, it QUIETLY gives a card to a foreign driver.
pub fn matches(b: &Bind, d: &PciDev) -> bool {
    match *b {
        Bind::Pci(v, dev) => d.vendor == v && d.device == dev,
        Bind::PciClass(c, mask) => d.class & mask == c & mask,
        Bind::Platform | Bind::OnDemand => false,
    }
}

/// Find a device for a driver and bring it up. It returns (the card, the MAC).
///
/// The order: first `talaria.nic=<name>` (the operator is always senior to the automatics),
/// then the table from top to bottom. The first driver that comes up is the card.
pub fn probe_all() -> Option<(&'static mut dyn NetDev, [u8; 6])> {
    let forced = want();
    // "Give me a card" is always THE FIRST card, even if it has already been opened.
    // That is what §0.1 does: the early stack of the user-data is taken down, and the mode's branch opens
    // the same card anew. The enumeration (`probe_next`) is separate semantics.
    reset_enumeration();
    match probe_next() {
        Some((nd, mac, _)) => Some((nd, mac)),
        None => {
            // An explicitly demanded driver and "there are no more cards" are different events, and
            // keeping silent about the first one is impossible: otherwise "the card did not come up" and "the driver is
            // the wrong one" look the same.
            if let Some(w) = forced { crate::println!("netdev: the driver '{}' has been demanded, but the device did not come up", w); }
            None
        }
    }
}

/// Bring up the instance `inst` of this driver, having skipped the already occupied PCI functions.
/// `skip` — how many matching devices to skip (somebody has already taken them).
fn probe_one_n(drv: &Driver, inst: usize, skip: &mut usize) -> Option<&'static mut dyn NetDev> {
    let forced = want() == Some(drv.name);
    for b in drv.bind {
        match b {
            // OnDemand is only at the operator's explicit demand. Otherwise the enumeration
            // "bring all the cards up" would have reached the loopback card and started it as
            // a real interface (the nicdev gate is valuable exactly because the loop looks
            // like an ordinary card).
            Bind::OnDemand if !forced => continue,
            Bind::Platform | Bind::OnDemand => {
                if let Some(nd) = (drv.probe)(None, inst) { return Some(nd); }
            }
            _ => {
                for i in 0..crate::pci::ndev() {
                    if let Some(d) = crate::pci::dev_at(i) {
                        if matches(b, &d) {
                            if *skip > 0 { *skip -= 1; continue; }
                            if let Some(nd) = (drv.probe)(Some(&d), inst) { return Some(nd); }
                        }
                    }
                }
            }
        }
    }
    None
}

// ── the enumeration of ALL the machine's cards ────────────────────────────────
/// How many instances of every driver have already been brought up. Both the driver (which
/// slot to occupy) and the PCI walk (which functions have already been given away) need it.
static mut OPENED: [usize; 8] = [0; 8];

/// Start the enumeration of the cards anew: the next `probe_next` will give out the first one again.
pub fn reset_enumeration() { unsafe { for i in 0..8 { OPENED[i] = 0; } } }

/// Bring up the NEXT card not yet brought up. `None` — there are no more cards.
/// It returns (the card, the MAC, the driver's name).
///
/// It is exactly this way that the node learns that it has not one network but several: before, the walk
/// ended on the first card that came up, and a second ENI/vNIC simply did not exist
/// for the OS.
pub fn probe_next() -> Option<(&'static mut dyn NetDev, [u8; 6], &'static str)> {
    let forced = want();
    for (di, drv) in DRIVERS.iter().enumerate() {
        if let Some(w) = forced { if drv.name != w { continue; } }
        if di >= 8 { break; }
        let inst = unsafe { OPENED[di] };
        let mut skip = inst; // the matching PCI functions already given to this driver
        if let Some(nd) = probe_one_n(drv, inst, &mut skip) {
            unsafe { OPENED[di] = inst + 1; }
            let mac = nd.mac();
            crate::println!("netdev: the card '{}' #{} — mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                drv.name, inst, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
            return Some((nd, mac, drv.name));
        }
        if forced.is_some() { return None; }
    }
    None
}

/// What the table can do and what it sees — for the console and the gate. It prints the drivers,
/// the devices that match them and HOW each one is able to interrupt: a card with INTx
/// alone will come up and will keep silent, and that has to be known in advance rather than from
/// "the datapath works, but one must not sleep on the ring".
pub fn dump() {
    crate::println!("netdev: drivers in the table {}, PCI devices {}", DRIVERS.len(), crate::pci::ndev());
    for drv in DRIVERS {
        let mut hits = 0;
        let mut ondemand = false;
        for b in drv.bind {
            match b {
                Bind::Platform => {}
                Bind::OnDemand => ondemand = true,
                _ => {
                    for i in 0..crate::pci::ndev() {
                        if let Some(d) = crate::pci::dev_at(i) {
                            if matches(b, &d) {
                                hits += 1;
                                crate::println!("netdev:     {:02x}:{:02x}.{} {:04x}:{:04x} the class {:#08x} the interrupts {}",
                                    d.bus, d.dev, d.func, d.vendor, d.device, d.class, crate::pci::intr_kind(&d));
                            }
                        }
                    }
                }
            }
        }
        crate::println!("netdev:   {} — rules {}, matching devices {}{}",
            drv.name, drv.bind.len(), hits, if ondemand { " (only by talaria.nic=)" } else { "" });
    }
}
