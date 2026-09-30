//! The I/O APIC's job — a device line to a core — is the GIC distributor's on arm64
//! (`gic::enable_spi`). The x86 names: a "GSI" is an SPI number, the "APIC id" the
//! core's affinity as `smp::apic_of` packs it. There is one "I/O APIC": the distributor.
pub fn note(_addr: u64, _gsi_base: u32) {}
pub fn available() -> bool { super::gic::present() }
pub fn count() -> usize { if super::gic::present() { 1 } else { 0 } }
/// Route SPI `gsi` (an INTID = 32 + gsi) to the core `apic`; the vector is the INTID
/// itself on this machine, so a caller's vector is recorded only as the handler's key.
pub fn route(gsi: u32, _vector: u8, apic: u32) -> bool {
    if !super::gic::present() { return false; }
    let mpidr = (apic as u64 & 0xFF_FFFF) | ((apic as u64 >> 24) << 32);
    unsafe { super::gic::enable_spi(32 + gsi, mpidr); }
    true
}
pub fn top_gsi() -> u32 { 32 + 256 }
pub fn dump() { crate::println!("ioapic: the GIC distributor plays the I/O APIC"); }
