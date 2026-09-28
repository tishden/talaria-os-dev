/* tboot stage1: the code of sector 0 — reads stage2 from the gap and
 * hands control to it. Nothing more is needed in 440 bytes.
 *
 * Why a loader of our own at all: on *.metal a trace on disk proved that the
 * firmware DOES EXECUTE our sector 0, and int 13h reads any number of sectors
 * from the volume without errors — yet neither our GRUB nor another one on
 * this disk prints a single line. We have both primitive operations that
 * booting rests on; GRUB in the chain is superfluous.
 *
 * The partition table (446..510) and 0x55AA are left alone — mk-tboot.sh writes them.
 */
.code16
.org 0
.global _start
_start:
    cli
    xor  %ax, %ax
    mov  %ax, %ds
    mov  %ax, %es
    mov  %ax, %ss
    mov  $0x7C00, %sp
    sti
    mov  %dl, drive             /* drive number from the firmware */
    /* stage2 -> 0000:8000 */
    mov  $dap, %si
    mov  $0x42, %ah
    mov  drive, %dl
    int  $0x13
    jc   err
    /* stage2 signature check */
    cmpl $0x544F4254, 0x8004    /* "TBOT" */
    jne  err
    mov  drive, %dl
    ljmp $0, $0x8000
err:
    /* Output here is not visible on metal anyway (verified), so we simply
     * halt: the signal upward goes through the on-disk trace from stage2. */
    hlt
    jmp  err

drive:
    .byte 0x80
.align 4
dap:
    .byte 0x10, 0x00
s2_sectors:
    .word 32                    /* how many stage2 sectors (patched by mk-tboot.sh) */
    .word 0x8000, 0x0000        /* buffer 0000:8000 */
    .quad 1                     /* stage2 starts at sector 1 */
