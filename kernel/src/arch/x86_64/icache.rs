//! The instruction cache on x86-64 is coherent with data writes: a store to code is
//! visible to fetch after the next jump (the SDM's self-modifying-code rule), so there is
//! nothing to do here. The name exists because on arm64 there is (`arch/aarch64/icache.rs`),
//! and the shared loaders call it after every write of code a process will execute.

/// Nothing to synchronise on this machine.
#[inline(always)]
pub fn sync(_addr: u64, _len: u64) {}
