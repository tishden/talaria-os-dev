#!/bin/sh
# Build vmlinuz + initramfs for the Linux-guest baseline from the CURRENT host
# kernel: resolves the full module dependency chain for virtio net (PCI and
# MMIO transports), packs numbered .ko files + static init. Run as root.
# usage: mk_guest.sh [output-dir]
set -e
OUT=${1:-$(dirname "$0")}
KVER=$(uname -r)
D=$(mktemp -d)
cp "$(dirname "$0")/init" "$D/init"
chmod +x "$D/init"
mkdir -p "$D/proc"

# Depth-first dependency resolution. Subshell body: POSIX sh variables are
# global, recursion would clobber the loop variable otherwise.
resolve() (
    for m in $1; do
        f=$(modinfo -k "$KVER" -n "$m" 2>/dev/null) || f=""
        case "$f" in
        */*.ko*) ;;
        *) continue ;; # built-in or absent
        esac
        dep=$(modinfo -k "$KVER" -F depends "$m" 2>/dev/null | head -1 | tr ',' ' ') || dep=""
        if [ -n "$dep" ]; then
            resolve "$dep"
        fi
        echo "$m"
    done
)

list=$(resolve "virtio_net virtio_pci virtio_mmio" | awk '!seen[$0]++')
i=0
for m in $list; do
    f=$(modinfo -k "$KVER" -n "$m" 2>/dev/null) || continue
    [ -f "$f" ] || continue # built-in
    dst="$D/$(printf %02d $i)_$m.ko"
    case "$f" in
    *.xz) xz -dc "$f" >"$dst" ;;
    *.zst) zstd -dc "$f" >"$dst" ;;
    *.gz) gzip -dc "$f" >"$dst" ;;
    *) cp "$f" "$dst" ;;
    esac
    i=$((i + 1))
done
echo "packed modules: $(ls "$D" | grep '\.ko$' || echo none)"

(cd "$D" && find . | cpio -o -H newc --quiet | gzip -9) >"$OUT/initramfs.gz"
cp "/boot/vmlinuz-$KVER" "$OUT/vmlinuz"
rm -rf "$D"
ls -la "$OUT/vmlinuz" "$OUT/initramfs.gz"
