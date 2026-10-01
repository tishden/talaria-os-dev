//! The Talaria UEFI stub: a PE application without dependencies that carries the kernel
//! inside (`include_bytes!`), asks the firmware for what the kernel cannot ask after
//! `ExitBootServices` (the command line, the RSDP, the memory map), and hands control to
//! the kernel's own entry. One crate, two machines:
//!   * `x86_64.rs` (x86_64-unknown-uefi): drops from long mode to the 32-bit PVH entry;
//!   * `aarch64.rs` (aarch64-unknown-uefi): turns the MMU off and enters the arm64 Image
//!     with the stub's boot block in `x0` (docs/ARM64-PLAN.md §4.2).
//! `efi.rs` is the firmware's interface both use.
#![no_std]
#![no_main]

mod efi;
#[cfg(target_arch = "x86_64")]
mod x86_64;
#[cfg(target_arch = "aarch64")]
mod aarch64;
