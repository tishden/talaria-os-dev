#!/bin/bash
# mk-bios.sh: a raw Talaria disk image with BIOS boot.
#
# Why: EC2 *.metal does NOT accept an AMI with boot-mode uefi (checked on 6 types),
# so native bare metal needs a BIOS loader. We take GRUB (i386-pc) and the
# kernel's multiboot2 entry — no MBR/stage2 of our own here (see mk-tboot.sh for that).
#
# BIOS image layout (msdos):
#   sector 0        MBR (boot.img) + partition table
#   sectors 1..2047 the BIOS loader "gap": core.img
#   p1              ext2 /boot: /talaria (the kernel ELF) + /boot/grub/grub.cfg
#   p2              data partition (TFS/ext2 as is) — the same as mk-ami.sh uses
#
# HYBRID=1 — a HYBRID image that boots both via BIOS and via UEFI (GPT):
#   p1 BIOS boot (ef02, 1 MB, no FS) — the gap for core.img
#   p2 ESP (FAT16): EFI/BOOT/BOOTX64.EFI (uefi-stub with the kernel inside) + talaria.cfg
#   p3 ext2 /boot: /talaria + grub.cfg (the BIOS path)
#   p4 talaria-data — data partition (the kernel finds it BY NAME, fs.rs)
#   On EC2 such an image is registered with --boot-mode uefi-preferred: ordinary
#   instances go through UEFI, *.metal through BIOS.
#
# LAYOUT=amzn (only together with HYBRID=1) — a layout ONE-TO-ONE with AL2023,
# whose volume the *.metal firmware executes while ours it did not. A byte-by-byte
# comparison left exactly four differences, and all of them are removed here:
#   p128 ESP@2048 (physically the FIRST partition; its FAT sector ends in 55AA,
#        whereas we had a raw core.img without a signature there),
#   p127 BIOS boot right after it (the gap for core.img),
#   p1   talaria-boot (ext2: /talaria + grub.cfg) — a "low" number, like the root on
#        Amazon,
#   p2   talaria-data UP TO THE END OF THE VOLUME (the reference has no free space at all).
# The volume size is DISK_GB (default 8, as AL2023): this also gives the
# protective MBR's end CHS ff ff ff — at 2 GiB it is representable and sgdisk wrote
# the real one.
#
# Usage:
#   tools/mk-bios.sh out.img data.img ["cmdline"]
#   HYBRID=1 tools/mk-bios.sh out.img data.img ["cmdline"]
#   HYBRID=1 LAYOUT=amzn [DISK_GB=8] tools/mk-bios.sh out.img data.img ["cmdline"]
# Local check (no AWS): qemu WITHOUT -kernel and WITHOUT OVMF —
#   qemu-system-x86_64 -drive file=out.img,format=raw ...  (SeaBIOS boots it)
# EC2: import-snapshot + register-image WITHOUT --boot-mode uefi (or uefi-preferred
# for the hybrid).
#
# Dependencies: sfdisk/sgdisk, mkfs.ext2 (e2fsprogs), mtools (for the hybrid's ESP);
# GRUB i386-pc in docker (the image is built on first run).
set -euo pipefail
OUT=${1:?usage: mk-bios.sh out.img data.img [cmdline]}
DATA=${2:?no data.img}
CFG=${3:-"console=ttyS0 talaria.mode=cell talaria.pci=1 talaria.ip=dhcp"}
HYBRID=${HYBRID:-0}
LAYOUT=${LAYOUT:-}
# The Amazon layout makes sense only for the hybrid: it has both an ESP and BIOS boot.
[ "$LAYOUT" = amzn ] && HYBRID=1
HERE=$(cd "$(dirname "$0")/.." && pwd)
KERN="$HERE/kernel/target/x86_64-talaria/release/talaria"
EFI="$HERE/kernel/uefi-stub/target/x86_64-unknown-uefi/release/talaria-uefi-stub.efi"
[ -f "$KERN" ] || { echo "no kernel $KERN — build it first (BUILD.md)"; exit 1; }
[ -f "$DATA" ] || { echo "no $DATA"; exit 1; }
[ "$HYBRID" = 1 ] && [ ! -f "$EFI" ] && { echo "HYBRID=1 needs $EFI — kernel/uefi-stub/build.sh"; exit 1; }
IMG=talaria-grub

if ! docker image inspect "$IMG" >/dev/null 2>&1; then
  echo "mk-bios: building docker image $IMG (grub i386-pc)"
  docker build -q -t "$IMG" - >/dev/null <<'DOCKER'
FROM ubuntu:22.04
ENV DEBIAN_FRONTEND=noninteractive
# grub-bios-setup lives in grub-pc (not in grub-pc-bin); install_devices_empty
# silences the debconf question "where to install the loader" — it goes nowhere.
RUN apt-get update -qq \
 && echo "grub-pc grub-pc/install_devices_empty boolean true" | debconf-set-selections \
 && apt-get install -y -qq grub-pc-bin grub2-common grub-pc \
 && rm -rf /var/lib/apt/lists/*
DOCKER
fi

W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT

KB=$(( ($(stat -c%s "$KERN") + 1048575) / 1048576 ))
BOOT_MB=$(( KB + 12 ))                       # kernel + room for grub modules
# THE ESP SIZE IS COMPUTED FROM THE STUB ITSELF, NOT HARDCODED.
#
# The stub carries the kernel ELF INSIDE, and the kernel's .bss is IN THE FILE
# (linker.ld puts everything into one PT_LOAD with filesz == memsz), i.e. the image
# grows with every new static array. A hardcoded forty-eight megabytes survived a
# "~28 MB stub" and died at 52.9: `mcopy` answered just "Disk full", test.sh swallowed
# it into /dev/null and turned it into a SKIP — five BIOS-path gates vanished from
# the suite SILENTLY, noticeable only by comparing the count with the previous run.
ESP_MB=$(( ($(stat -c%s "$EFI") + 1048575) / 1048576 + 16 ))   # the stub (kernel inside) + FAT headroom
DATA_B=$(stat -c%s "$DATA")
DATA_MB=$(( (DATA_B + 1048575) / 1048576 ))

# ── /boot (ext2): kernel + grub.cfg ──
# Where GRUB looks for /boot/grub/grub.cfg: the partition number depends on the layout.
case "$LAYOUT:$HYBRID" in
  amzn:*) GRUB_PART="(hd0,gpt1)" ;;   # talaria-boot became p1 (see the header)
  *:1)    GRUB_PART="(hd0,gpt3)" ;;
  *)      GRUB_PART="(hd0,msdos1)" ;;
esac
mkdir -p "$W/root/boot/grub"
cp "$KERN" "$W/root/talaria"
cat > "$W/root/boot/grub/grub.cfg" <<CFGEOF
set timeout=0
set default=0
serial --unit=0 --speed=115200
terminal_input serial console
terminal_output serial console
menuentry "Talaria OS" {
  echo TALARIA-GRUB-OK
  multiboot2 /talaria $CFG
  boot
}
CFGEOF
truncate -s ${BOOT_MB}M "$W/boot.img"
# ^dir_index: our ext2 does not read htree (GRUB does, but the kernel may open the volume too)
mkfs.ext2 -q -b 4096 -O ^dir_index,^has_journal -d "$W/root" "$W/boot.img" >/dev/null

if [ "$HYBRID" = 1 ]; then
  # ── ESP (FAT16) for the UEFI path ──
  truncate -s ${ESP_MB}M "$W/esp.img"
  printf '%s' "$CFG" > "$W/talaria.cfg"
  cp "$EFI" "$W/BOOTX64.EFI"
  if command -v mcopy >/dev/null 2>&1 && command -v mkfs.vfat >/dev/null 2>&1; then
    mkfs.vfat -F 16 -n TALARIA "$W/esp.img" >/dev/null
    mmd -i "$W/esp.img" ::/EFI ::/EFI/BOOT
    mcopy -i "$W/esp.img" "$W/BOOTX64.EFI" ::/EFI/BOOT/BOOTX64.EFI
    mcopy -i "$W/esp.img" "$W/talaria.cfg" ::/talaria.cfg
  else
    docker run --rm -v "$W":/w alpine sh -ec '
      apk add -q mtools dosfstools
      mkfs.vfat -F 16 -n TALARIA /w/esp.img >/dev/null
      mmd -i /w/esp.img ::/EFI ::/EFI/BOOT
      mcopy -i /w/esp.img /w/BOOTX64.EFI ::/EFI/BOOT/BOOTX64.EFI
      mcopy -i /w/esp.img /w/talaria.cfg ::/talaria.cfg'
  fi
  BIOSB_MB=1
  if [ "$LAYOUT" = amzn ]; then
    # ── a layout one-to-one with AL2023 ──
    DISK_GB=${DISK_GB:-8}
    TOTAL_MB=$(( DISK_GB * 1024 ))
    NEED_MB=$(( 1 + ESP_MB + BIOSB_MB + BOOT_MB + DATA_MB + 1 ))
    [ "$TOTAL_MB" -ge "$NEED_MB" ] || { echo "DISK_GB=$DISK_GB too small: need >= ${NEED_MB}MB"; exit 1; }
    truncate -s ${TOTAL_MB}M "$OUT"          # sparse: zeros take no space
    sgdisk -Z "$OUT" >/dev/null
    P_ESP=2048
    P_BB=$(( P_ESP + ESP_MB * 2048 ))
    P_BOOT=$(( P_BB + BIOSB_MB * 2048 ))
    P_DATA=$(( P_BOOT + BOOT_MB * 2048 ))
    # PARTITION numbers and physical order as in the reference: the ESP first and
    # numbered 128, BIOS boot 127, a "low" p1 next, and the last partition takes the
    # volume to its end (end 0 = to the edge of free space) — AL2023 has no free
    # space on the volume at all.
    sgdisk -n 128:${P_ESP}:+${ESP_MB}M   -t 128:ef00 -c 128:"EFI System Partition" \
           -n 127:${P_BB}:+${BIOSB_MB}M  -t 127:ef02 -c 127:"BIOS Boot Partition" \
           -n 1:${P_BOOT}:+${BOOT_MB}M   -t 1:8300   -c 1:talaria-boot \
           -n 2:${P_DATA}:0              -t 2:8300   -c 2:talaria-data "$OUT" >/dev/null
    GAP=$P_BB
    GAP_LEN=$(( BIOSB_MB * 2048 ))
    dd if="$W/esp.img"  of="$OUT" bs=512 seek="$P_ESP"  conv=notrunc status=none
    dd if="$W/boot.img" of="$OUT" bs=512 seek="$P_BOOT" conv=notrunc status=none
    dd if="$DATA"       of="$OUT" bs=512 seek="$P_DATA" conv=notrunc status=none
    LAYOUT_DESC="GPT (as AL2023), volume ${DISK_GB}GiB: p128 ESP@${P_ESP} (${ESP_MB}MB), p127 bios-boot@${P_BB}, p1 /boot@${P_BOOT} (${BOOT_MB}MB), p2 talaria-data@${P_DATA} (to the end of the volume)"
  else
  TOTAL_MB=$(( 1 + BIOSB_MB + ESP_MB + BOOT_MB + DATA_MB + 1 ))
  truncate -s ${TOTAL_MB}M "$OUT"
  sgdisk -Z "$OUT" >/dev/null
  sgdisk -n 1:2048:+${BIOSB_MB}M -t 1:ef02 -c 1:bios-boot \
         -n 2:0:+${ESP_MB}M      -t 2:ef00 -c 2:ESP \
         -n 3:0:+${BOOT_MB}M     -t 3:8300 -c 3:talaria-boot \
         -n 4:0:+${DATA_MB}M     -t 4:8300 -c 4:talaria-data "$OUT" >/dev/null
  GAP=$(sgdisk -i 1 "$OUT" | sed -n 's/^First sector: \([0-9]*\).*/\1/p')
  P2=$(sgdisk -i 2 "$OUT" | sed -n 's/^First sector: \([0-9]*\).*/\1/p')
  P3=$(sgdisk -i 3 "$OUT" | sed -n 's/^First sector: \([0-9]*\).*/\1/p')
  P4=$(sgdisk -i 4 "$OUT" | sed -n 's/^First sector: \([0-9]*\).*/\1/p')
  GAP_LEN=$(( BIOSB_MB * 2048 ))
  dd if="$W/esp.img"  of="$OUT" bs=512 seek="$P2" conv=notrunc status=none
  dd if="$W/boot.img" of="$OUT" bs=512 seek="$P3" conv=notrunc status=none
  dd if="$DATA"       of="$OUT" bs=512 seek="$P4" conv=notrunc status=none
  LAYOUT_DESC="GPT: p1 bios-boot@${GAP}, p2 ESP@${P2} (${ESP_MB}MB), p3 /boot@${P3} (${BOOT_MB}MB), p4 talaria-data@${P4} (${DATA_MB}MB)"
  fi
else
  TOTAL_MB=$(( 1 + BOOT_MB + DATA_MB + 1 ))
  truncate -s ${TOTAL_MB}M "$OUT"
  P1_S=2048
  P1_N=$(( BOOT_MB * 2048 ))
  P2_S=$(( P1_S + P1_N ))
  P2_N=$(( DATA_MB * 2048 ))
  sfdisk -q "$OUT" >/dev/null <<SFEOF
label: dos
${P1_S},${P1_N},83,*
${P2_S},${P2_N},83
SFEOF
  dd if="$W/boot.img" of="$OUT" bs=512 seek=$P1_S conv=notrunc status=none
  dd if="$DATA"       of="$OUT" bs=512 seek=$P2_S conv=notrunc status=none
  GAP=1; GAP_LEN=2047
  LAYOUT_DESC="MBR: p1 /boot@${P1_S} (${BOOT_MB}MB), p2 data@${P2_S} (${DATA_MB}MB)"
fi

# ── GRUB: core.img into the gap + boot.img into the MBR ──
# grub-mkimage runs in the container (the i386-pc modules are there). The EMBEDDING we
# do ourselves: grub-bios-setup (both in the container and on the host) insists on
# guessing the root device of the HOST system (overlayfs / LVM dm-0) and fails on an
# image file. The operation itself is fully deterministic and defined by the format:
#   * the first 440 bytes of the MBR are boot.img (the disk signature and the partition
#     table at 0x1B8..0x1FF are left alone);
#   * boot.img[0x5C] (8 bytes LE) — the LBA of the first sector of core.img;
#   * the first sector of core.img (diskboot.img) holds at offset 0x1F4 a list of
#     blocks {start u64, len u16, segment u16}: where from and how many sectors to
#     read, and into which segment (0x820).
mkdir -p "$W/gmod"
docker run --rm -v "$W":/w "$IMG" bash -ec "
  grub-mkimage -O i386-pc -p '${GRUB_PART}/boot/grub' -o /w/gmod/core.img \
    biosdisk part_msdos part_gpt ext2 multiboot2 normal configfile echo serial terminal loadenv search
  cp /usr/lib/grub/i386-pc/boot.img /w/gmod/
  chmod -R a+rw /w/gmod"
python3 - "$OUT" "$W/gmod/boot.img" "$W/gmod/core.img" "$GAP" "$GAP_LEN" "$HYBRID" <<'PYEOF'
import struct, sys
out, boot_p, core_p, gap, gap_len = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5])
hybrid = sys.argv[6] == '1'
boot = bytearray(open(boot_p, 'rb').read())
core = bytearray(open(core_p, 'rb').read())
core += b'\x00' * ((-len(core)) % 512)
# boot.img[0x66]: GRUB has a `jmp +5` there around the drive-number fixup, and
# grub-setup replaces it with two NOPs when the target is a HARD DISK (then a `%dl`
# outside 0x00-0x0f/0x80-0x8f is forced to 0x80: some firmware passes garbage to
# the MBR). We embed core.img ourselves, so we apply this fixup ourselves too —
# the reference AL2023 has exactly 90 90 there, we had eb 05 (difference (6) of the
# byte-by-byte comparison). Our target is always a hard disk.
boot[0x66:0x68] = b'\x90\x90'
sectors = len(core) // 512
if sectors > gap_len:
    sys.exit("core.img (%d sectors) does not fit in the gap (%d)" % (sectors, gap_len))
# the block list in diskboot.img: read the rest of core.img from the next sector
struct.pack_into('<QHH', core, 512 - 12, gap + 1, sectors - 1, 0x820)
boot[0x5C:0x64] = struct.pack('<Q', gap)          # kernel_sector
with open(out, 'r+b') as f:
    f.seek(0); f.write(boot[:440])                # MBR code, leaving 0x1B8..0x1FF alone
    f.seek(gap * 512); f.write(core)              # core.img into the gap
    if hybrid:
        # The protective MBR is brought to the CANONICAL form — exactly the one of
        # an ordinary Linux AMI that boots on *.metal in legacy-bios mode (taken
        # from a live volume):
        #   boot=0x00, start CHS 00 02 00, type 0xEE, end CHS ff ff ff,
        #   first LBA 1, size min(disk-1, 0xFFFFFFFF).
        # Before, we set flag 0x80 and kept the real CHS/size from sgdisk (an
        # "add 0x80" check gave nothing) — on metal the firmware did not execute
        # even the first byte of sector 0, and this was the only remaining
        # difference from the reference.
        f.seek(0, 2); total = f.tell() // 512
        f.seek(0x1BE)
        f.write(struct.pack('<BBBBBBBBII', 0x00, 0x00, 0x02, 0x00, 0xEE,
                            0xFF, 0xFF, 0xFF, 1, min(total - 1, 0xFFFFFFFF)))
print("mk-bios: core.img, %d sectors, embedded from sector %d" % (sectors, gap))
PYEOF

echo "mk-bios: $OUT (${TOTAL_MB}MB) — $LAYOUT_DESC"
echo "mk-bios: cmdline: $CFG"
echo "BIOS check: qemu-system-x86_64 -M q35 -m 512 -drive file=$OUT,format=raw,if=none,id=d0 -device virtio-blk-pci,drive=d0 -nographic  (no -kernel, no OVMF)"
[ "$HYBRID" = 1 ] && echo "UEFI check: the same + -bios OVMF_CODE.fd (the same image)"
exit 0
