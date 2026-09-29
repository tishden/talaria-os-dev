//! The arm64 entry. The image starts with the 64-byte Linux `Image` header, so that
//! `qemu-system-aarch64 -kernel` (and any loader that knows a Linux kernel) puts it at
//! RAM base + `text_offset` and enters `_start` at EL1 (or EL2) with `x0` = the physical
//! address of the device tree, the MMU off, interrupts masked. The contract is the
//! kernel's `Documentation/arch/arm64/booting.rst`; what we rely on is only the load
//! address, `x0` and the exception level.
//!
//! In order: if entered at EL2, configure EL1 as AArch64 and drop into it; mask DAIF;
//! zero `.bss` (A9: no loader fills NOBITS for us); enable FP/SIMD for EL1 (the kernel
//! is built hard-float and Rust will use `V` registers in its first function); set the
//! stack; call `rust_main_aarch64(x0)`. The MMU stays OFF here — with `+strict-align`
//! in the target the Rust code that follows makes no unaligned access, so it may run
//! on Device-typed memory until `mmu::early_enable` turns the map on.

core::arch::global_asm!(r#"
.section .text.boot, "ax"
.global _head
_head:
    b       _start                      /* code0: the branch to the entry */
    .long   0                           /* code1 */
    .quad   0x80000                     /* text_offset: the image goes at RAM base + 512 KiB */
    .quad   __kernel_size               /* image_size: .bss included, the loader keeps this much free */
    .quad   0xA                         /* flags: little-endian, 4K pages, physical placement anywhere */
    .quad   0, 0, 0                     /* res2..res4 */
    .ascii  "ARM\x64"                   /* magic */
    .long   0                           /* res5 (a PE header offset — none) */

.global _start
_start:
    /* the device tree's address travels in x0 through everything below */
    mov     x19, x0
    msr     daifset, #0xf
    /* EL2 -> EL1 when a firmware left us high: EL1 is AArch64 (HCR.RW), the counter
       and the timer are visible at EL1 (CNTHCTL), no traps on FP/SIMD (CPTR). */
    mrs     x9, CurrentEL
    lsr     x9, x9, #2
    cmp     x9, #2
    b.ne    1f
    mov     x9, #(1 << 31)
    msr     hcr_el2, x9
    mov     x9, #3
    msr     cnthctl_el2, x9
    msr     cntvoff_el2, xzr
    mov     x9, #0x33ff
    msr     cptr_el2, x9
    msr     hstr_el2, xzr
    mov     x9, #0x3c5                  /* SPSR: EL1h, DAIF masked */
    msr     spsr_el2, x9
    adr     x9, 1f
    msr     elr_el2, x9
    eret
1:
    /* .bss = 0, BEFORE any static is touched (the tables, the stack, the atomics) */
    adrp    x9, __bss_start
    add     x9, x9, :lo12:__bss_start
    adrp    x10, __bss_end
    add     x10, x10, :lo12:__bss_end
2:  cmp     x9, x10
    b.hs    3f
    stp     xzr, xzr, [x9], #16
    b       2b
3:
    /* FP/SIMD at EL1 and EL0 (CPACR.FPEN = 0b11); SVE stays trapped (ZEN = 0) */
    mov     x9, #(3 << 20)
    msr     cpacr_el1, x9
    isb
    adrp    x9, boot_stack_top
    add     x9, x9, :lo12:boot_stack_top
    mov     sp, x9
    mov     x29, xzr
    mov     x0, x19
    bl      rust_main_aarch64
4:  wfi
    b       4b

.section .bss
.balign 16
boot_stack_bottom:
    .skip 65536
.global boot_stack_top
boot_stack_top:
"#);
