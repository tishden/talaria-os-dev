#!/bin/sh
# Build the Linux guest's initramfs for the suite: the workload tree + a generic init +
# the host kernel's modules.
#
#   mk-linux-guest.sh <tree> <spec> <out.gz> [modules...]
#
# <tree> — the directory that becomes the guest's root (the same one that goes into
#            Talaria's ext2 image);
# <spec>   — the description file (see the header of guest-init.c), installed as /spec;
# modules   — names of the host kernel's modules (virtio_net virtio_blk ext4 ...),
#            dependencies are resolved automatically.
set -eu
TREE=${1:?tree}
SPEC=${2:?spec}
OUT=${3:?output}
shift 3
MODS=${*:-"virtio_net virtio_pci virtio_mmio"}

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
KVER=$(uname -r)
[ -f "/boot/vmlinuz-$KVER" ] || { echo "no /boot/vmlinuz-$KVER"; exit 1; }

# init is built statically: there is not a single library in the initramfs.
mkdir -p "$HERE/build"
if [ ! -x "$HERE/build/guest-init" ] || [ "$HERE/guest-init.c" -nt "$HERE/build/guest-init" ]; then
    docker run --rm -v "$REPO":/w talaria-alpine-build sh -c \
        "gcc -O2 -static -o /w/bench/suite/lib/build/guest-init /w/bench/suite/lib/guest-init.c" \
        || { echo "init did not build"; exit 1; }
fi

IR=$(mktemp -d)
cp -a "$TREE/." "$IR/"
# AFTER copying: `cp -a src/. dst/` carries the mode of `.` itself onto dst,
# and mktemp -d gives 700. That directory becomes the guest's ROOT, and a workload
# dropped to an unprivileged uid cannot even get through "/" — stat
# returns EACCES on every path, and it looks like "the binary is missing".
chmod 755 "$IR"
cp "$HERE/build/guest-init" "$IR/init"; chmod +x "$IR/init"
cp "$SPEC" "$IR/spec"
mkdir -p "$IR/proc" "$IR/sys" "$IR/dev" "$IR/tmp"

resolve() (
    for m in $1; do
        f=$(modinfo -k "$KVER" -n "$m" 2>/dev/null) || f=""
        case "$f" in */*.ko*) ;; *) continue ;; esac
        dep=$(modinfo -k "$KVER" -F depends "$m" 2>/dev/null | head -1 | tr ',' ' ') || dep=""
        [ -n "$dep" ] && resolve "$dep"
        echo "$m"
    done
)
i=0
for m in $(resolve "$MODS" | awk '!seen[$0]++'); do
    f=$(modinfo -k "$KVER" -n "$m" 2>/dev/null) || continue
    [ -f "$f" ] || continue
    dst="$IR/$(printf %02d $i)_$m.ko"
    case "$f" in
      *.xz)  xz -dc   "$f" > "$dst" ;;
      *.zst) zstd -dc "$f" > "$dst" ;;
      *.gz)  gzip -dc "$f" > "$dst" ;;
      *)     cp "$f" "$dst" ;;
    esac
    i=$((i + 1))
done

(cd "$IR" && find . | cpio -o -H newc --quiet | gzip -1) > "$OUT"
rm -rf "$IR"
cp -f "/boot/vmlinuz-$KVER" "$REPO/bench/linux-guest/vmlinuz" 2>/dev/null || true
ls -la "$OUT"
