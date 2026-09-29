//! x86-64: the machine as the kernel has always seen it. Nothing in this
//! directory is new — it is the `asm!`-holding code of the shared modules,
//! moved behind the `arch::` surface (`docs/ARM64-PLAN.md` §4.1).

pub mod boot;    // the PVH / multiboot2 entry and the 32-bit -> long mode switch
pub mod cat;     // Intel CAT: the L3 masks and the class per core (MSRs)
pub mod cpu;     // the privileged instructions, one wrapper each
pub mod crypto;  // AES-128-GCM on AES-NI + PCLMULQDQ
pub mod ioapic;  // the I/O APIC: a GSI -> a vector on the BSP
pub mod percpu;  // the per-CPU block through GS.base
pub mod pmu;     // the performance counters through MSRs + rdpmc
pub mod vga;     // the legacy text window at 0xB8000
