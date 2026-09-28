#!/bin/bash
# Build the Linux guest's initramfs for the harness: modules from bench/linux-guest/initramfs.gz
# (virtio_net + failover; virtio_pci/mmio are built into the kernel) + a new /init + the static
# sender/receiver (musl, the same binaries that run on Talaria). No root needed.
# init is built here too, in talaria-qemu2 (glibc-static). The modules come from
# bench/linux-guest/initramfs.gz (bench/linux-guest/mk_guest.sh) — they are not in git.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TASK=${TASK:-$HOME/sources/task}
SRC=$HERE/../../linux-guest/initramfs.gz
[ -f "$SRC" ] || { echo "no $SRC — run bench/linux-guest/mk_guest.sh first"; exit 1; }
if [ ! -x "$HERE/init" ] || [ "$HERE/init.c" -nt "$HERE/init" ]; then
  docker run --rm -v "$HERE":/w -w /w talaria-qemu2 gcc -O2 -static -o init init.c
fi
D=$(mktemp -d); trap 'rm -rf "$D"' EXIT
(cd "$D" && zcat "$SRC" | cpio -id --quiet 2>/dev/null; rm -f init)
cp "$HERE/init" "$D/init"; chmod +x "$D/init"
cp "$TASK/transport/bin-musl/sender" "$TASK/transport/bin-musl/receiver" "$D/"
(cd "$D" && find . | cpio -o -H newc --quiet | gzip -6) > "$HERE/initramfs.gz"
ls -la "$HERE/initramfs.gz"
