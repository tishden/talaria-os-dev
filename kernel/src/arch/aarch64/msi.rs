//! What a device's MSI/MSI-X entry holds on arm64 — `arch::msi` (docs/ARM64-PLAN.md §4.8).
//! The message's address is the ITS's `GITS_TRANSLATER`, its data the EVENT number (the
//! index of the vector at the device); which core takes it is the ITS's business, set by
//! a command, so a re-route does not touch the entry at all.

use crate::pci::PciDev;

/// "The boot core, as the firmware would have left it" (`pci::msix_enable_masked`).
pub const BOOT: usize = usize::MAX;
fn devid(d: &PciDev) -> u32 { (d.bus as u32) << 8 | (d.dev as u32) << 3 | d.func as u32 }

/// The entry for vector `index` (of `nvec`) of `d`, delivered to the logical core `cpu` on
/// the handler key `vector`: (address low, address high, data). None — no ITS, or it refused.
pub fn compose(d: &PciDev, index: u16, nvec: u16, cpu: usize, vector: u16) -> Option<(u32, u32, u32)> {
    let cpu = if cpu == BOOT { 0 } else { cpu };
    let (addr, data) = super::its::msi(devid(d), index as u32, nvec as u32, cpu, Some(vector))?;
    Some((addr as u32, (addr >> 32) as u32, data))
}
/// Move vector `index` of `d` to `cpu`, its handler unchanged: the address the entry holds
/// afterwards (unchanged here — the ITS moved the event to the core's collection).
pub fn retarget(d: &PciDev, index: u16, nvec: u16, cpu: usize) -> Option<(u32, u32)> {
    let (addr, _) = super::its::msi(devid(d), index as u32, nvec as u32, cpu, None)?;
    Some((addr as u32, (addr >> 32) as u32))
}
/// The low address word a message routed to `cpu` must read back as (the gates compare).
pub fn expect_addr(_cpu: usize) -> u32 { super::its::translater() as u32 }
/// Is a core with this id reachable by a message? (x86: the xAPIC's 8-bit ids.)
pub fn reachable(_cpu: usize) -> bool { super::its::present() }
/// The data word a single MSI carries: the event number, 0 (the handler key is the ITS's).
pub fn expect_data(_vector: u16) -> u32 { 0 }
/// The message's address is the same for every core: the ITS routes.
pub const ADDRESS_NAMES_CORE: bool = false;
