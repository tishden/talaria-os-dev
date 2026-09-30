//! Cache allocation: Intel CAT has no arm64 twin in this kernel (Arm's MPAM is a later
//! question). The x86 module's surface, with `present() == false` and every rule refused.
pub fn init() {}
pub fn present() -> bool { false }
pub fn mask_valid(_m: u32, _cbm_len: u32) -> bool { false }
pub fn apply(_rules: &[(u64, u32)]) -> u32 { 0 }
pub fn apply_local_if_changed(_cpu: usize) {}
pub fn share_of_cpu(_cpu: usize) -> u64 { 0 }
pub fn dump() { crate::println!("cat: no cache allocation on this machine"); }
pub fn report_footprint(_what: &str, _queues: u64, _ring_bufs: u64, _buf: u64, _desc_bytes: u64) -> bool { false }
pub fn selftest() -> bool { crate::println!("cat-selftest: no CAT on arm64 — nothing to test"); true }
