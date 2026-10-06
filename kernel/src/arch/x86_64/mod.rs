//! x86-64: the machine as the kernel has always seen it. Nothing in this
//! directory is new — it is the `asm!`-holding code of the shared modules,
//! moved behind the `arch::` surface (`docs/ARM64-PLAN.md` §4.1).

pub mod abi;     // the Linux x86-64 syscall numbers ARE the internal ones: the identity decoder
#[macro_use]
pub mod serial;  // the 16550 UART on COM1 (port I/O), `print!`/`println!`
pub mod barrier; // dma_wmb: the ring's stores before the doorbell, as the DEVICE sees them
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
pub mod msi;     // what a device's MSI/MSI-X entry holds on this machine
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
    if i < VIRTIO_MMIO_SLOTS { Some((mmio_va(0xFEB0_0000 + i as u64 * 512) as usize, 512)) } else { None }
}
/// The kernel's pointer to a device register: the devices' window (`paging::mmio_va`), the
/// identity before it is built or with `talaria.mwin=0`.
#[inline] pub fn mmio_va(pa: u64) -> u64 { crate::paging::mmio_va(pa) }
#[inline] pub fn mmio_pa(va: u64) -> u64 { crate::paging::mmio_pa(va) }
/// The devices' window gate (`talaria.test=mwin`): the device pointers this machine keeps.
pub fn mwin_pointers() -> [(&'static str, u64); 2] {
    [("lapic", smp::LAPIC_VA.load(core::sync::atomic::Ordering::Relaxed)), ("ioapic", ioapic::base(0))]
}
/// A device's ID word read through the kernel's pointer (the IOAPIC's line count; 0: none).
pub fn mwin_witness() -> u32 { ioapic::lines_of(0) }
/// The GSI of the virtio-mmio window in slot `i`. The microvm puts the virtio lines at the
/// TOP of the GSI space: the highest occupied slot sits on the highest line and the rest are
/// counted down from it (measured — see `netsys::netirq_setup`). `None`: not derivable.
pub fn virtio_mmio_gsi(i: usize) -> Option<u32> {
    let top = ioapic::top_gsi();
    let smax = crate::virtnet::mmio_slot_max();
    if top == 0 || smax < i { None } else { Some(top - (smax - i) as u32) }
}

/// The ELF `e_machine` this kernel runs, the `AT_HWCAP` word it hands a process (cpuid.1:edx:
/// fpu..sse2) and the `AT_PLATFORM` string.
#[allow(dead_code)] // elfhdr keeps its own cfg const (it is #[path]-included into kernel-unit); this is the pair's x86 half
pub const ELF_MACHINE: u16 = 0x3E;
pub const AT_HWCAP: u64 = 0x0f8b_fbff;
pub const PLATFORM: [u8; 8] = *b"x86_64\0\0";

/// The vDSO symbol a libc looks for on this architecture; `vdso::init` refuses an image without it.
pub const VDSO_CLOCK_SYMBOL: &[u8] = b"__vdso_clock_gettime";

/// `talaria.test=its`: MSI-X through the interrupt translation service (arm64).
pub fn msi_selftest() -> ! {
    crate::println!("its: there is no ITS on x86 — MSI-X routing is the gate `talaria.test=msix`");
    crate::suite::park()
}
