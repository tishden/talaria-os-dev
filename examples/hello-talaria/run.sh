#!/bin/sh
# Run hello-talaria on the Talaria kernel in QEMU (docker talaria-qemu2, needs /dev/kvm).
# First build the kernel (kernel/BUILD.md) and run `make` here (binary + disk.img).
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
K=kernel/target/x86_64-talaria/release/talaria
[ -f "$REPO/$K" ] || { echo "no kernel $K — build it as described in kernel/BUILD.md"; exit 1; }
[ -f "$HERE/disk.img" ] || { echo "no disk.img — run make"; exit 1; }
docker run --rm -i --device /dev/kvm -v "$REPO":/w -v "$HERE":/e talaria-qemu2 sh -c '
  timeout 20 qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 1 -m 512 \
    -kernel /w/'"$K"' -append "console=ttyS0 talaria.mode=cell" \
    -display none -serial stdio -no-reboot \
    -drive id=d0,file=/e/disk.img,format=raw,if=none -device virtio-blk-device,drive=d0 \
  | grep -aE "hello|supervise|manifest" || true'
