//! No legacy text window on arm64: the x86 module's surface, every question answered "no".
pub fn enable() {}
pub fn disable() {}
pub fn enabled() -> bool { false }
pub fn putb(_b: u8) {}
pub fn row_contains(_row: usize, _pat: &[u8]) -> bool { false }
pub fn contains(_pat: &[u8]) -> bool { false }
