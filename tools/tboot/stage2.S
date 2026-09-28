/* tboot stage2: Talaria's own BIOS loader instead of GRUB.
 *
 * What it does: enables A20, takes the memory map (int 15h E820), reads the
 * kernel image from disk (int 13h AH=42, in chunks of 127 sectors) to physical
 * address 0x200000 via unreal mode, builds a multiboot2 structure (cmdline,
 * bootloader and memory map tags) and enters protected mode at the kernel's mb2_start.
 *
 * Why our own: on *.metal an on-disk trace proved that the firmware executes our
 * sector 0 and int 13h reads the whole gap without errors — while GRUB (ours and
 * others alike) prints not a single line there. Both needed primitives exist; GRUB is superfluous.
 *
 * Real-mode memory map:
 *   0x00007C00  top of the stack
 *   0x00008000  this code (stage1 puts it here)
 *   0x00010000  disk read buffer (64 KB)
 *   0x00020000  the multiboot2 structure being built (MBI)
 *   0x00200000  the kernel image
 *
 * THE ON-DISK TRACE (cfg_trace): the only diagnostic channel that works on
 * metal — the console there is mute right up to `normal`. Each phase writes a
 * sector with the phase number, the last int 13h status and the sectors left unread.
 */
.code16
.org 0
.global _start
_start:
    jmp  start
.org 4
    .ascii "TBOT"               /* stage1 checks the signature */
.org 8
cfg_lba:    .quad 0             /* LBA of the kernel image (patched by mk-tboot.sh) */
cfg_sect:   .long 0             /* its length in sectors */
cfg_dest:   .long 0x200000      /* physical load address */
cfg_entry:  .long 0             /* the mb2_start entry point */
cfg_trace:  .long 0             /* trace LBA, 0 — do not write */
cfg_cmd:    .space 256          /* kernel cmdline, ASCIIZ */
.org 288
cfg_vga:    .byte 1             /* 1 — set text mode 3 before entering the kernel */
.org 296
start:
    cli
    xor  %ax, %ax
    mov  %ax, %ds
    mov  %ax, %es
    mov  %ax, %ss
    mov  $0x7C00, %sp
    sti
    mov  %dl, drive
    movl cfg_trace, %eax
    movl %eax, tdap+8           /* where to write the trace */
    call count_boot             /* how many times the machine has booted already (A9) */
    mov  $1, %al
    call trace                  /* phase 1: stage2 alive */
    mov  $msg_s2, %si
    call puts                   /* via int 10h: the only output the firmware on metal was seen to show */
    call a20
    call e820
    mov  $2, %al
    call trace                  /* phase 2: memory map taken */
    call load_kernel
    mov  $3, %al
    call trace                  /* phase 3: the whole kernel read */
    call mbi_finish
    call dump_mbi               /* the finished MBI (with the memory map) — to disk */
    mov  $4, %al
    call trace                  /* phase 4: entering protected mode */
    /* A9: 80x25 text mode is set EXPLICITLY, as GRUB does. The kernel on
     * bare metal writes straight to 0xB8000, and if the firmware keeps the
     * screen in a graphics mode, that output is visible nowhere. GRUB's output
     * on metal IS visible in the console — so the "firmware text screen"
     * channel works, and the difference from our kernel may be exactly the
     * mode left unset. The screen is cleared too: anything on it later was written by the kernel. */
    cmpb $0, cfg_vga
    je   1f
    mov  $0x0003, %ax
    int  $0x10
1:  mov  $msg_go, %si
    call puts
    jmp  to_pm

/* ── print a string via the BIOS teletype (int 10h AH=0Eh) ─────────────────
 * The metal firmware redirects exactly this path to the instance console (that
 * is how GRUB is seen), while the raw 0x3F8 port is not. A line left on the screen
 * after the kernel halted survives until the console snapshot: that makes it a probe. */
puts:
    push %ax
    push %bx
1:  lodsb
    test %al, %al
    jz   2f
    mov  $0x0E, %ah
    mov  $0x0007, %bx
    int  $0x10
    jmp  1b
2:  pop  %bx
    pop  %ax
    ret

/* ── A20 ────────────────────────────────────────────────────────────────── */
a20:
    mov  $0x2401, %ax          /* via the BIOS */
    int  $0x15
    inb  $0x92, %al            /* and via port 0x92, if present */
    test $2, %al
    jnz  1f
    or   $2, %al
    and  $0xFE, %al            /* bit 0 resets the machine, do not touch */
    outb %al, $0x92
1:  ret

/* ── memory map: E820 straight into the body of tag 6 of the future MBI ─── */
e820:
    mov  $0x2000, %ax          /* MBI segment */
    mov  %ax, %es
    mov  $0x130, %di           /* body of the mmap tag */
    xor  %ebx, %ebx
    xor  %si, %si              /* entry counter */
1:  movl $0, %es:20(%di)       /* attributes field: the BIOS may leave it unset */
    mov  $0xE820, %eax
    mov  $24, %ecx
    mov  $0x534D4150, %edx
    int  $0x15
    jc   2f
    cmp  $0x534D4150, %eax
    jne  2f
    movl %es:8(%di), %eax      /* length == 0 -> skip the entry */
    orl  %es:12(%di), %eax
    jz   3f
    add  $24, %di
    inc  %si
3:  test %ebx, %ebx
    jnz  1b
2:  mov  %si, mm_count
    xor  %ax, %ax
    mov  %ax, %es
    ret

/* ── reading the kernel image ─────────────────────────────────────────── */
load_kernel:
    movl cfg_sect, %eax
    movl %eax, remain
    movl cfg_dest, %eax
    movl %eax, dest
    movl cfg_lba, %eax
    movl %eax, kdap+8
    movl cfg_lba+4, %eax
    movl %eax, kdap+12
1:  movl remain, %eax
    testl %eax, %eax
    jz   9f
    cmp  $127, %eax            /* the Phoenix EDD limit per call */
    jbe  2f
    mov  $127, %eax
2:  mov  %ax, chunk
    mov  %ax, kdap+2
    mov  $kdap, %si
    mov  $0x42, %ah
    mov  drive, %dl
    int  $0x13
    mov  %ax, tb_st
    jc   8f
    call copy_up               /* buffer -> high memory */
    movzwl chunk, %eax
    addl %eax, kdap+8          /* LBA += read */
    adcl $0, kdap+12
    movzwl chunk, %eax
    shll $9, %eax
    addl %eax, dest            /* destination address += bytes */
    movzwl chunk, %eax
    subl %eax, remain
    movl remain, %eax
    movl %eax, tb_rem
    jmp  1b
8:  mov  $0x80, %al            /* phase 0x80: read error (status in the trace) */
    call trace
    hlt
    jmp  8b
9:  ret

/* Copy chunk*512 bytes from 0x10000 to dest (a linear address) via unreal
 * mode: 32-bit addressing in real mode. Unreal is set again before EVERY copy —
 * the BIOS is free to spoil the shadow descriptor of ES. */
copy_up:
    pusha
    call unreal
    movl dest, %edi
    movzwl chunk, %ecx
    shll $7, %ecx              /* sectors * 512 / 4 = dwords */
    xor  %esi, %esi
    mov  $0x1000, %ax          /* DS -> 0x10000 (read buffer) */
    mov  %ax, %ds
    cld
    addr32 rep movsl %ds:(%esi), %es:(%edi)
    xor  %ax, %ax
    mov  %ax, %ds
    mov  %ax, %es
    popa
    ret

unreal:
    cli
    lgdt gdt_desc
    mov  %cr0, %eax
    or   $1, %eax
    mov  %eax, %cr0            /* protected mode for two instructions */
    mov  $0x10, %bx
    mov  %bx, %es              /* ES <- flat descriptor (4 GB limit) */
    and  $0xFFFFFFFE, %eax
    mov  %eax, %cr0            /* back to real mode, the ES limit stays */
    sti
    ret

/* ── building the multiboot2 structure ────────────────────────────────── */
/* 0x00 total_size, 0x04 reserved,
 * 0x08 tag 1 (cmdline, string at 0x10), 0x110 tag 2 (loader name),
 * 0x120 tag 6 (memory map, body at 0x130), then the end tag 0. */
mbi_finish:
    mov  $0x2000, %ax
    mov  %ax, %es
    mov  $cfg_cmd, %si
    mov  $0x10, %di
    xor  %cx, %cx
1:  lodsb
    stosb
    inc  %cx
    test %al, %al
    jnz  1b
    /* The cmdline tag size is FIXED (264 = 0x108): the kernel walks the tags in
     * order, adding each one's size, not by our offsets. With the "honest" size
     * 8+len the next tag ended up somewhere other than where we put it, and the
     * memory map never reached the kernel (caught by the very first local run:
     * "multiboot2: map of 0 entries" -> panic without memory). Extra bytes inside
     * the tag are harmless: the string inside it is NUL-terminated. */
    movl $1, %es:0x08
    movl $264, %es:0x0C
    movl $2, %es:0x110
    movl $16, %es:0x114
    movl $0x4F4F4254, %es:0x118   /* "TBOO" */
    movl $0x54, %es:0x11C         /* "T\0\0\0" */
    movl $6, %es:0x120
    movzwl mm_count, %ebx
    imul $24, %ebx, %ebx
    mov  %ebx, %eax
    add  $16, %eax
    movl %eax, %es:0x124
    movl $24, %es:0x128
    movl $0, %es:0x12C
    mov  %bx, %di
    add  $0x130, %di              /* offset of the end tag */
    movl $0, %es:(%di)
    movl $8, %es:4(%di)
    movzwl %di, %eax
    add  $8, %eax
    movl %eax, %es:0x00           /* total_size */
    movl $0, %es:0x04
    xor  %ax, %ax
    mov  %ax, %es
    ret

/* ── boot counter (A9) ──────────────────────────────────────────────────────
 * Reads its own trace sector, takes the counter from it and puts the incremented
 * value into the template — so the disk always holds how many times the machine
 * went through the loader.
 *
 * Why. On bare metal the instance console shows a SNAPSHOT of the firmware
 * screen, and a kernel hang is indistinguishable from an endless reboot loop:
 * the banner is the same. The on-disk counter removes that ambiguity entirely —
 * and gives a positive control for `talaria.rebootat=` (if the kernel reached
 * the point and reset the machine, the counter must grow). */
count_boot:
    mov  $0x3000, %ax
    mov  %ax, rdap+6            /* read buffer segment */
    movl cfg_trace, %eax
    movl %eax, rdap+8
    cmpl $0, cfg_trace
    je   2f
    mov  $rdap, %si
    mov  $0x4200, %ax
    mov  drive, %dl
    int  $0x13
    jc   2f
    push %es
    mov  $0x3000, %ax
    mov  %ax, %es
    cmpl $0x4F4F4254, %es:0     /* "TBOO" */
    jne  1f
    cmpl $0x52542D54, %es:4     /* "T-TR" */
    jne  1f
    movl %es:18, %eax           /* the previous counter */
    jmp  3f
1:  xor  %eax, %eax
3:  pop  %es
    inc  %eax
    movl %eax, tb_boots
2:  ret

/* ── dump of the MBI being built to disk (A9) ─────────────────────────────
 * Writes the first 8 sectors of the MBI area (4 KB from 0x20000) to sectors
 * cfg_trace-8 .. cfg_trace-1. Inside is the int 15h E820 memory map exactly as
 * the kernel will see it.
 *
 * Why. On bare metal the kernel has no output channel before NVMe comes up, and
 * when it is silent it is not even known what it was loaded into: there is no
 * way to see the machine's memory map. The real-mode loader can write to disk,
 * though — so it can lay out the map BEFORE handing over control. That answers
 * "is there RAM at 0x200000, where we put the kernel" without a single guess. */
dump_mbi:
    cmpl $0, cfg_trace
    je   1f
    movl cfg_trace, %eax
    sub  $8, %eax
    movl %eax, mdap+8
    mov  $mdap, %si
    mov  $0x4300, %ax
    mov  drive, %dl
    int  $0x13
1:  ret

/* ── the on-disk trace ────────────────────────────────────────────────── */
trace:                            /* AL = phase number */
    push %ax
    push %si
    push %dx
    cmpl $0, cfg_trace
    je   1f
    mov  %al, tb_phase
    mov  $tdap, %si
    mov  $0x4300, %ax             /* AH=43: extended write */
    mov  drive, %dl
    int  $0x13
1:  pop  %dx
    pop  %si
    pop  %ax
    ret

/* ── switch to protected mode and enter the kernel ─────────────────────── */
to_pm:
    cli
    lgdt gdt_desc
    mov  %cr0, %eax
    or   $1, %eax
    mov  %eax, %cr0
    ljmp $0x08, $pm32
.code32
pm32:
    mov  $0x10, %ax
    mov  %ax, %ds
    mov  %ax, %es
    mov  %ax, %fs
    mov  %ax, %gs
    mov  %ax, %ss
    mov  $0x7C00, %esp
    mov  $0x36D76289, %eax        /* multiboot2 magic (the kernel looks at ebx) */
    mov  $0x20000, %ebx           /* MBI address */
    mov  cfg_entry, %ecx
    jmp  *%ecx

.code16
/* ── data ─────────────────────────────────────────────────────────────── */
.align 8
gdt:
    .quad 0
    .quad 0x00CF9A000000FFFF      /* code 32, base 0, limit 4 GB */
    .quad 0x00CF92000000FFFF      /* data, base 0, limit 4 GB */
gdt_end:
gdt_desc:
    .word gdt_end - gdt - 1
    .long gdt
msg_s2:
    .asciz "TBOOT stage2\r\n"
msg_go:
    .asciz "TBOOT -> kernel\r\n"
drive:
    .byte 0x80
mm_count:
    .word 0
chunk:
    .word 0
remain:
    .long 0
dest:
    .long 0
.align 4
kdap:
    .byte 0x10, 0x00
    .word 0
    .word 0x0000, 0x1000          /* buffer 1000:0000 = 0x10000 */
    .quad 0
.align 4
tdap:
    .byte 0x10, 0x00
    .word 1
    .word trace_buf, 0x0000
    .quad 0
.align 4
mdap:                             /* MBI dump: 8 sectors from 0x2000:0000 */
    .byte 0x10, 0x00
    .word 8
    .word 0x0000, 0x2000
    .quad 0
.align 4
trace_buf:
    .ascii "TBOOT-TRACE"
tb_phase:
    .byte 0
tb_st:
    .word 0                       /* last int 13h status */
tb_rem:
    .long 0                       /* kernel sectors left */
tb_boots:
    .long 0                       /* how many times the machine went through the loader */
    .space 512 - 22
.align 4
rdap:                             /* reading the trace sector back (the counter) */
    .byte 0x10, 0x00
    .word 1
    .word 0x0000, 0x3000
    .quad 0
