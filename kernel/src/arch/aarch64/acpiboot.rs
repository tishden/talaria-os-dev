//! The machine's description from ACPI — what `dtb.rs` reads from a device tree, read from
//! the tables a UEFI firmware leaves (docs/ARM64-PLAN.md §4.2: EC2 and `edk2`, the product
//! path). An arm64 server has no device tree to give: the interrupt controller, the cores,
//! the timer's interrupts, the console and the way a core is started are in the MADT, the
//! GTDT, the SPCR and the FADT. The result is the same `dtb::Summary` the tree gives, so
//! everything below `kmain` is one path.
//!
//! What each table gives:
//! * `APIC` (the MADT): a GICC entry per core (type 0x0B: the flags and the MPIDR), the
//!   distributor (0x0C), the redistributor region (0x0E; a machine without one gives each
//!   core's frame in its GICC entry, and the region is their span), the ITS (0x0F);
//! * `GTDT`: the generic timer's interrupts, as INTIDs;
//! * `SPCR`: the console's type and address — a PL011, or the 16550 a Nitro guest names. A
//!   revision-2 table may name a PCI function instead (EC2's does: `1d0f:8250` at 00:01.0,
//!   with a placeholder address that is the ENA's BAR); then the console is that function's
//!   BAR 0, read through the MCFG's window, its memory decoding switched on here — the
//!   firmware leaves it off;
//! * `FACP` (the FADT): the Arm boot flags — PSCI, and through `hvc` or `smc`.
//! The ECAM window (the MCFG) is read here only to find a PCI console; the PCI layer, the
//! power-off and the NUMA tables stay with the shared `acpi.rs`, which walks the same RSDP
//! later.
//!
//! The parsers take the table's BYTES, so a host test feeds them a table built in the test;
//! `summary` is the one function that turns physical addresses into slices.

use super::dtb::{Psci, Range, Summary, UartKind, MAX_CPU};

fn le32(b: &[u8], o: usize) -> Option<u32> { Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?)) }
fn le64(b: &[u8], o: usize) -> Option<u64> { Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?)) }

/// What the walk found besides the summary (for the boot line).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    pub madt: bool, pub gtdt: bool, pub spcr: bool, pub fadt: bool,
    /// The SPCR's interface type when it is not a PL011 (0x03), an SBSA UART (0x0E) or a
    /// 16550 (0x00, 0x01, 0x12) — a console the kernel has no driver for.
    pub uart_other: Option<u8>,
    /// The SPCR names a PCI function (bus, device, function) rather than an address, and
    /// whether its BAR 0 became the console (false: no MCFG, no such function, no memory BAR —
    /// then there is no console at all: the table's address is a placeholder).
    pub uart_pci: Option<(u8, u8, u8)>,
    pub uart_pci_ok: bool,
}

/// The MADT: the cores, the distributor, the redistributors, the ITS.
pub fn madt(t: &[u8], out: &mut Summary) {
    let len = (le32(t, 4).unwrap_or(0) as usize).min(t.len());
    let mut p = 44;
    // The redistributor frames named core by core (no GICR entry): their span.
    let (mut lo, mut hi) = (u64::MAX, 0u64);
    while p + 2 <= len {
        let (typ, el) = (t[p], t[p + 1] as usize);
        if el < 2 || p + el > len { break; }
        let e = &t[p..p + el];
        match typ {
            // GICC: the flags at 12 (bit 0 — enabled, bit 3 — online capable), the
            // redistributor's frame at 60, the MPIDR at 68.
            0x0B if el >= 76 => {
                let flags = le32(e, 12).unwrap_or(0);
                if flags & 0b1001 != 0 {
                    out.cpu_seen += 1;
                    if out.ncpu < MAX_CPU { out.cpus[out.ncpu] = le64(e, 68).unwrap_or(0) & 0xFF_00FF_FFFF; out.ncpu += 1; }
                    let r = le64(e, 60).unwrap_or(0);
                    if r != 0 { lo = lo.min(r); hi = hi.max(r + 0x2_0000); }
                }
            }
            // GICD: the base at 8.
            0x0C if el >= 24 => out.gicd = le64(e, 8).map(|base| Range { base, size: 0x1_0000 }),
            // GICR: a discovery range — the base at 4, the length at 12.
            0x0E if el >= 16 && out.gicr.is_none() => {
                if let (Some(base), Some(size)) = (le64(e, 4), le32(e, 12)) { out.gicr = Some(Range { base, size: size as u64 }); }
            }
            // The ITS: the base at 8.
            0x0F if el >= 20 && out.its.is_none() => out.its = le64(e, 8).map(|base| Range { base, size: 0x2_0000 }),
            _ => {}
        }
        p += el;
    }
    if out.gicr.is_none() && lo != u64::MAX { out.gicr = Some(Range { base: lo, size: hi - lo }); }
}

/// The GTDT: the timer's interrupts as INTIDs, in the order `Summary::timer_ppi` keeps
/// (the secure physical, the non-secure physical, the virtual, the hypervisor).
pub fn gtdt(t: &[u8], out: &mut Summary) {
    for (i, off) in [48usize, 56, 64, 72].into_iter().enumerate() {
        if let Some(v) = le32(t, off) { out.timer_ppi[i] = v; }
    }
}

/// The SPCR: the console. The type at 36, the address in the generic address structure at
/// 40 (the access size at 43, the address itself at 44). Returns the type when the kernel
/// has no driver for it. A 16550's register stride follows the access size (1 byte, or a
/// 32-bit word — the `mmio32` of a Nitro guest); 0 ("undefined") means bytes.
pub fn spcr(t: &[u8], out: &mut Summary) -> Option<u8> {
    let typ = *t.get(36)?;
    let base = le64(t, 44)?;
    if base == 0 { return None; }
    match typ {
        0x03 | 0x0E => { out.uart = Some(Range { base, size: 0x1000 }); out.uart_kind = UartKind::Pl011; None }
        0x00 | 0x01 | 0x12 => {
            let (shift, width) = match t.get(43).copied().unwrap_or(0) { 2 => (1, 2), 3 => (2, 4), _ => (0, 1) };
            out.uart = Some(Range { base, size: 0x1000 });
            out.uart_kind = UartKind::Ns16550 { shift, width };
            None
        }
        other => Some(other),
    }
}

/// A revision-2 SPCR's PCI function: (bus, device, function), when the vendor field names
/// one (0xFFFF — or 0 in a table that never set it — means "not a PCI device").
pub fn spcr_pci(t: &[u8]) -> Option<(u8, u8, u8)> {
    if *t.get(8)? < 2 { return None; }
    let vendor = u16::from_le_bytes([*t.get(66)?, *t.get(67)?]);
    if vendor == 0xFFFF || vendor == 0 { return None; }
    Some((*t.get(68)?, *t.get(69)?, *t.get(70)?))
}

/// The MCFG's window for segment 0: its base (the address of bus 0, whatever the first bus).
pub fn mcfg_seg0(t: &[u8]) -> Option<u64> {
    let len = (le32(t, 4)? as usize).min(t.len());
    let mut p = 44;
    while p + 16 <= len {
        if u16::from_le_bytes([t[p + 8], t[p + 9]]) == 0 { return le64(t, p); }
        p += 16;
    }
    None
}

/// The console behind a PCI function: its BAR 0 (a memory BAR, 32- or 64-bit), with memory
/// decoding switched on. Config space through the ECAM window, aligned words only (the
/// MMU may be off).
/// # Safety
/// `ecam` is the MCFG's segment-0 window and identity-accessible.
unsafe fn pci_uart_bar(ecam: u64, (bus, dev, func): (u8, u8, u8)) -> Option<u64> {
    let cfg = ecam + ((bus as u64) << 20 | (dev as u64 & 0x1F) << 15 | (func as u64 & 7) << 12);
    let rd = |o: u64| core::ptr::read_volatile((cfg + o) as *const u32);
    if rd(0) & 0xFFFF == 0xFFFF { return None; }
    let b0 = rd(0x10);
    if b0 & 1 != 0 { return None; }                       // an I/O BAR: no ports on arm64
    let mut base = (b0 & !0xF) as u64;
    if (b0 >> 1) & 3 == 2 { base |= (rd(0x14) as u64) << 32; }
    if base == 0 { return None; }
    // Command: memory space on (bit 1); the status half is written as zeros, which its
    // write-one-to-clear bits ignore.
    core::ptr::write_volatile((cfg + 4) as *mut u32, (rd(4) & 0xFFFF) | 2);
    Some(base)
}

/// The FADT: the Arm boot architecture flags at 129 — bit 0 PSCI, bit 1 "use hvc".
pub fn fadt(t: &[u8], out: &mut Summary) {
    let Some(f) = t.get(129..131) else { return; };
    let f = u16::from_le_bytes([f[0], f[1]]);
    if f & 1 != 0 { out.psci = Some(if f & 2 != 0 { Psci::Hvc } else { Psci::Smc }); }
}

/// Walk the tables from the RSDP at the physical address `rsdp` and fill a summary. The RAM
/// ranges are NOT here (the firmware's memory map is the stub's to pass); the caller adds them.
/// # Safety
/// `rsdp` points at the firmware's RSDP and the tables it names are readable (identity).
pub unsafe fn summary(rsdp: u64) -> Option<(Summary, Found)> {
    let at = |a: u64, n: usize| core::slice::from_raw_parts(a as *const u8, n);
    if rsdp == 0 || at(rsdp, 8) != b"RSD PTR " { return None; }
    let r = at(rsdp, 36);
    let (sdt, wide) = match (r[15] >= 2, le64(r, 24)) {
        (true, Some(x)) if x != 0 => (x, true),
        _ => (le32(r, 16)? as u64, false),
    };
    if sdt == 0 { return None; }
    let len = le32(at(sdt, 8), 4)? as usize;
    if !(36..=1 << 20).contains(&len) { return None; }
    let x = at(sdt, len);
    let esz = if wide { 8 } else { 4 };
    let mut out = Summary::default();
    let mut found = Found::default();
    let mut ecam = None;
    let mut p = 36;
    while p + esz <= len {
        let t = if wide { le64(x, p)? } else { le32(x, p)? as u64 };
        p += esz;
        if t == 0 { continue; }
        let tl = le32(at(t, 8), 4)? as usize;
        if !(36..=16 << 20).contains(&tl) { continue; }
        let tb = at(t, tl);
        match &tb[..4] {
            b"APIC" => { madt(tb, &mut out); found.madt = true; }
            b"GTDT" => { gtdt(tb, &mut out); found.gtdt = true; }
            b"SPCR" => { found.uart_other = spcr(tb, &mut out); found.uart_pci = spcr_pci(tb); found.spcr = true; }
            b"MCFG" => ecam = mcfg_seg0(tb),
            b"FACP" => { fadt(tb, &mut out); found.fadt = true; }
            _ => {}
        }
    }
    if let Some(bdf) = found.uart_pci {
        let bar = ecam.and_then(|e| pci_uart_bar(e, bdf));
        found.uart_pci_ok = bar.is_some();
        if found.uart_other.is_none() { out.uart = bar.map(|base| Range { base, size: 0x1000 }); }
    }
    Some((out, found))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(sig: &[u8; 4], body: &[u8]) -> alloc_free::Table {
        let mut t = alloc_free::Table::new();
        t.push(sig); t.push(&((36 + body.len()) as u32).to_le_bytes()); t.push(&[0u8; 28]); t.push(body);
        t
    }
    /// A fixed-size byte buffer: the test crate is `no_std`-shaped (no `Vec` here).
    mod alloc_free {
        pub struct Table { pub b: [u8; 1024], pub n: usize }
        impl Table {
            pub fn new() -> Self { Table { b: [0; 1024], n: 0 } }
            pub fn push(&mut self, s: &[u8]) { self.b[self.n..self.n + s.len()].copy_from_slice(s); self.n += s.len(); }
            pub fn bytes(&self) -> &[u8] { &self.b[..self.n] }
        }
    }
    fn gicc(mpidr: u64, flags: u32, gicr: u64) -> [u8; 80] {
        let mut e = [0u8; 80];
        e[0] = 0x0B; e[1] = 80;
        e[12..16].copy_from_slice(&flags.to_le_bytes());
        e[60..68].copy_from_slice(&gicr.to_le_bytes());
        e[68..76].copy_from_slice(&mpidr.to_le_bytes());
        e
    }

    #[test]
    fn madt_of_a_virt_machine() {
        // The MADT body: the local controller address (4) and the flags (4), then the entries.
        let mut body = alloc_free::Table::new();
        body.push(&[0u8; 8]);
        body.push(&gicc(0, 1, 0));
        body.push(&gicc(1, 1, 0));
        body.push(&gicc(2, 0, 0));                 // a disabled core is not a core
        body.push(&gicc(3, 8, 0));                 // an online-capable one is
        let mut gicd = [0u8; 24]; gicd[0] = 0x0C; gicd[1] = 24; gicd[8..16].copy_from_slice(&0x0800_0000u64.to_le_bytes());
        body.push(&gicd);
        let mut gicr = [0u8; 16]; gicr[0] = 0x0E; gicr[1] = 16;
        gicr[4..12].copy_from_slice(&0x080A_0000u64.to_le_bytes()); gicr[12..16].copy_from_slice(&0x00F6_0000u32.to_le_bytes());
        body.push(&gicr);
        let mut its = [0u8; 20]; its[0] = 0x0F; its[1] = 20; its[8..16].copy_from_slice(&0x0808_0000u64.to_le_bytes());
        body.push(&its);
        let t = header(b"APIC", body.bytes());
        let mut s = Summary::default();
        madt(t.bytes(), &mut s);
        assert_eq!(s.ncpu, 3);
        assert_eq!(&s.cpus[..3], &[0, 1, 3]);
        assert_eq!(s.gicd, Some(Range { base: 0x0800_0000, size: 0x1_0000 }));
        assert_eq!(s.gicr, Some(Range { base: 0x080A_0000, size: 0x00F6_0000 }));
        assert_eq!(s.its, Some(Range { base: 0x0808_0000, size: 0x2_0000 }));
    }

    #[test]
    fn madt_without_a_gicr_entry_spans_the_cores_frames() {
        let mut body = alloc_free::Table::new();
        body.push(&[0u8; 8]);
        body.push(&gicc(0x100, 1, 0x1040_0000));
        body.push(&gicc(0x000, 1, 0x1042_0000));
        let t = header(b"APIC", body.bytes());
        let mut s = Summary::default();
        madt(t.bytes(), &mut s);
        assert_eq!(s.ncpu, 2);
        assert_eq!(s.gicr, Some(Range { base: 0x1040_0000, size: 0x4_0000 }));
    }

    #[test]
    fn a_truncated_madt_entry_stops_the_walk() {
        let mut body = alloc_free::Table::new();
        body.push(&[0u8; 8]);
        body.push(&gicc(0, 1, 0));
        body.push(&[0x0B, 80, 0, 0]);             // claims 80 bytes, has 4
        let t = header(b"APIC", body.bytes());
        let mut s = Summary::default();
        madt(t.bytes(), &mut s);
        assert_eq!(s.ncpu, 1);
    }

    #[test]
    fn gtdt_spcr_and_fadt() {
        let mut b = [0u8; 60];
        for (off, v) in [(12usize, 29u32), (20, 30), (28, 27), (36, 26)] { b[off..off + 4].copy_from_slice(&v.to_le_bytes()); }
        let t = header(b"GTDT", &b);
        let mut s = Summary::default();
        gtdt(t.bytes(), &mut s);
        assert_eq!(s.timer_ppi, [29, 30, 27, 26]);

        let mut b = [0u8; 44];
        b[0] = 0x03; b[8..16].copy_from_slice(&0x0900_0000u64.to_le_bytes());
        let t = header(b"SPCR", &b);
        assert_eq!(spcr(t.bytes(), &mut s), None);
        assert_eq!(s.uart, Some(Range { base: 0x0900_0000, size: 0x1000 }));
        b[0] = 0x12;                                // a 16550 described by its address structure
        b[7] = 3;                                   // ... accessed as 32-bit words (mmio32)
        let t = header(b"SPCR", &b);
        let mut s2 = Summary::default();
        assert_eq!(spcr(t.bytes(), &mut s2), None);
        assert_eq!(s2.uart, Some(Range { base: 0x0900_0000, size: 0x1000 }));
        assert_eq!(s2.uart_kind, UartKind::Ns16550 { shift: 2, width: 4 });
        b[0] = 0x00; b[7] = 0;                      // a plain 16550, byte registers
        let t = header(b"SPCR", &b);
        assert_eq!(spcr(t.bytes(), &mut s2), None);
        assert_eq!(s2.uart_kind, UartKind::Ns16550 { shift: 0, width: 1 });
        assert_eq!(spcr_pci(t.bytes()), None);      // revision 0: no PCI fields
        let mut b2 = [0u8; 44];                     // EC2's: a 16550 at PCI 00:01.0, 1d0f:8250
        b2[28..32].copy_from_slice(&[0x50, 0x82, 0x0f, 0x1d]); b2[33] = 1; // device, vendor; bus 0, device 1, function 0
        let mut t2 = header(b"SPCR", &b2);
        t2.b[8] = 2;
        assert_eq!(spcr_pci(t2.bytes()), Some((0, 1, 0)));
        let mut b3 = [0u8; 16 + 8];                 // MCFG: reserved 8, then one entry for segment 0
        b3[8..16].copy_from_slice(&0x2000_0000u64.to_le_bytes()); b3[19] = 0xFF;
        assert_eq!(mcfg_seg0(header(b"MCFG", &b3).bytes()), Some(0x2000_0000));
        b[0] = 0x08;                                // a type nobody drives
        let t = header(b"SPCR", &b);
        assert_eq!(spcr(t.bytes(), &mut s2), Some(0x08));

        let mut b = [0u8; 120];
        b[129 - 36] = 0b11;
        let t = header(b"FACP", &b);
        fadt(t.bytes(), &mut s);
        assert_eq!(s.psci, Some(Psci::Hvc));
        b[129 - 36] = 0b01;
        let t = header(b"FACP", &b);
        fadt(t.bytes(), &mut s);
        assert_eq!(s.psci, Some(Psci::Smc));
    }
}
