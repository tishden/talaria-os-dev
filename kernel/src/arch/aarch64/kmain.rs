//! Bringing the arm64 machine up — phase 1 of docs/ARM64-PLAN.md. What exists so far:
//! the UART, the exception vectors, the early identity map. The banner prints the
//! facts the rest of the port depends on (the exception level, the DTB's address and
//! size, the core's id registers, the counter's frequency) and then stops: nothing
//! above `arch::` is compiled for this target yet.

use super::{cpu, mmu, trap, uart};

/// The magic at the start of a flattened device tree, big-endian.
const FDT_MAGIC: u32 = 0xD00D_FEED;

#[unsafe(no_mangle)]
extern "C" fn rust_main_aarch64(dtb: u64) -> ! {
    uart::init();
    crate::println!("talaria: boot (aarch64), EL{}, dtb={:#x}", cpu::current_el(), dtb);
    trap::init();
    crate::println!("talaria: the exception vectors are installed (a diagnosis instead of a silent reset)");
    // QEMU `virt`: RAM at 1 GiB; the size is a guess until the DTB is parsed — mapping
    // memory that is not there costs nothing while nothing touches it.
    unsafe { mmu::early_enable(0x4000_0000, 4 << 30); }
    crate::println!("talaria: the MMU is on: sctlr={:#x} ttbr0={:#x} (1 GiB blocks: device | normal x4)",
        cpu::sctlr_el1(), cpu::ttbr0_el1());
    if dtb != 0 {
        let magic = u32::from_be(unsafe { core::ptr::read_volatile(dtb as *const u32) });
        let total = u32::from_be(unsafe { core::ptr::read_volatile((dtb + 4) as *const u32) });
        if magic == FDT_MAGIC {
            crate::println!("talaria: the device tree: {} bytes at {:#x}", total, dtb);
        } else {
            crate::println!("talaria: x0={:#x} is NOT a device tree (magic {:#x})", dtb, magic);
        }
    }
    let midr = cpu::midr();
    crate::println!("talaria: cpu: implementer {:#x} part {:#x} r{}p{}, mpidr={:#x}",
        (midr >> 24) & 0xFF, (midr >> 4) & 0xFFF, (midr >> 20) & 0xF, midr & 0xF, cpu::mpidr());
    let hz = cpu::counter_hz();
    crate::println!("talaria: the counter: {} Hz ({} ns per tick), now {}", hz, if hz > 0 { 1_000_000_000 / hz } else { 0 }, cpu::counter());
    crate::println!("talaria: arm64 phase 1 stops here — the kernel above arch/ is not compiled for this target yet (docs/ARM64-PLAN.md §5)");
    cpu::halt_forever()
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    crate::println!("PANIC: {}", info);
    cpu::halt_irq_off_forever()
}
