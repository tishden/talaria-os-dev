#!/bin/bash
# mk-ami.sh: a raw Talaria disk image for EC2/UEFI hardware:
#   GPT: p1 = ESP (FAT16: EFI/BOOT/BOOTX64.EFI = uefi-stub with the kernel inside,
#        talaria.cfg = cmdline), p2 = data partition (a TFS/ext2 image as is;
#        the kernel sees GPT and takes data from the second partition — fs.rs).
# Usage:
#   tools/mk-ami.sh out.img data.img ["cmdline"]
#     data.img — a TFS image (talaria-pack/mkfs2.py) or ext2 (mkext2.sh)
#     cmdline  — default: console=ttyS0 talaria.mode=cell talaria.pci=1 talaria.ip=dhcp
# Local check (no AWS): qemu OVMF + out.img as an NVMe disk — boots from the ESP.
# EC2: import-snapshot + register-image (--boot-mode uefi --ena-support) — see README.
# Dependencies: sgdisk (gdisk), mkfs.vfat (dosfstools), mcopy/mmd (mtools);
# without mtools on the host the ESP is built in docker (alpine + apk add).
set -euo pipefail
OUT=${1:?usage: mk-ami.sh out.img data.img [cmdline]}
DATA=${2:?no data.img}
CFG=${3:-"console=ttyS0 talaria.mode=cell talaria.pci=1 talaria.ip=dhcp"}
HERE=$(cd "$(dirname "$0")/.." && pwd)
EFI="$HERE/kernel/uefi-stub/target/x86_64-unknown-uefi/release/talaria-uefi-stub.efi"
[ -f "$EFI" ] || { echo "no $EFI — kernel/uefi-stub/build.sh"; exit 1; }
[ -f "$DATA" ] || { echo "no $DATA"; exit 1; }

# The ESP size is computed from the stub itself: it carries the kernel ELF with its
# .bss inside and grows with every new static array. A hardcoded 48 MB (sized for a
# ~28 MB stub) stopped fitting once the stub passed 48 MB, and mcopy failed with
# "Disk full" — the same fix as in mk-bios.sh.
ESP_MB=$(( ($(stat -c%s "$EFI") + 1048575) / 1048576 + 16 ))   # the stub + FAT headroom
DATA_B=$(stat -c%s "$DATA")
DATA_MB=$(( (DATA_B + 1048575) / 1048576 ))
TOTAL_MB=$(( 1 + ESP_MB + DATA_MB + 1 ))  # +1MB alignment, +1MB GPT tail

W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT

# ── ESP (FAT16) ──
truncate -s ${ESP_MB}M "$W/esp.img"
printf '%s' "$CFG" > "$W/talaria.cfg"
if command -v mcopy >/dev/null 2>&1 && command -v mkfs.vfat >/dev/null 2>&1; then
  mkfs.vfat -F 16 -n TALARIA "$W/esp.img" >/dev/null
  mmd -i "$W/esp.img" ::/EFI ::/EFI/BOOT
  mcopy -i "$W/esp.img" "$EFI" ::/EFI/BOOT/BOOTX64.EFI
  mcopy -i "$W/esp.img" "$W/talaria.cfg" ::/talaria.cfg
else
  echo "mk-ami: no mtools on the host — building the ESP in docker (alpine)"
  cp "$EFI" "$W/BOOTX64.EFI"
  docker run --rm -v "$W":/w alpine sh -ec '
    apk add -q mtools dosfstools
    mkfs.vfat -F 16 -n TALARIA /w/esp.img >/dev/null
    mmd -i /w/esp.img ::/EFI ::/EFI/BOOT
    mcopy -i /w/esp.img /w/BOOTX64.EFI ::/EFI/BOOT/BOOTX64.EFI
    mcopy -i /w/esp.img /w/talaria.cfg ::/talaria.cfg'
fi

# ── GPT + partitions ──
truncate -s ${TOTAL_MB}M "$OUT"
sgdisk -Z "$OUT" >/dev/null
sgdisk -n 1:2048:+${ESP_MB}M -t 1:ef00 -c 1:ESP \
       -n 2:0:+${DATA_MB}M   -t 2:8300 -c 2:talaria-data "$OUT" >/dev/null
P1=$(sgdisk -i 1 "$OUT" | sed -n 's/^First sector: \([0-9]*\).*/\1/p')
P2=$(sgdisk -i 2 "$OUT" | sed -n 's/^First sector: \([0-9]*\).*/\1/p')
dd if="$W/esp.img" of="$OUT" bs=512 seek="$P1" conv=notrunc status=none
dd if="$DATA"     of="$OUT" bs=512 seek="$P2" conv=notrunc status=none
echo "mk-ami: $OUT (${TOTAL_MB}MB): p1 ESP@${P1} (${ESP_MB}MB, BOOTX64.EFI + talaria.cfg), p2 data@${P2} (${DATA_MB}MB <- $DATA)"
echo "mk-ami: cmdline: $CFG"
echo "local check: qemu OVMF + '-drive file=$OUT,format=raw,if=none,id=nv -device nvme,drive=nv,serial=tal001' (no -kernel)"
