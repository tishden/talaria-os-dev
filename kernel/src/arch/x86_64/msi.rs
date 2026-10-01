//! What a device's MSI/MSI-X entry holds on x86 — `arch::msi`. The message's address names
//! the core: `0xFEE0_0000 | APIC id << 12` (the LAPIC's window); the data is the IDT vector.
//! (arm64's twin sends every message to the ITS and routes by a command: `arch/aarch64/msi.rs`.)

use crate::pci::PciDev;

/// "The boot core, as the firmware would have left it": APIC id 0, whatever the logical
/// map says (`pci::msix_enable_masked` has always written the bare `0xFEE0_0000`).
pub const BOOT: usize = usize::MAX;

fn addr_of(cpu: usize) -> Option<u32> {
    if cpu == BOOT { return Some(0xFEE0_0000); }
    let apic = crate::smp::apic_of(cpu);
    if apic > 254 { None } else { Some(0xFEE0_0000 | (apic << 12)) } // the xAPIC addresses 8-bit ids (see smp.rs)
}
/// The entry for vector `index` of `d` delivered to the logical core `cpu` on the IDT
/// vector `vector`: (address low, address high, data).
pub fn compose(_d: &PciDev, _index: u16, _nvec: u16, cpu: usize, vector: u16) -> Option<(u32, u32, u32)> {
    addr_of(cpu).map(|a| (a, 0, vector as u32))
}
/// Move a vector to `cpu`: the new address of its entry (the data — the IDT vector — stays).
pub fn retarget(_d: &PciDev, _index: u16, _nvec: u16, cpu: usize) -> Option<(u32, u32)> {
    addr_of(cpu).map(|a| (a, 0))
}
/// The low address word a message routed to `cpu` must read back as (the gates compare).
pub fn expect_addr(cpu: usize) -> u32 { addr_of(cpu).unwrap_or(u32::MAX) }
/// Is a core with this id reachable by a message?
pub fn reachable(cpu: usize) -> bool { addr_of(cpu).is_some() }
/// The data word a message on the IDT vector `vector` carries: the vector.
pub fn expect_data(vector: u16) -> u32 { vector as u32 }
/// The message's address names the target core (the APIC id field) on this machine.
pub const ADDRESS_NAMES_CORE: bool = true;
