//! The I/O APIC's job — a device line to a core — is the GIC distributor's on arm64
//! (`gic::enable_spi`). The x86 names: a "GSI" is an SPI number, the "APIC id" the
//! core's affinity as `smp::apic_of` packs it. There is one "I/O APIC": the distributor.
pub fn note(_addr: u64, _gsi_base: u32) {}
pub fn available() -> bool { super::gic::present() }
pub fn count() -> usize { if super::gic::present() { 1 } else { 0 } }
/// Route SPI `gsi` (an INTID = 32 + gsi) to the core `apic`; the vector is the INTID
/// itself on this machine, so a caller's vector is recorded only as the handler's key.
/// The handler the caller keeps under `vector` is therefore installed under the INTID too
/// (the card's line: `NIC_VECTOR` -> `netsys::nic_irq`) BEFORE the line is enabled: an SPI
/// nobody handles is only counted and ended, and a level line would then fire forever.
pub fn route(gsi: u32, vector: u8, apic: u32) -> bool {
    if !super::gic::present() { return false; }
    let h = if vector as usize == super::idt::NIC_VECTOR { super::idt::nic_handler() }
        else { super::idt::handler_of(vector as u32).map(|f| f as usize as u64).unwrap_or(0) };
    if h == 0 { return false; }
    super::idt::set_vector(32 + gsi as usize, h);
    let mpidr = (apic as u64 & 0xFF_FFFF) | ((apic as u64 >> 24) << 32);
    unsafe { super::gic::enable_spi(32 + gsi, mpidr); }
    true
}
pub fn top_gsi() -> u32 { 32 + 256 }
pub fn dump() { crate::println!("ioapic: the GIC distributor plays the I/O APIC"); }
