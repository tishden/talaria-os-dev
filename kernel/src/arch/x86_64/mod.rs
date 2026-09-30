//! x86-64: the machine as the kernel has always seen it. Nothing in this
//! directory is new — it is the `asm!`-holding code of the shared modules,
//! moved behind the `arch::` surface (`docs/ARM64-PLAN.md` §4.1).

pub mod abi;     // the Linux x86-64 syscall numbers ARE the internal ones: the identity decoder
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
pub mod icache;  // nothing: the instruction cache is coherent with stores on x86-64
pub mod ioapic;  // the I/O APIC: a GSI -> a vector on the BSP
pub mod mmu;     // the page-table descriptor as the MMU reads it, CR3, the TLB, the PAT
pub mod percpu;  // the per-CPU block through GS.base
pub mod pmu;     // the performance counters through MSRs + rdpmc
pub mod setjmp;  // tal_setjmp/tal_longjmp: the recovery of a crashed workload
pub mod signal;  // the ucontext/gregs layout, the fork and dump registers, the sigreturn restore
pub mod switch;  // the FPU state, the saved-context entry, the tick's stub, the soft-yield kick
pub mod syscall_entry; // the SYSCALL MSRs, the entry stub, the sigreturn iretq
pub mod vga;     // the legacy text window at 0xB8000

/// The virtio-mmio slots a driver may probe: QEMU microvm's fixed grid — 32 windows of 512
/// bytes from 0xFEB0_0000 (`virtnet::scan_mmio_window`).
pub const VIRTIO_MMIO_SLOTS: usize = 32;
pub fn virtio_mmio_slot(i: usize) -> Option<(usize, usize)> {
    if i < VIRTIO_MMIO_SLOTS { Some((0xFEB0_0000 + i * 512, 512)) } else { None }
}

/// The ELF `e_machine` this kernel runs, the `AT_HWCAP` word it hands a process (cpuid.1:edx:
/// fpu..sse2) and the `AT_PLATFORM` string.
#[allow(dead_code)] // elfhdr keeps its own cfg const (it is #[path]-included into kernel-unit); this is the pair's x86 half
pub const ELF_MACHINE: u16 = 0x3E;
pub const AT_HWCAP: u64 = 0x0f8b_fbff;
pub const PLATFORM: [u8; 8] = *b"x86_64\0\0";

/// The vDSO symbol a libc looks for on this architecture; `vdso::init` refuses an image without it.
pub const VDSO_CLOCK_SYMBOL: &[u8] = b"__vdso_clock_gettime";
