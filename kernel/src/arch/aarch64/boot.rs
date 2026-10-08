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
    .quad   0x2                         /* flags: little-endian, 4K pages, placement NOT anywhere (bit 3
                                           clear): the image is linked KOFF above ONE physical address
                                           (`kaddr::kpa` subtracts a constant), so a loader that moved it
                                           elsewhere would get a kernel whose every `kpa` lies — the check
                                           at `_start` stops it instead (ARM64-PLAN §11 item 4) */
    .quad   0, 0, 0                     /* res2..res4 */
    .ascii  "ARM\x64"                   /* magic */
    .long   0                           /* res5 (a PE header offset — none) */

.global _start
_start:
    /* the device tree's address travels in x0 through everything below */
    mov     x19, x0
    msr     daifset, #0xf
    /* WHERE WE WERE PUT must be where we were linked (see the flags above): otherwise stop
       here, with the MMU off and nothing written, rather than run with every image address
       translated to the wrong frame. A debugger finds the core in `9:`. */
    adr     x9, _head
    ldr     x10, =_head_pa
    cmp     x9, x10
    b.eq    8f
9:  wfi
    b       9b
8:
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
    /* The registers whose reset value the architecture leaves UNKNOWN and this kernel
       reads before it writes them, or lets EL0 see. Under KVM "unknown" is the poison
       0x1de7ec7edbadc0de (reset_unknown in the host's sys_regs.c): TPIDR_EL1 was that,
       the exception stubs took it for the per-core block and wrote into it — every
       fault faulted again, silently, until the stack had walked through the image
       (docs/ARM64-PLAN.md §10.4). TCG starts them at zero, which hid it. */
    msr     tpidr_el1, xzr
    msr     tpidr_el0, xzr
    msr     tpidrro_el0, xzr
    msr     contextidr_el1, xzr
    msr     amair_el1, xzr
    msr     cntkctl_el1, xzr
    msr     mdscr_el1, xzr
    /* FP/SIMD at EL1 and EL0 (CPACR.FPEN = 0b11); SVE stays trapped (ZEN = 0) */
    mov     x9, #(3 << 20)
    msr     cpacr_el1, x9
    isb
    /* STAGE 7.3 (ARM64-PLAN §10.16): the MMU on BEFORE any Rust. The image is linked KOFF above
       where it is loaded, so Rust may not run with the MMU off: an absolute pointer (a format
       string's pieces, a vtable) would be a high address. The early map: every gigabyte of the
       first 512 GiB by its identity as a Device block — what the MMU-off code saw, so the
       tree, the UEFI block and the console are reached as before — except the image's, a
       Normal block; and the image's gigabyte once more at its high alias (L0[511]/L1[510]).
       `mmu::early_enable` replaces it with the real map. */
    bl      boot_map
    /* TCR: the boot value with the core's PARange as IPS (at most 48 bits) */
    mrs     x10, id_aa64mmfr0_el1
    and     x10, x10, #0xf
    mov     x11, #5
    cmp     x10, x11
    csel    x10, x11, x10, hi
    ldr     x1, ={tcr}
    orr     x1, x1, x10, lsl #32
    adrp    x0, boot_l0
    bl      mmu_on
    ldr     x9, =7f
    br      x9                          /* into the high half */
7:
    adrp    x9, boot_stack_top
    add     x9, x9, :lo12:boot_stack_top
    mov     sp, x9
    mov     x29, xzr
    mov     x0, x19
    bl      rust_main_aarch64
4:  wfi
    b       4b

/* A secondary core, started by PSCI CPU_ON: it arrives here with the MMU off, at the
   caller's exception level, with x0 = the context we passed — the top of its own
   stack. No .bss to zero (the boot core did), the same EL2 check as above. */
.global ap_start
ap_start:
    mov     x19, x0
    msr     daifset, #0xf
    mrs     x9, CurrentEL
    lsr     x9, x9, #2
    cmp     x9, #2
    b.ne    5f
    mov     x9, #(1 << 31)
    msr     hcr_el2, x9
    mov     x9, #3
    msr     cnthctl_el2, x9
    msr     cntvoff_el2, xzr
    mov     x9, #0x33ff
    msr     cptr_el2, x9
    msr     hstr_el2, xzr
    mov     x9, #0x3c5
    msr     spsr_el2, x9
    adr     x9, 5f
    msr     elr_el2, x9
    eret
5:
    /* the same UNKNOWN-at-reset registers as on the boot core (see above) */
    msr     tpidr_el1, xzr
    msr     tpidr_el0, xzr
    msr     tpidrro_el0, xzr
    msr     contextidr_el1, xzr
    msr     amair_el1, xzr
    msr     cntkctl_el1, xzr
    msr     mdscr_el1, xzr
    mov     x9, #(3 << 20)
    msr     cpacr_el1, x9
    isb
    /* the MMU on with the kernel's root (`AP_ROOT`, set and cleaned to the point of coherency
       by `kmain::start_secondary`), then into the high half; x19 = the stack's physical top */
    adrp    x9, AP_ROOT
    ldr     x0, [x9, :lo12:AP_ROOT]
    adrp    x9, AP_TCR
    ldr     x1, [x9, :lo12:AP_TCR]      /* the boot core's own TCR: HA/HD as its tables expect */
    bl      mmu_on
    ldr     x9, =8f
    br      x9
8:
    ldr     x10, ={koff}
    add     x19, x19, x10               /* the stack is in the image: its alias */
    mov     sp, x19
    mov     x29, xzr
    mov     x0, x19
    bl      rust_ap_main
6:  wfi
    b       6b

/* boot_map: fill boot_l0/boot_l1/boot_l1h (the MMU is off: adrp gives physical addresses). */
boot_map:
    adrp    x9, boot_l1
    ldr     x11, ={devblk}
    mov     x10, #0
1:  orr     x12, x11, x10, lsl #30
    str     x12, [x9, x10, lsl #3]
    add     x10, x10, #1
    cmp     x10, #512
    b.lo    1b
    adrp    x13, _head
    lsr     x14, x13, #30
    lsl     x15, x14, #30
    ldr     x16, ={normblk}
    orr     x15, x15, x16
    str     x15, [x9, x14, lsl #3]      /* the image's gigabyte by its identity */
    adrp    x17, boot_l1h
    str     x15, [x17, #(510 * 8)]      /* ... and at its high alias */
    adrp    x16, boot_l0
    orr     x12, x9, #3
    str     x12, [x16]
    orr     x12, x17, #3
    str     x12, [x16, #(511 * 8)]
    /* written with the MMU off: no stale line of these tables may shadow them once the
       walks are cacheable */
    mrs     x10, ctr_el0
    ubfx    x10, x10, #16, #4
    mov     x11, #4
    lsl     x10, x11, x10               /* the D-cache line */
    adrp    x9, boot_l0
    add     x12, x9, #(3 * 4096)
2:  dc      ivac, x9
    add     x9, x9, x10
    cmp     x9, x12
    b.lo    2b
    dsb     sy
    ret

/* mmu_on(x0 = the root's physical address, x1 = TCR): MAIR, TCR, TTBR0, then SCTLR.M/C/I.
   The same values `mmu.rs` writes. */
mmu_on:
    ldr     x9, ={mair}
    msr     mair_el1, x9
    msr     tcr_el1, x1
    msr     ttbr0_el1, x0
    isb
    tlbi    vmalle1
    dsb     ish
    isb
    mrs     x9, sctlr_el1
    orr     x9, x9, #(1 << 0)
    orr     x9, x9, #(1 << 2)
    orr     x9, x9, #(1 << 12)
    msr     sctlr_el1, x9
    isb
    ret
.ltorg

.section .bss
.balign 4096
boot_l0:  .skip 4096
boot_l1:  .skip 4096
boot_l1h: .skip 4096
.balign 16
boot_stack_bottom:
    .skip 65536
.global boot_stack_top
boot_stack_top:
"#,
    koff = const crate::mem::KIMG_OFF,
    devblk = const super::mmu::BOOT_DEVICE_BLOCK,
    normblk = const super::mmu::BOOT_NORMAL_BLOCK,
    mair = const super::mmu::BOOT_MAIR,
    tcr = const super::mmu::BOOT_TCR,
);
