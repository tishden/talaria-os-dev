#!/bin/bash
# mk-tboot.sh: a Talaria image with its OWN loader, no GRUB.
#
# Why. On *.metal an on-disk trace proved that the firmware executes our sector 0,
# and int 13h reads any number of sectors from the volume without a single error —
# yet neither our GRUB nor another one on the same disk prints a single line. Both
# primitive operations that booting rests on work; GRUB in the chain is
# superfluous. Our loader reads the kernel itself and jumps to mb2_start — the very
# multiboot2 entry the kernel already has.
#
# Image layout (msdos, as fs.rs expects: the data partition is the SECOND one):
#   sector 0        stage1 (tools/tboot/stage1.S) + partition table
#   sectors 1..     stage2 (tools/tboot/stage2.S), as many as it takes
#   sector 1000     the suite's mode list (SUITE=), 4 sectors, ASCIIZ, one per line
#   sector 1004     suite state (magic + number of the next run)
#   sector 2047     loader trace (phase, int 13h status, remainder)
#   sectors 2048..  run logs: 129 sectors per slot (talaria.trace=2048)
#   p1 @8192        the kernel image as a raw block (the ELF PT_LOAD segment)
#   p2              the data partition (TFS/ext2 as is)
#
# Usage:
#   tools/mk-tboot.sh out.img data.img ["cmdline"]
#   SUITE="talaria.test=ext2;talaria.test=wc" tools/mk-tboot.sh out.img data.img \
#       "console=ttyS0 talaria.pci=1 talaria.suite=1000 talaria.trace=<log LBA>"
#   (the script prints the log area LBA itself; it also writes the list and zeroes the counter)
# Local check (no AWS): qemu WITHOUT -kernel and WITHOUT OVMF —
#   qemu-system-x86_64 -drive file=out.img,format=raw ...
set -euo pipefail
OUT=${1:?usage: mk-tboot.sh out.img data.img [cmdline]}
DATA=${2:?no data.img}
CFG=${3:-"console=ttyS0 talaria.pci=1 talaria.ip=dhcp"}
HERE=$(cd "$(dirname "$0")/.." && pwd)
KERN=${KERN:-$HERE/kernel/target/x86_64-talaria/release/talaria}
TRACE_LBA=${TRACE_LBA:-2047}
[ -f "$KERN" ] || { echo "no kernel $KERN — build it first (BUILD.md)"; exit 1; }
[ -f "$DATA" ] || { echo "no $DATA"; exit 1; }
W=$(mktemp -d); trap 'rm -rf "$W"' EXIT

as --32 -o "$W/s1.o" "$HERE/tools/tboot/stage1.S"
ld -m elf_i386 -Ttext=0x7c00 -o "$W/s1.elf" "$W/s1.o"
objcopy -O binary "$W/s1.elf" "$W/stage1.bin"
as --32 -o "$W/s2.o" "$HERE/tools/tboot/stage2.S"
ld -m elf_i386 -Ttext=0x8000 -o "$W/s2.elf" "$W/s2.o"
objcopy -O binary "$W/s2.elf" "$W/stage2.bin"
S1=$(stat -c%s "$W/stage1.bin"); S2=$(stat -c%s "$W/stage2.bin")
[ "$S1" -le 440 ] || { echo "stage1 is $S1 bytes — does not fit in 440"; exit 1; }
# The PHYSICAL entry (linker.ld: mb2_start - KOFF; a script symbol, so `nm` types it A).
ENTRY=$(nm "$KERN" | sed -n 's/^0*\([0-9a-f]*\) [TtAa] mb2_start_pa$/\1/p' | head -1)
[ -n "$ENTRY" ] || { echo "the kernel has no mb2_start_pa symbol"; exit 1; }

python3 - "$OUT" "$DATA" "$KERN" "$W/stage1.bin" "$W/stage2.bin" "$CFG" "0x$ENTRY" "$TRACE_LBA" <<'PYEOF'
import os, struct, sys
out, data_p, kern_p, s1_p, s2_p, cmdline, entry, trace_lba = sys.argv[1:9]
entry = int(entry, 16); trace_lba = int(trace_lba)
s1 = bytearray(open(s1_p, 'rb').read())
s2 = bytearray(open(s2_p, 'rb').read())
k = open(kern_p, 'rb')

# ── the kernel PT_LOAD segment: it goes onto the disk as a raw block ─────────
k.seek(0); eh = k.read(64)
assert eh[:4] == b'\x7fELF', 'the kernel is not an ELF'
phoff, phentsize, phnum = struct.unpack('<Q', eh[32:40])[0], *struct.unpack('<HH', eh[54:58])
seg = None
for i in range(phnum):
    k.seek(phoff + i * phentsize)
    ph = k.read(phentsize)
    p_type, = struct.unpack('<I', ph[0:4])
    if p_type == 1:                                   # PT_LOAD
        p_offset, p_vaddr, p_paddr, p_filesz, p_memsz = struct.unpack('<QQQQQ', ph[8:48])
        assert seg is None, 'expected ONE PT_LOAD segment'
        seg = (p_offset, p_paddr, p_filesz, p_memsz)
assert seg, 'the kernel has no PT_LOAD'
p_offset, p_paddr, p_filesz, p_memsz = seg
ksect = (p_filesz + 511) // 512

# ── layout ───────────────────────────────────────────────────────────────────
S2_LBA, S2_SECT = 1, (len(s2) + 511) // 512
# The kernel is moved to 4 MB (it used to be 2048): the gap before it is the
# diagnostics work area, and it must be FIXED, otherwise the log LBA would depend
# on the kernel size and would have to be put into the cmdline on every build.
# In the gap: the suite list (1000), its counter (1004), the loader trace (2047)
# and the run logs (2048 + i*129, 47 slots up to the kernel).
KERN_LBA = 8192
SUITE_LBA, SUITE_STATE, LOG_LBA, LOG_SLOT = 1000, 1004, 2048, 129
DATA_LBA = (KERN_LBA + ksect + 2047) // 2048 * 2048
data_sz = os.path.getsize(data_p)
dsect = (data_sz + 511) // 512
TOTAL = DATA_LBA + dsect + 2048
assert S2_SECT < trace_lba < KERN_LBA, 'the trace overlaps stage2 or the kernel'

# ── stage1: how many stage2 sectors to read ──────────────────────────────────
i = s1.find(b'\x10\x00')                              # start of the DAP
struct.pack_into('<H', s1, i + 2, S2_SECT)

# ── stage2: configuration (offsets fixed by .org in stage2.S) ───────────────
struct.pack_into('<Q', s2, 8, KERN_LBA)
struct.pack_into('<I', s2, 16, ksect)
struct.pack_into('<I', s2, 20, p_paddr)
struct.pack_into('<I', s2, 24, entry)
struct.pack_into('<I', s2, 28, trace_lba)
cb = cmdline.encode() + b'\0'
assert len(cb) <= 256, 'cmdline longer than 256 bytes'
s2[32:32 + len(cb)] = cb
s2[288] = int(os.environ.get('VGAMODE', '1')) & 1   # text mode 3 before entering the kernel

suite = os.environ.get('SUITE', '')
with open(out, 'wb') as f:
    f.truncate(TOTAL * 512)
    f.seek(0);              f.write(s1)
    f.seek(S2_LBA * 512);   f.write(s2)
    # the suite mode list and a zeroed counter (the kernel reads them via blk_read_raw)
    if suite:
        items = [x.strip() for x in suite.split(';') if x.strip()]
        blob = ('\n'.join(items) + '\n').encode() + b'\0'
        assert len(blob) <= 4 * 512, 'the suite list is longer than 2 KB'
        # +1 slot: slot zero is the common "prologue" of every boot (early flushes
        # before the kernel knows whose run it is); runs occupy slots from the first
        assert (len(items) + 1) * LOG_SLOT + LOG_LBA <= KERN_LBA, 'more log slots than fit in the gap before the kernel'
        f.seek(SUITE_LBA * 512); f.write(blob)
        st = bytearray(512)
        st[:13] = b'TALARIA-SUITE'
        f.seek(SUITE_STATE * 512); f.write(st)
    # the kernel as a raw block
    k.seek(p_offset); left = p_filesz; f.seek(KERN_LBA * 512)
    while left:
        buf = k.read(min(left, 1 << 20))
        f.write(buf); left -= len(buf)
    # the data partition
    f.seek(DATA_LBA * 512)
    with open(data_p, 'rb') as d:
        while True:
            buf = d.read(1 << 20)
            if not buf: break
            f.write(buf)
    # partition table: p1 — the kernel (raw block), p2 — data (fs.rs takes the SECOND)
    def ent(lba, cnt, typ):
        return struct.pack('<BBBBBBBBII', 0x00, 0xFE, 0xFF, 0xFF, typ,
                           0xFE, 0xFF, 0xFF, lba, cnt)
    f.seek(0x1BE)
    f.write(ent(KERN_LBA, ksect, 0xDA))               # 0xDA — "not a FS", raw data
    f.write(ent(DATA_LBA, dsect, 0x83))
    f.write(b'\0' * 32)
    f.seek(0x1FE); f.write(b'\x55\xAA')

print("mk-tboot: stage1 %d B, stage2 %d B (%d sect.), kernel %d B -> LBA %d (%d sect.), "
      "data LBA %d (%d sect.), image %d MB"
      % (len(s1), len(s2), S2_SECT, p_filesz, KERN_LBA, ksect, DATA_LBA, dsect, TOTAL * 512 // 1048576))
print("mk-tboot: entry mb2_start=%#x, loaded at %#x, loader trace in sector %d" % (entry, p_paddr, trace_lba))
if suite:
    n = len([x for x in suite.split(';') if x.strip()])
    print("mk-tboot: a suite of %d modes: list LBA %d, counter LBA %d, logs from LBA %d, %d sectors each"
          % (n, SUITE_LBA, SUITE_STATE, LOG_LBA, LOG_SLOT))
    print("mk-tboot: the cmdline needs talaria.suite=%d and talaria.trace=%d" % (SUITE_LBA, LOG_LBA))
PYEOF
echo "mk-tboot: $OUT"
echo "mk-tboot: cmdline: $CFG"
