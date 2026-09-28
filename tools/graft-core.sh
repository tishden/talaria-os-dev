#!/bin/bash
# graft-core.sh: transplant a FOREIGN loader (boot.img + core.img of the reference
# AL2023) into OUR image, keeping our layout.
#
# Why. On *.metal an on-disk trace proved that sector 0 of our volume is executed and
# our whole core.img (305 sectors) is read via int 13h without a single error. So
# the failure is already INSIDE GRUB's code, after loading. The question this splits:
# "no GRUB runs on this firmware" or "specifically OUR build does not run".
# A foreign loader known to work on metal, inside our image, gives the answer:
#   * GRUB output appears (even an error like "no such device") -> GRUB code runs on
#     metal, our build/its size is at fault;
#   * silence -> nothing runs at all, and it is not about the build.
#
# The foreign core.img is taken from a slice of the reference volume (infra/aws/ref-volume.sh):
#   ref-biosboot.bin — the whole BIOS boot partition, ref-head.bin — sector 0.
# The pointers are recomputed for our gap exactly as mk-bios.sh does it: the LBA of
# the first core.img sector in boot.img at 0x5C and the block list in
# diskboot.img at 0x1F4 (start/len/segment).
#
# Usage:
#   tools/graft-core.sh <ours.img> <ref-head.bin> <ref-biosboot.bin> [out.img]
set -euo pipefail
SRC=${1:?usage: graft-core.sh <ours.img> <ref-head.bin> <ref-biosboot.bin> [out.img]}
HEAD=${2:?no ref-head.bin}
BB=${3:?no ref-biosboot.bin}
OUT=${4:-${SRC%.img}-amzngrub.img}
cp --sparse=always "$SRC" "$OUT"

python3 - "$OUT" "$HEAD" "$BB" <<'PYEOF'
import struct, sys
out, head_p, bb_p = sys.argv[1], sys.argv[2], sys.argv[3]
head = open(head_p, 'rb').read(512)          # sector 0 of the reference (its boot.img)
bb   = open(bb_p, 'rb').read()               # the reference's BIOS boot partition
start, ln, seg = struct.unpack('<QHH', bb[512 - 12:512 - 12 + 12])
sectors = ln + 1                             # the reference's whole core.img
core = bytearray(bb[:sectors * 512])
GAP = 1                                      # our gap starts at sector 1
# block list: the core.img tail lies right after its first sector
struct.pack_into('<QHH', core, 512 - 12, GAP + 1, sectors - 1, seg)
boot = bytearray(head[:440])                 # only the CODE, the partition table is ours
struct.pack_into('<Q', boot, 0x5C, GAP)      # kernel_sector -> our gap
with open(out, 'r+b') as f:
    f.seek(0);            f.write(boot)      # 0..439: the foreign MBR code
    f.seek(GAP * 512);    f.write(core)      # the gap: the foreign core.img
print("graft-core: the reference core.img, %d sectors, embedded from sector %d (segment %#x)"
      % (sectors, GAP, seg))
PYEOF
echo "graft-core: $OUT — our layout, a foreign loader"
