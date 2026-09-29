//! x86-64: the machine as the kernel has always seen it. Nothing in this
//! directory is new — it is the `asm!`-holding code of the shared modules,
//! moved behind the `arch::` surface (`docs/ARM64-PLAN.md` §4.1).

#[macro_use]
pub mod serial;  // the 16550 UART on COM1 (port I/O), `print!`/`println!`
pub mod boot;    // the PVH / multiboot2 entry and the 32-bit -> long mode switch
pub mod gdb;     // the gdb stub over COM2: the x86 register file, DR0/DR7 watchpoints
pub mod gdt;     // the GDT, the per-core TSS, the iretq into ring 3
pub mod idt;     // the IDT, the exception handlers, the #PF fast paths, the IPI vectors
pub mod smp;     // the LAPIC (timer, IPIs, x2APIC, PV EOI), the INIT-SIPI-SIPI bring-up, the topology
pub mod tsc;     // the TSC as the clock: the calibration (CPUID/PIT/kvmclock) and the wall-clock servo
pub mod unwind;  // the guarded load a profiler makes on a foreign stack
pub mod cat;     // Intel CAT: the L3 masks and the class per core (MSRs)
pub mod cpu;     // the privileged instructions, one wrapper each
pub mod crypto;  // AES-128-GCM on AES-NI + PCLMULQDQ
pub mod ioapic;  // the I/O APIC: a GSI -> a vector on the BSP
pub mod mmu;     // the page-table descriptor as the MMU reads it, CR3, the TLB, the PAT
pub mod percpu;  // the per-CPU block through GS.base
pub mod pmu;     // the performance counters through MSRs + rdpmc
pub mod setjmp;  // tal_setjmp/tal_longjmp: the recovery of a crashed workload
pub mod switch;  // the FPU state, the saved-context entry, the tick's stub, the soft-yield kick
pub mod syscall_entry; // the SYSCALL MSRs, the entry stub, the sigreturn iretq
pub mod vga;     // the legacy text window at 0xB8000

/// The vDSO symbol a libc looks for on this architecture; `vdso::init` refuses an image without it.
pub const VDSO_CLOCK_SYMBOL: &[u8] = b"__vdso_clock_gettime";
