//! PCI(e) through ECAM (PLAN-V1.1 §4.3). The ECAM base comes from the ACPI MCFG (UEFI/EC2, q35)
//! with a fallback of 0xE000_0000 (QEMU microvm `pcie=on`, there is no MCFG). A full enumeration of
//! bus/dev/func (bridges -> the whole q35 hierarchy), a table of devices for the drivers
//! (ENA 1d0f:ec20/0ec2, NVMe class 0x0108), the MSI-X capability. The BARs: the firmware
//! (OVMF/EC2) has already assigned them — we respect that; without firmware (microvm) we assign them ourselves
//! out of the window 0xC000_0000. ivshmem-plain (1af4:1110) is the channel of the harness's shm rings.
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const ECAM_DEFAULT: u64 = 0xE000_0000; // microvm pcie=on (without an MCFG)
const MMIO_ALLOC_START: u64 = 0xC000_0000;
static ECAM: AtomicU64 = AtomicU64::new(0);
static PORTIO: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static MMIO_NEXT: AtomicU64 = AtomicU64::new(MMIO_ALLOC_START);
/// The next free address INSIDE the bridge's window (A11): when assigning a BAR ourselves, we
/// are bound to land in the window that the bridge forwards, otherwise the device does not answer.
static BRIDGE_NEXT: AtomicU64 = AtomicU64::new(0);
static IVSHMEM_BASE: AtomicU64 = AtomicU64::new(0);
static IVSHMEM_SIZE: AtomicU64 = AtomicU64::new(0);

// the table of the devices found (function 0 and multifunctions)
// A9: it used to be 32 — that was enough for Nitro and QEMU guests, but NOT for bare metal.
// Measured on a c6i.metal: 52 bridges + 307 functions (the uncore/PCU of two sockets). With
// 32 places the table ran out long before the NVMe, with 192 another 115 did not fit, and
// among them there could have been the network card. 512 covers the machine with room to spare.
pub const MAX_DEV: usize = 512;
#[derive(Clone, Copy, Default)]
pub struct PciDev {
    pub bus: u8, pub dev: u8, pub func: u8,
    pub vendor: u16, pub device: u16,
    pub class: u32,          // class<<16|subclass<<8|progif
    pub msix_cap: u16,       // the offset of the MSI-X capability (0 — there is none)
    pub msi_cap: u16,        // the offset of the MSI capability (0 — there is none)
}
static mut DEVS: [PciDev; MAX_DEV] = [PciDev { bus: 0, dev: 0, func: 0, vendor: 0, device: 0, class: 0, msix_cap: 0, msi_cap: 0 }; MAX_DEV];
static NDEV: AtomicUsize = AtomicUsize::new(0);
static NBRIDGE: AtomicUsize = AtomicUsize::new(0);   // §4.8: how many bridges were skipped (a counter only)
static NLOST: AtomicUsize = AtomicUsize::new(0);     // A9: how many devices did not fit into the table

#[inline] fn ecam() -> u64 { ECAM.load(Ordering::Relaxed) }
/// §4.8: THE TOP BUS of the ECAM window (from the MCFG). We compute the config's offset from bus 0
/// rather than from the record's start_bus, although formally the spec says the opposite: the MCFG on a c7i
/// gives out two records with ONE base (buses 1..35 and 36..42), and it was checked on the spot
/// that at base+0 lies the config of bus 0 (the same host bridge/VGA/NVMe that
/// Linux sees). That is, AWS's start_bus is decorative, and the window is linear from zero.
static ECAM_BUSN: AtomicUsize = AtomicUsize::new(255);
#[inline] fn ecam_covers(bus: u8) -> bool {
    ecam() != 0 && (bus as usize) <= ECAM_BUSN.load(Ordering::Relaxed)
}
#[inline] fn cfg(bus: u8, dev: u8, func: u8, off: u16) -> *mut u32 {
    (ecam() + ((bus as u64) << 20) + ((dev as u64) << 15) + ((func as u64) << 12) + off as u64) as *mut u32
}
// Legacy access to the config through the ports 0xCF8/0xCFC: on EC2 virtual machines (Nitro
// emulates an i440FX) there is no MCFG in the ACPI at all, ECAM is unavailable — only this works.
// The limitation: the first 256 bytes of the config (the BARs and the capability list fit in there).
#[inline] fn io_out32(port: u16, v: u32) { unsafe { crate::arch::cpu::outl(port, v) } }
#[inline] fn io_in32(port: u16) -> u32 { unsafe { crate::arch::cpu::inl(port) } }
#[inline] fn cf8(bus: u8, dev: u8, func: u8, off: u16) -> u32 {
    0x8000_0000 | ((bus as u32) << 16) | ((dev as u32) << 11) | ((func as u32) << 8) | (off as u32 & 0xFC)
}
#[inline] fn rd(bus: u8, dev: u8, func: u8, off: u16) -> u32 {
    if ecam_covers(bus) { unsafe { core::ptr::read_volatile(cfg(bus, dev, func, off)) } }
    else { io_out32(0xCF8, cf8(bus, dev, func, off)); io_in32(0xCFC) }
}
#[inline] fn wr(bus: u8, dev: u8, func: u8, off: u16, v: u32) {
    if ecam_covers(bus) { unsafe { core::ptr::write_volatile(cfg(bus, dev, func, off), v) } }
    else { io_out32(0xCF8, cf8(bus, dev, func, off)); io_out32(0xCFC, v) }
}

/// Choose the way of accessing the config and check that the bus answers.
/// The priority: ECAM from the ACPI MCFG (UEFI/q35/metal) -> the default microvm ECAM
/// (if a device is really visible through it) -> the legacy ports 0xCF8 (EC2 virtual machines).
pub fn present() -> bool {
    if ECAM.load(Ordering::Relaxed) == 0 && !PORTIO.load(Ordering::Relaxed) {
        let mcfg = crate::acpi::ecam_base();
        if mcfg != 0 {
            // §4.8: on the 7th generation (c7i and newer) the ECAM window lies ABOVE 4 GB
            // (0x1_4000_0000), while the kernel identity-maps only the first 4 GB — the very
            // first read of the config gave a #PF in ring 0 right after the MCFG was parsed.
            // We map the window the same way as the 64-bit BARs (a 1G leaf, uncached).
            if mcfg >= 0x1_0000_0000 {
                // the ECAM window is 1 MB per bus; we take it with room for 256 buses (256 MB)
                let last = mcfg + (256u64 << 20) - 1;
                let mut p = mcfg & !((1u64 << 30) - 1);
                while p <= last {
                    if crate::paging::map_mmio_1g(p) == 0 {
                        crate::println!("pci: the ECAM window {:#x} was not mapped — the config is unavailable", p);
                    }
                    p += 1u64 << 30;
                }
            }
            ECAM_BUSN.store(crate::acpi::ecam_bus_end() as usize, Ordering::Relaxed);
            ECAM.store(mcfg, Ordering::Relaxed);
        } else {
            // A9: without an MCFG we first try THE LEGACY PORTS, and only then the default
            // ECAM. The order used to be the other way round, and that is dangerous on exactly bare metal:
            // the ECAM_DEFAULT window (0xE0000000) there belongs not to us, and a blind
            // read at that physical address is an access to an undecoded
            // MMIO of a foreign machine (at best 0xFFFFFFFF, at worst a #MC).
            // The ports 0xCF8/0xCFC, on the contrary, exist on any x86 and are safe, and on a
            // microvm they simply give out 0xFFFFFFFF — and then we honestly go
            // to the default ECAM, as before.
            PORTIO.store(true, Ordering::Relaxed);
            let id = rd(0, 0, 0, 0);
            if id == 0xFFFF_FFFF || id == 0 {
                PORTIO.store(false, Ordering::Relaxed);
                ECAM.store(ECAM_DEFAULT, Ordering::Relaxed);
                if rd(0, 0, 0, 0) == 0xFFFF_FFFF || rd(0, 0, 0, 0) == 0 {
                    ECAM.store(0, Ordering::Relaxed);      // and the ECAM is empty -> the legacy ports
                    PORTIO.store(true, Ordering::Relaxed);
                }
            }
        }
    }
    let id = rd(0, 0, 0, 0);
    id != 0xFFFF_FFFF && id != 0
}

/// A11: find the memory window of the bridge behind which the bus `bus` lives.
///
/// A bridge (class 0x0604) forwards upwards only the addresses from its own windows: Memory
/// Base/Limit (0x20/0x22, a granularity of 1 MB) and Prefetchable Base/Limit
/// (0x24/0x26, on 64-bit bridges — with the upper halves in 0x28/0x2C).
/// Everything outside the window does not reach the device. It returns (the base, the limit+1)
/// of the non-prefetchable window — the one where the devices' registers are put.
fn bridge_window(bus: u8) -> Option<(u64, u64)> {
    if bus == 0 { return None; }              // the root bus: there is no window, the addresses are direct
    let last = if ecam() != 0 { crate::acpi::ecam_bus_end() } else { 255 };
    // There may be several levels of bridges above a bus. One has to take the window of the
    // NEAREST one (it has the greatest secondary): the upper bridge's window is wider, but
    // an address from it does not have to land in the lower one's window, and both forward.
    let mut best: Option<(u8, u64, u64)> = None;
    let mut b = 0u16;
    while b <= last as u16 {
        for d in 0..32u8 {
            for f in 0..8u8 {
                let id = rd(b as u8, d, f, 0);
                if id == 0xFFFF_FFFF || id == 0 { if f == 0 { break; } else { continue; } }
                if (rd(b as u8, d, f, 8) >> 16) != 0x0604 { continue; }  // not a bridge
                let sec = ((rd(b as u8, d, f, 0x18) >> 8) & 0xFF) as u8;
                let sub = ((rd(b as u8, d, f, 0x18) >> 16) & 0xFF) as u8;
                if bus < sec || bus > sub { continue; }
                let mem = rd(b as u8, d, f, 0x20);
                let base = ((mem & 0xFFF0) as u64) << 16;
                let limit = (((mem >> 16) & 0xFFF0) as u64) << 16 | 0xFFFFF;
                if limit <= base { continue; }  // the bridge's window is empty
                if best.is_none_or(|(s, _, _)| sec > s) { best = Some((sec, base, limit + 1)); }
            }
        }
        b += 1;
    }
    best.map(|(_, base, end)| (base, end))
}

/// A BAR (32/64-bit MMIO): read the address assigned by the firmware; if the BAR is empty
/// (a microvm without firmware) — determine the size with a probe and assign one out of the window.
/// It returns (base, size); (0,0) — the BAR is absent/IO-space.
pub fn bar(bus: u8, dev: u8, func: u8, idx: u16) -> (u64, u64) {
    let off = 0x10 + idx * 4;
    let lo = rd(bus, dev, func, off);
    if lo & 1 != 0 { crate::println!("pci: BAR{} of {:02x}:{:02x}.{} — IO-space, skipped", idx, bus, dev, func); return (0, 0); }
    let is64 = lo & 0x6 == 0x4;
    let cur = if is64 { ((rd(bus, dev, func, off + 4) as u64) << 32) | (lo as u64 & !0xF) } else { lo as u64 & !0xF };
    // the size: a write-all-ones probe (the value is restored/assigned below)
    wr(bus, dev, func, off, 0xFFFF_FFFF);
    let szlo = rd(bus, dev, func, off);
    let szhi = if is64 { wr(bus, dev, func, off + 4, 0xFFFF_FFFF); rd(bus, dev, func, off + 4) } else { 0xFFFF_FFFF };
    let mask = if is64 { ((szhi as u64) << 32) | (szlo as u64 & !0xF) } else { 0xFFFF_FFFF_0000_0000 | (szlo as u64 & !0xF) };
    let size = (!mask).wrapping_add(1) & 0xFFFF_FFFF_FFFF;
    if size == 0 {
        crate::println!("pci: BAR{} of {:02x}:{:02x}.{}: the size is 0 (lo={:#x} szlo={:#x} szhi={:#x})", idx, bus, dev, func, lo, szlo, szhi);
        wr(bus, dev, func, off, lo); return (0, 0);
    }
    // A firmware address above 4 GB (UEFI puts a 64-bit BAR into a window of the form 0x38_0000_0000)
    // is outside the kernel's identity map. It MUST NOT be reassigned (the windows of the host bridge/root port
    // are programmed by the firmware — on a real EC2 the device would disappear from the bus):
    // instead we add a 1G uncached window into the tables (§4.3).
    if cur >= 0x1_0000_0000 {
        wr(bus, dev, func, off, lo);
        if is64 { wr(bus, dev, func, off + 4, (cur >> 32) as u32); }
        let cmd = rd(bus, dev, func, 4);
        wr(bus, dev, func, 4, cmd | 0x6);
        if crate::paging::map_mmio_1g(cur) == 0 { crate::println!("pci: BAR{} {:#x} was not mapped", idx, cur); return (0, 0); }
        return (cur, size);
    }
    let base = if cur != 0 { cur } else {
        // A11: the firmware did NOT assign an address. Under QEMU (a microvm without firmware) one can
        // take any free one out of one's own window — there are no bridges there and whatever we
        // write into the BAR the device decodes. On bare metal that MUST NOT be done:
        // an address that does not land in the parent bridge's window simply does not
        // reach the device. That is exactly what happened on a c6i.metal: the firmware's BIOS mode does not
        // touch what it does not boot from, the ENA was left without a BAR, the kernel gave it its own
        // 0xC0000000 — and all its registers read as 0xFFFFFFFF.
        // So first we look for the window of the bridge behind which this bus lives, and take an
        // address OUT OF IT; the window of our own allocator stays a fallback for QEMU.
        match bridge_window(bus) {
            Some((wbase, wlimit)) => {
                let a = (BRIDGE_NEXT.load(Ordering::Relaxed).max(wbase) + size - 1) & !(size - 1);
                if a + size <= wlimit {
                    BRIDGE_NEXT.store(a + size, Ordering::Relaxed);
                    crate::println!("pci: BAR{} of {:02x}:{:02x}.{} was not assigned by the firmware — I take {:#x} out of the bridge's window {:#x}..{:#x}",
                        idx, bus, dev, func, a, wbase, wlimit);
                    a
                } else {
                    crate::println!("pci: BAR{} of {:02x}:{:02x}.{}: the bridge's window {:#x}..{:#x} is too small for {} KB",
                        idx, bus, dev, func, wbase, wlimit, size / 1024);
                    return (0, 0);
                }
            }
            None => {
                let a = (MMIO_NEXT.load(Ordering::Relaxed) + size - 1) & !(size - 1);
                MMIO_NEXT.store(a + size, Ordering::Relaxed);
                a
            }
        }
    };
    wr(bus, dev, func, off, (base as u32) | (lo & 0xF));
    if is64 { wr(bus, dev, func, off + 4, (base >> 32) as u32); }
    // command: memory space enable + bus master
    let cmd = rd(bus, dev, func, 4);
    wr(bus, dev, func, 4, cmd | 0x6);
    (base, size)
}

/// The offset of a capability by its id (a walk over the list; 0 — there is none).
fn find_cap(bus: u8, dev: u8, func: u8, id: u8) -> u16 {
    if rd(bus, dev, func, 4) >> 16 & 0x10 == 0 { return 0; } // status.cap-list
    let mut off = (rd(bus, dev, func, 0x34) & 0xFC) as u16;
    for _ in 0..48 {
        if off == 0 { return 0; }
        let hdr = rd(bus, dev, func, off);
        if hdr as u8 == id { return off; }
        off = ((hdr >> 8) & 0xFC) as u16;
    }
    0
}

fn probe_func(bus: u8, dev: u8, func: u8) {
    let id = rd(bus, dev, func, 0);
    if id == 0xFFFF_FFFF || id == 0 { return; }
    let (vendor, device) = ((id & 0xFFFF) as u16, (id >> 16) as u16);
    let class = rd(bus, dev, func, 8) >> 8; // class|subclass|progif
    let hdr = (rd(bus, dev, func, 0x0C) >> 16) as u8;
    let msix = find_cap(bus, dev, func, 0x11);
    let msi = find_cap(bus, dev, func, 0x05);
    let bridge = hdr & 0x7F == 1 && class >> 8 == 0x0604;
    // §4.8: we do not put BRIDGES into the device table and do not print them in the log one by one:
    // on a c7i there are about a hundred of them (empty hot-plug slots), they overflowed the table and
    // displaced the real devices from it — the drivers "could not find" the card.
    let n = NDEV.load(Ordering::Relaxed);
    // A bus may be met twice (a direct walk over the ECAM window and a descent through
    // a bridge) — do not start a duplicate.
    let dup = (0..n).any(|i| { let d = unsafe { DEVS[i] }; d.bus == bus && d.dev == dev && d.func == func });
    if !bridge && !dup {
        if n < MAX_DEV {
            unsafe { DEVS[n] = PciDev { bus, dev, func, vendor, device, class, msix_cap: msix, msi_cap: msi }; }
            NDEV.store(n + 1, Ordering::Relaxed);
        } else {
            // An overflow of the table is bound to be LOUD: a silent displacement
            // of a real device from it has already cost a session.
            NLOST.fetch_add(1, Ordering::Relaxed);
        }
    }
    if dup { return; }
    if bridge { NBRIDGE.fetch_add(1, Ordering::Relaxed); } else {
        crate::println!("pci: {:02x}:{:02x}.{} {:04x}:{:04x} class={:#08x}{}",
            bus, dev, func, vendor, device, class, if msix != 0 { " msi-x" } else { "" });
    }
    // A PCI-PCI bridge (class 0x0604xx): recursively the secondary bus
    if bridge {
        let sec = (rd(bus, dev, func, 0x18) >> 8) as u8;
        if sec != 0 && sec != bus { scan_bus(sec); }
    }
    if vendor == 0x1af4 && device == 0x1110 {
        let (b, s) = bar(bus, dev, func, 2);
        if b != 0 {
            IVSHMEM_BASE.store(b, Ordering::SeqCst); IVSHMEM_SIZE.store(s, Ordering::SeqCst);
            crate::println!("pci: ivshmem BAR2 -> {:#x} ({} KB) — the host's shared memory", b, s / 1024);
        }
    }
}

// §4.8: which buses have already been walked. One and the same bus is reachable both by a direct walk
// over the ECAM window and by a descent through a bridge — without a mark the walk was repeated (on a c7i that
// meant hundreds of extra config reads and duplicates in the log).
static mut BUS_SEEN: [u64; 4] = [0; 4];
fn bus_first_visit(bus: u8) -> bool {
    unsafe {
        let (w, b) = ((bus >> 6) as usize, bus & 63);
        if BUS_SEEN[w] & (1 << b) != 0 { return false; }
        BUS_SEEN[w] |= 1 << b;
        true
    }
}
fn scan_bus(bus: u8) {
    if !bus_first_visit(bus) { return; }
    for dev in 0..32u8 {
        let id = rd(bus, dev, 0, 0);
        if id == 0xFFFF_FFFF || id == 0 { continue; }
        probe_func(bus, dev, 0);
        if (rd(bus, dev, 0, 0x0C) >> 16) & 0x80 != 0 { // multifunction
            for f in 1..8u8 { probe_func(bus, dev, f); }
        }
    }
}

/// Forget the chosen way of accessing the config (A9). Exactly one place needs it:
/// the early trace `talaria.trace=`, which brings the NVMe up BEFORE the ACPI is parsed. Without
/// a reset in `present()` the legacy path 0xCF8 would stick, and the regular enumeration would
/// no longer see the ECAM window from the MCFG — that is, on the 7th generation the network
/// card would disappear. The NVMe device itself, brought up before the reset, keeps working:
/// its BARs have already been read and mapped.
pub fn reset_access() {
    ECAM.store(0, Ordering::Relaxed);
    ECAM_BUSN.store(255, Ordering::Relaxed);
    PORTIO.store(false, Ordering::Relaxed);
    NDEV.store(0, Ordering::Relaxed);
    NBRIDGE.store(0, Ordering::Relaxed);
    unsafe { BUS_SEEN = [0; 4]; }
}

/// A full enumeration from bus 0 (bridges — recursively).
pub fn init() {
    if !present() { crate::println!("pci: there is no ECAM (a microvm without pcie=on) — skipped"); return; }
    if ecam() != 0 {
        crate::println!("pci: ECAM {:#x} ({})", ecam(), if crate::acpi::ecam_base() != 0 { "MCFG" } else { "microvm" });
    } else {
        crate::println!("pci: the config through the ports 0xCF8/0xCFC (legacy, there is no MCFG)");
    }
    NDEV.store(0, Ordering::Relaxed); NBRIDGE.store(0, Ordering::Relaxed); NLOST.store(0, Ordering::Relaxed);
    unsafe { BUS_SEEN = [0; 4]; }
    scan_bus(0);
    // §4.8: on the 7th generation (c7i) on bus 0 there stand only the host bridge, the VGA and
    // the NVMe, while the network card lives on another bus — one ECAM branch per
    // device. As long as we do not parse the bridges, we honestly walk all the buses from the MCFG.
    // §4.8: on a c7i the ENA card stands on bus 0x27 (39) — not under the bridges of bus 0
    // (the tree ends there at 0x23) and not in the first MCFG record. So we walk
    // ALL the buses of the ECAM window rather than only those reachable by a walk over the bridges.
    let end = crate::acpi::ecam_bus_end();
    if ecam() != 0 && end > 0 {
        for b in 1..=end { scan_bus(b); }
    }
    if NBRIDGE.load(Ordering::Relaxed) > 0 {
        crate::println!("pci: bridges skipped {} (empty hot-plug slots), devices in the table {}",
            NBRIDGE.load(Ordering::Relaxed), NDEV.load(Ordering::Relaxed));
    }
    if NLOST.load(Ordering::Relaxed) > 0 {
        crate::println!("pci: ATTENTION: {} devices did not fit into the table ({} places) — the needed one may be among them",
            NLOST.load(Ordering::Relaxed), MAX_DEV);
    }
    // §4.8: on a c7i.large only the host bridge, the VGA, the NVMe and a hundred
    // EMPTY bridges are visible in the ECAM window — the network card answers only through the legacy ports. If there is no network in
    // the ECAM, we honestly re-enumerate through 0xCF8/0xCFC (as on the 6th generation,
    // where the MCFG is absent altogether). Did not find it there either — we go back to the ECAM.
    if ecam() != 0 && find_class(0x02_0000).is_none() {
        crate::println!("pci: there is no network device in the ECAM window — a repeated enumeration through the ports 0xCF8");
        let saved = ecam();
        ECAM.store(0, Ordering::Relaxed);
        PORTIO.store(true, Ordering::Relaxed);
        NDEV.store(0, Ordering::Relaxed); NBRIDGE.store(0, Ordering::Relaxed);
        unsafe { BUS_SEEN = [0; 4]; }
        scan_bus(0);
        if find_class(0x02_0000).is_none() {
            PORTIO.store(false, Ordering::Relaxed);
            ECAM.store(saved, Ordering::Relaxed);
            NDEV.store(0, Ordering::Relaxed); NBRIDGE.store(0, Ordering::Relaxed);
            unsafe { BUS_SEEN = [0; 4]; }
            scan_bus(0);
            if end > 0 { for b in 1..=end { scan_bus(b); } }
        }
    }
}

/// Switch MSI-X on at the device, having masked the WHOLE FUNCTION (the Function Mask):
/// the ENA rejects the creation of IO queues if MSI-X is not on at the function, but with
/// Function Mask=1 the interrupts are not delivered — the datapath stays polling-first
/// (the device writes the descriptors into memory independently of the interrupts).
/// It returns the size of the vector table (0 — there is no MSI-X).
pub fn msix_enable_masked(d: &PciDev) -> u16 {
    if d.msix_cap == 0 { return 0; }
    let dw = rd(d.bus, d.dev, d.func, d.msix_cap);
    let size = (((dw >> 16) & 0x7FF) + 1) as u16;
    // The vector table: the BIR (the lower 3 bits) + the offset in that BAR. We program
    // all the entries "for real" (the BSP's LAPIC, the vector MSI_VECTOR_BASE+i), but with the mask
    // bit set: Nitro rejects queues with an invalid vector, while the mask does not let
    // the interrupts be delivered — the datapath stays polling.
    let tbl = rd(d.bus, d.dev, d.func, d.msix_cap + 4);
    let bir = (tbl & 0x7) as u16;
    let off = (tbl & !0x7) as u64;
    let (bar_base, _) = bar(d.bus, d.dev, d.func, bir);
    if bar_base != 0 {
        for i in 0..size as u64 {
            let e = bar_base + off + i * 16;
            unsafe {
                core::ptr::write_volatile(e as *mut u32, 0xFEE0_0000);        // msg addr: LAPIC BSP
                core::ptr::write_volatile((e + 4) as *mut u32, 0);            // msg addr hi
                core::ptr::write_volatile((e + 8) as *mut u32, crate::idt::MSI_VECTOR_BASE as u32 + i as u32); // msg data: the vector
                // vector control: unmasked (in the IDT there stand the stubs
                // MSI_VECTOR_BASE.. with an EOI — the delivery is safe)
                core::ptr::write_volatile((e + 12) as *mut u32, 0);
            }
        }
    }
    // the bits 31/30 of this dword = MSI-X Enable / Function Mask in the Message Control:
    // Enable=1 (the device sees MSI-X), Function Mask=0 (the masks are per-vector above)
    wr(d.bus, d.dev, d.func, d.msix_cap, (dw | (1 << 31)) & !(1 << 30));
    size
}

/// Direct ALL the device's MSI-X vectors onto the logical core `cpu`.
///
/// The MSI-X message's address is `0xFEE00000 | (APIC id << 12)`: wherever the address is
/// written, that is where the interrupt will arrive. By default there are zeroes there, that is APIC id 0 —
/// the boot core; while the scheduler lived only on it, there was no question.
/// With dedicated cores it appeared: a device's interrupt that has arrived on the core of a
/// latency-critical thread is exactly the jitter for the absence of which the core
/// was dedicated, and moreover one that comes not from us and not on our schedule.
///
/// It returns the APIC id the vectors are directed to (for the log), or u32::MAX
/// if the device has no MSI-X or the table's BAR is not mapped.
pub fn msix_route(d: &PciDev, cpu: usize) -> u32 {
    if d.msix_cap == 0 { return u32::MAX; }
    let dw = rd(d.bus, d.dev, d.func, d.msix_cap);
    let size = (((dw >> 16) & 0x7FF) + 1) as u64;
    let tbl = rd(d.bus, d.dev, d.func, d.msix_cap + 4);
    let (bar_base, _) = bar(d.bus, d.dev, d.func, (tbl & 0x7) as u16);
    if bar_base == 0 { return u32::MAX; }
    let apic = crate::smp::apic_of(cpu);
    if apic > 254 { return u32::MAX; } // the xAPIC addresses 8-bit ids (see smp.rs)
    let off = (tbl & !0x7) as u64;
    for i in 0..size {
        let e = bar_base + off + i * 16;
        unsafe { core::ptr::write_volatile(e as *mut u32, 0xFEE0_0000 | (apic << 12)); }
    }
    apic
}

// ── MSI (capability 0x05) ───────────────────────────────────────────────────
// MSI-X appeared in PCIe 3.0 and is far from present everywhere: FPGA shells (Xilinx's XDMA),
// old 10G cards and custom functions declare only MSI all over the place. Until
// now the kernel parsed ONLY MSI-X (`msix_cap`), and such a card came up
// silently and just as silently delivered not a single interrupt — a failure which on a
// stand looks like "the datapath works, but one must not sleep on the ring".
//
// The essential difference from MSI-X: MSI has no vector table in a BAR — the address and the
// data lie in the configuration space itself, the vector is one per function
// (or a block of 2^k in a row), and masking is optional. So a BAR is not
// needed here at all, and MSI works even where a BAR has not yet been assigned.

/// The decomposition of the MSI capability: (a 64-bit address?, are there per-vector masks,
/// how many vectors the device has requested).
fn msi_layout(d: &PciDev) -> (bool, bool, u8) {
    let ctl = rd(d.bus, d.dev, d.func, d.msi_cap) >> 16;
    (ctl & (1 << 7) != 0, ctl & (1 << 8) != 0, ((ctl >> 1) & 0x7) as u8)
}

/// Switch MSI on and direct it onto the logical core `cpu`, the vector `vector`.
/// It returns the recipient's APIC id or `u32::MAX` (there is no MSI / the core is unavailable).
///
/// The number of vectors requested by the device we deliberately reduce to ONE (the field
/// Multiple Message Enable = 0): several MSI vectors are bound to go in a row and
/// aligned by vector, and there is nobody for us to hand out such blocks to yet. One is
/// enough for exactly what the interrupt is needed for — to wake a sleeping
/// core; the source is figured out by polling the rings.
pub fn msi_enable(d: &PciDev, cpu: usize, vector: u8) -> u32 {
    if d.msi_cap == 0 { return u32::MAX; }
    let apic = crate::smp::apic_of(cpu);
    if apic > 254 { return u32::MAX; }  // the xAPIC addresses 8-bit ids (see smp.rs)
    let (is64, maskable, _) = msi_layout(d);
    let addr = 0xFEE0_0000u32 | (apic << 12);
    wr(d.bus, d.dev, d.func, d.msi_cap + 4, addr);
    if is64 {
        wr(d.bus, d.dev, d.func, d.msi_cap + 8, 0);
        wr(d.bus, d.dev, d.func, d.msi_cap + 0x0C, vector as u32);
        if maskable { wr(d.bus, d.dev, d.func, d.msi_cap + 0x10, 0); } // the masks are cleared
    } else {
        wr(d.bus, d.dev, d.func, d.msi_cap + 8, vector as u32);
        if maskable { wr(d.bus, d.dev, d.func, d.msi_cap + 0x0C, 0); }
    }
    // Message Control: Enable = bit 0, Multiple Message Enable = bits 6..4 -> 0.
    let dw = rd(d.bus, d.dev, d.func, d.msi_cap);
    wr(d.bus, d.dev, d.func, d.msi_cap, (dw | (1 << 16)) & !(0x7 << 20));
    apic
}

/// Read back what has been written into the MSI: (the message's address, the data, is it on?).
/// The read-back is not over-insurance: at a function without an assigned BAR or behind a bridge
/// that does not forward it, a write into the config goes nowhere, while "we have configured it"
/// would look the same as "it works" (the same reason as with the msix gate in v1.9).
pub fn msi_readback(d: &PciDev) -> (u32, u32, bool) {
    if d.msi_cap == 0 { return (0, 0, false); }
    let (is64, _, _) = msi_layout(d);
    let addr = rd(d.bus, d.dev, d.func, d.msi_cap + 4);
    let data = rd(d.bus, d.dev, d.func, d.msi_cap + if is64 { 0x0C } else { 8 }) & 0xFFFF;
    let on = rd(d.bus, d.dev, d.func, d.msi_cap) & (1 << 16) != 0;
    (addr, data, on)
}

/// How the device is able to interrupt: "MSI-X", "MSI", "INTx" (the last one the kernel does
/// not wire up — a card with INTx alone will not deliver interrupts, and that has to be
/// said out loud rather than found out on a stand).
pub fn intr_kind(d: &PciDev) -> &'static str {
    if d.msix_cap != 0 { "MSI-X" } else if d.msi_cap != 0 { "MSI" } else { "INTx (not wired up)" }
}


/// The number of devices in the table and the i-th device (for a walk: the MSI-X self-test).
pub fn ndev() -> usize { NDEV.load(Ordering::Relaxed) }
pub fn dev_at(i: usize) -> Option<PciDev> {
    if i >= ndev() { return None; }
    Some(unsafe { DEVS[i] })
}

/// Read the message address of the i-th MSI-X vector (u32::MAX — there is no table).
/// The self-test needs it: "we have written the address" and "the device will see this address" are different
/// statements, and it is the second one that has to be checked, that is by reading back.
pub fn msix_msg_addr(d: &PciDev, idx: u16) -> u32 {
    if d.msix_cap == 0 { return u32::MAX; }
    let tbl = rd(d.bus, d.dev, d.func, d.msix_cap + 4);
    let (bar_base, _) = bar(d.bus, d.dev, d.func, (tbl & 0x7) as u16);
    if bar_base == 0 { return u32::MAX; }
    unsafe { core::ptr::read_volatile((bar_base + (tbl & !0x7) as u64 + (idx as u64) * 16) as *const u32) }
}

/// Find a device by vendor:device (the first one). None — there is none.
pub fn find(vendor: u16, device: u16) -> Option<PciDev> {
    for i in 0..NDEV.load(Ordering::Relaxed) {
        let d = unsafe { DEVS[i] };
        if d.vendor == vendor && d.device == device { return Some(d); }
    }
    None
}
/// Find one by class (class<<16|subclass<<8, the progif is ignored): NVMe = 0x010802.
pub fn find_class(class_prefix: u32) -> Option<PciDev> { find_class_nth(class_prefix, 0) }

/// The n-th device of the class (A9). On bare metal there may be several controllers of
/// one class — on a Nitro metal every EBS volume is a separate NVMe controller —
/// and "the first one that turns up" may turn out not to be the one we were booted from.
pub fn find_class_nth(class_prefix: u32, n: usize) -> Option<PciDev> {
    let mut k = 0usize;
    for i in 0..NDEV.load(Ordering::Relaxed) {
        let d = unsafe { DEVS[i] };
        if d.class >> 8 == class_prefix >> 8 {
            if k == n { return Some(d); }
            k += 1;
        }
    }
    None
}
/// How many devices of such a class are in the table.
pub fn count_class(class_prefix: u32) -> usize {
    (0..NDEV.load(Ordering::Relaxed))
        .filter(|&i| unsafe { DEVS[i] }.class >> 8 == class_prefix >> 8).count()
}
/// (base, size) of the ivshmem shared memory or (0,0).
pub fn ivshmem() -> (u64, u64) { (IVSHMEM_BASE.load(Ordering::SeqCst), IVSHMEM_SIZE.load(Ordering::SeqCst)) }
