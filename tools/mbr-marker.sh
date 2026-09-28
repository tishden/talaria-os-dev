#!/bin/bash
# mbr-marker.sh: replace the CODE in an image's MBR with a marker that prints a
# line to COM1 (0x3F8) with its VERY FIRST instructions and halts.
#
# Why: on *.metal our BIOS image does not even reach GRUB — serial shows only the
# firmware banner. It is unclear whether the firmware reads the MBR at all. The marker
# answers that unambiguously: a line in the console -> the MBR was read and executed
# (so our GRUB chain is at fault), silence -> the firmware never reaches the MBR (platform).
#
# The partition table and the 0x55AA signature are kept byte for byte — only the
# first 446 bytes (the code) change.
#
# CHAIN=1 — a "TALKING" marker: prints a line and hands control to the ORIGINAL
# sector 0, saved beforehand into a spare sector SAVE_LBA (default 2047 — the LAST
# sector of the gap before the first partition).
# Why the last and not the first free one: GPT images have 34..2047 free and any
# will do, but a PLAIN MBR image has GRUB's core.img in the gap from sector 1 —
# sector 34 falls right on it, and the marker silently broke the loader
# (verified: the chain printed, but GRUB no longer started after it).
# Why this way: an ordinary marker destroys booting, so the probe "marker on a
# volume KNOWN TO BOOT" is impossible — and without it the marker's silence on metal
# is indistinguishable from "raw real-mode output to COM1 never reaches
# get-console-output at all". CHAIN=1 combines both signals in ONE probe: first our
# marker, then the volume's normal chain.
#
# EMIT=<file> — leave the image alone and only emit the code binary (446 bytes max):
# needed when sector 0 is patched not in a file but on a live volume via dd on a helper.
#
# Usage:
#   tools/mbr-marker.sh <image.img>                        (patches the image in place)
#   CHAIN=1 [SAVE_LBA=2047] tools/mbr-marker.sh <image.img>
#   CHAIN=1 EMIT=/tmp/mbr.bin tools/mbr-marker.sh          (code only, no image)
#   TEXT="..." — a custom marker line (ASCII).
set -euo pipefail
CHAIN=${CHAIN:-0}
HOLD=${HOLD:-0}          # seconds to "hold" the line on screen (see below)
PROOF=${PROOF:-}         # LBA to write the execution TRACE to (see below)
READ=${READ:-}           # LBA[:count] — what to READ and put into the trace (PROOF-1)
READ_LBA=${READ%%:*}; READ_CNT=${READ##*:}
[ "$READ_CNT" = "$READ_LBA" ] && READ_CNT=1
SAVE_LBA=${SAVE_LBA:-2047}
EMIT=${EMIT:-}
TEXT=${TEXT:-}
IMG=${1:-}
if [ -z "$EMIT" ]; then
  IMG=${IMG:?usage: mbr-marker.sh <image.img>  (or EMIT=<file>)}
  [ -f "$IMG" ] || { echo "no image $IMG"; exit 1; }
fi
[ -n "$TEXT" ] || { [ "$CHAIN" = 1 ] && TEXT="TALARIA-MBR-CHAIN-OK" || TEXT="TALARIA-MBR-MARKER-OK"; }
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

# ── 16-bit code: initialise the UART (115200 8N1) and print a line ────────────
# In CHAIN mode the code first moves to 0x0600 (otherwise it overwrites itself when
# the original sector is put back at 0x7C00), so it is linked at 0x0600, and the
# first instructions are position-independent (constants, not labels).
LINK=0x7c00
[ "$CHAIN" = 1 ] && LINK=0x600
{
cat <<'EOS'
.code16
.org 0
_start:
    cli
    xor  %ax, %ax
    mov  %ax, %ds
    mov  %ax, %es
    mov  %ax, %ss          /* our own stack: int 10h and pusha in puts are unsafe without it */
    mov  $0x7c00, %sp
    sti
EOS
if [ "$CHAIN" = 1 ]; then
cat <<'EOS'
    /* move 0x7C00 -> 0x0600 and jump into the copy (the code is linked at 0x600) */
    mov  $0x7c00, %si
    mov  $0x600, %di
    mov  $512, %cx
    cld
    rep  movsb
    ljmp $0, $relocated
relocated:
    mov  %dl, drive        /* the boot drive number from the firmware — save it */
EOS
fi
cat <<'EOS'
    /* UART 0x3F8: DLAB=1, divisor 1 (115200), 8N1, FIFO */
    mov  $0x3FB, %dx
    mov  $0x80, %al
    outb %al, %dx
    mov  $0x3F8, %dx
    mov  $0x01, %al
    outb %al, %dx
    mov  $0x3F9, %dx
    xor  %al, %al
    outb %al, %dx
    mov  $0x3FB, %dx
    mov  $0x03, %al
    outb %al, %dx
    mov  $0x3FC, %dx
    mov  $0x03, %al
    outb %al, %dx
    mov  $msg, %si
    call puts
EOS
if [ "$HOLD" != 0 ]; then
cat <<EOS
    /* A DELAY of $HOLD s by the BIOS tick counter (0000:046C, 18.2 Hz) with a dot
     * per tick. Why: on metal the instance console is filled by the FIRMWARE, and
     * it seems to TAKE SNAPSHOTS of the screen periodically — a line printed
     * milliseconds before the next step clears the screen never reaches the
     * console (a probe showed the reference with the marker booting fully, with no
     * marker in the console). The dots also show the code is alive. */
    mov  \$0x46C, %bx
    mov  (%bx), %cx         /* tick at the start */
    mov  \$$(( HOLD * 18 )), %dx
hold_loop:
    mov  (%bx), %ax
    cmp  %ax, %cx
    je   hold_loop
    mov  %ax, %cx
    push %dx
    mov  \$dot, %si
    call puts
    pop  %dx
    dec  %dx
    jnz  hold_loop
EOS
fi
if [ -n "$READ" ]; then
cat <<EOS
    /* INSTRUMENTING THE boot.img -> core.img HANDOFF. Repeats EXACTLY what
     * diskboot.img does: reads the tail of core.img ($READ_CNT sectors from LBA
     * $READ_LBA) in chunks of at most 0x7F sectors (the Phoenix EDD limit), each
     * chunk into its own segment. On metal there is no way to see an error of such
     * a read: diskboot prints it via int 10h, which is invisible there. So TWO numbers
     * go into the trace: the status of the last int 13h (st2) and how many sectors
     * REMAINED unread (remain; 0 = everything read). */
    movw \$$READ_CNT, remain
    movw \$0x0820, rseg
rd_loop:
    mov  \$rdap, %si
    mov  remain, %ax
    cmp  \$0x7F, %ax
    jbe  1f
    mov  \$0x7F, %ax
1:  mov  %ax, 2(%si)       /* how many sectors in this round */
    push %ax
    mov  rseg, %bx
    mov  %bx, 6(%si)       /* buffer segment, offset 0 */
    movw \$0, 4(%si)
    mov  \$0x42, %ah
    mov  drive, %dl
    int  \$0x13
    mov  %ax, st2          /* AH = error code of the last round */
    pop  %ax
    jc   rd_done           /* error — remain will show where it stopped */
    sub  %ax, remain
    mov  %ax, %bx
    shl  \$5, %bx          /* sectors -> paragraphs (512/16) */
    add  %bx, rseg
    add  %ax, 8(%si)       /* LBA += read */
    adcw \$0, 10(%si)
    cmpw \$0, remain
    jne  rd_loop
rd_done:
EOS
fi
if [ -n "$PROOF" ]; then
cat <<EOS
    /* AN EXECUTION TRACE ON DISK (int 13h AH=43, extended write) — a signal that
     * does not depend on the console. On metal output from sector 0 is visible over
     * NO channel (verified: the reference with a marker holding the line for 8 s
     * booted, and neither the line nor the dots appeared in the console), so "the
     * marker is silent" means nothing there. We write 512 bytes from 0x0600 — a copy
     * of our own sector with recognisable strings — into the spare sector $PROOF; the
     * volume is read by a helper later, and the string present = sector 0 WAS EXECUTED. */
    mov  \$wdap, %si
    mov  \$0x4300, %ax     /* AH=43 write, AL=0 no verification */
    mov  drive, %dl
    int  \$0x13
EOS
[ -n "$READ" ] && cat <<EOS
    /* trace 2: EXACTLY what was read from the gap, into sector $(( PROOF - 1 )) */
    mov  \$w2dap, %si
    mov  \$0x4300, %ax
    mov  drive, %dl
    int  \$0x13
EOS
fi
if [ "$CHAIN" != 1 ]; then
cat <<'EOS'
3:  hlt
    jmp  3b
EOS
else
cat <<'EOS'
    /* the original sector 0 -> 0x7C00 via int 13h AH=42 (LBA) */
    mov  $dap, %si
    mov  $0x42, %ah
    mov  drive, %dl
    int  $0x13
    mov  %ax, st1
    jc   read_fail
    mov  $msg2, %si
    call puts
    mov  drive, %dl
    ljmp $0, $0x7c00
read_fail:
    mov  $msgf, %si
    call puts
3:  hlt
    jmp  3b
EOS
fi
cat <<'EOS'
/* Print to TWO channels at once: raw COM1 and the BIOS teletype (int 10h, AH=0x0E).
 * Why: on *.metal get-console-output returns the FIRMWARE console (with ANSI
 * clear-screen escapes in it), and raw writes to 0x3F8 never get there at all —
 * verified by a probe: on c6i.metal the marker was silent, but the chain after it
 * booted and Linux was visible. So the marker's silence on metal meant nothing.
 * Output via int 10h goes exactly through the channel the firmware redirects,
 * and does no harm on ordinary instances: COM1 is visible there. */
puts:
1:  lodsb
    test %al, %al
    jz   9f
2:  mov  $0x3FD, %dx        /* wait for the transmitter to be free */
    inb  %dx, %al
    test $0x20, %al
    jz   2b
    mov  $0x3F8, %dx
    mov  -1(%si), %al
    outb %al, %dx
    pusha                   /* int 10h clobbers registers — save them all */
    mov  -1(%si), %al
    mov  $0x0E, %ah
    mov  $0x0007, %bx
    int  $0x10
    popa
    jmp  1b
9:  ret
EOS
printf 'msg:\n    .ascii "%s\\r\\n"\n    .byte 0\n' "$TEXT"
[ "$HOLD" != 0 ] && printf 'dot:\n    .ascii "."\n    .byte 0\n'
if [ "$CHAIN" = 1 ]; then
cat <<EOS
msg2:
    .ascii "TALARIA-MBR-CHAIN-GO\\r\\n"
    .byte 0
msgf:
    .ascii "TALARIA-MBR-CHAIN-READ-FAIL\\r\\n"
    .byte 0
drive:
    .byte 0x80
st1:
    .word 0xFFFF           /* status of reading the saved sector 0 */
st2:
    .word 0xFFFF           /* status of the last gap read (READ=) */
remain:
    .word 0xFFFF           /* how many sectors remained unread */
rseg:
    .word 0
.align 4
dap:
    .byte 0x10, 0x00        /* packet size, reserved */
    .word 1                 /* sectors */
    .word 0x7c00, 0         /* buffer 0000:7C00 */
    .quad $SAVE_LBA         /* where the saved original sector 0 lies */
EOS
[ -n "$PROOF" ] && cat <<EOS
.align 4
wdap:
    .byte 0x10, 0x00
    .word 1
    .word 0x600, 0          /* we write a copy of our own sector (our strings are there) */
    .quad $PROOF            /* a spare sector for the execution trace */
EOS
[ -n "$READ" ] && cat <<EOS
.align 4
rdap:
    .byte 0x10, 0x00
    .word 1
    .word 0x8000, 0         /* buffer (overwritten in the loop) */
    .quad $READ_LBA
.align 4
w2dap:
    .byte 0x10, 0x00
    .word 1
    .word 0x0000, 0x0820    /* the first tail sector, as GRUB would load it */
    .quad $(( PROOF - 1 ))  /* trace 2: what exactly was read */
EOS
fi
} > "$TMP/m.s"

as --32 -o "$TMP/m.o" "$TMP/m.s"
ld -m elf_i386 -Ttext=$LINK -o "$TMP/m.elf" "$TMP/m.o"
objcopy -O binary "$TMP/m.elf" "$TMP/m.bin"
SZ=$(stat -c%s "$TMP/m.bin")
[ "$SZ" -le 446 ] || { echo "the marker is $SZ bytes — does not fit in 446"; exit 1; }
echo "mbr-marker: marker code $SZ bytes${CHAIN:+ (CHAIN=$CHAIN, original in sector $SAVE_LBA)}"
# Field offsets within the sector — read by whoever later parses the on-disk trace
# (sector PROOF holds a copy of this very sector).
if [ -n "$PROOF" ]; then
  nm "$TMP/m.elf" 2>/dev/null | while read -r addr _ name; do
    case "$name" in st1|st2|remain|rseg)
      printf 'mbr-marker: offset %-6s = %d (0x%x)\n' "$name" $((0x$addr - LINK)) $((0x$addr - LINK));;
    esac
  done
fi

if [ -n "$EMIT" ]; then
  cp "$TMP/m.bin" "$EMIT"
  echo "mbr-marker: code written to $EMIT (put it at the start of sector 0, leaving the table beyond 446 bytes alone)"
  exit 0
fi

# CHAIN: the whole original sector 0 goes to the spare sector (the partition table
# with 0x55AA stays there too, so GRUB gets exactly the sector it had).
if [ "$CHAIN" = 1 ]; then
  dd if="$IMG" of="$TMP/orig.bin" bs=512 count=1 status=none
  dd if="$TMP/orig.bin" of="$IMG" bs=512 seek="$SAVE_LBA" count=1 conv=notrunc status=none
  echo "mbr-marker: original sector 0 saved at LBA $SAVE_LBA"
fi
# keep the partition table (446..510) and replace only the code
dd if="$TMP/m.bin" of="$IMG" bs=1 count="$SZ" conv=notrunc status=none
dd if=/dev/zero of="$IMG" bs=1 seek="$SZ" count=$((446 - SZ)) conv=notrunc status=none
echo "mbr-marker: $IMG — MBR code replaced by the marker (partition table and 0x55AA intact)"
