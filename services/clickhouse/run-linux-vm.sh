#!/bin/sh
# ClickHouse in a Linux GUEST: the same QEMU/KVM as Talaria's, the same binary,
# the same query set. So that the comparison is system against system, not
# "us in a VM against them on hardware".
#   run-linux-vm.sh [secs]     env: SMP=N MEM=MB
set -eu
cd "$(dirname "$0")"
REPO=$(cd ../.. && pwd)
SECS=${1:-300}; SMP=${SMP:-4}; MEM=${MEM:-3072}
LK=$REPO/bench/linux-guest/vmlinuz
[ -f "$LK" ] || { echo "no $LK (bench/linux-guest/mk_guest.sh)"; exit 1; }
BIN=$(ls build/clickhouse-common-static-*/usr/bin/clickhouse | head -1)
mkdir -p build
# init (static) and the tree — exactly what goes into the Talaria image.
# Building the initramfs with the half-gigabyte binary takes a minute, and several
# runs are needed: KEEP_IR=1 reuses one already built.
# ONLY WHAT DID NOT CHANGE MAY BE REUSED. config.xml, users.xml and q.sql from the
# Talaria image are baked into the initramfs: with an old initramfs the Linux guest
# would run the PREVIOUS query set, and the table would label it with the new name.
# The content stamp is printed by `services/clickhouse/ir-stamp.sh`; the harness
# self-check (bench/suite/selfcheck.sh) checks it too.
IR_STAMP=$(./ir-stamp.sh disk-ch.img "$BIN")
if [ "${KEEP_IR:-0}" = 1 ] && [ -f build/initramfs-ch.gz ] \
   && [ "$(cat build/initramfs-ch.stamp 2>/dev/null)" = "$IR_STAMP" ]; then
  echo "initramfs reused (stamp $IR_STAMP matches)"
else
[ "${KEEP_IR:-0}" = 1 ] && [ -f build/initramfs-ch.gz ] \
  && echo "initramfs NOT reused: config.xml/users.xml/q.sql or the binary changed in the image — rebuilding"
docker run --rm -v "$REPO":/w talaria-alpine-build sh -c \
  'gcc -O2 -static -o /w/services/clickhouse/build/linux-init /w/services/clickhouse/linux-init.c'
IR=$(mktemp -d)
mkdir -p "$IR/proc" "$IR/sys" "$IR/tmp" "$IR/etc" "$IR/lib64" "$IR/lib/x86_64-linux-gnu" \
         "$IR/var/lib/clickhouse" "$IR/var/log/clickhouse-server" "$IR/dev"
cp build/linux-init "$IR/init"; chmod +x "$IR/init"
# MODULES FOR A REAL DISK. The database files must live on virtio-blk with ext4, not
# in tmpfs: otherwise four MergeTree test cases compare our disk with their memory
# (see the header of linux-init.c). The host kernel provides virtio_blk and ext4 as
# modules, so they go into the initramfs with their dependencies, numbered in load
# order; init loads all /NN_*.ko in several passes.
KVER=$(uname -r)
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
for m in $(resolve "virtio_blk ext4" | awk '!seen[$0]++'); do
    f=$(modinfo -k "$KVER" -n "$m" 2>/dev/null) || continue
    [ -f "$f" ] || continue
    dst="$IR/$(printf %02d $i)_$m.ko"
    case "$f" in
      *.xz) xz -dc "$f" > "$dst" ;;
      *.zst) zstd -dc "$f" > "$dst" ;;
      *.gz) gzip -dc "$f" > "$dst" ;;
      *) cp "$f" "$dst" ;;
    esac
    i=$((i+1))
done
[ "$i" = 0 ] && echo "WARNING: the virtio_blk/ext4 modules were not found — the guest will use tmpfs, the MergeTree comparison will be unfair"
cp "$BIN" "$IR/clickhouse"
docker run --rm ubuntu:22.04 tar -C / -cf - \
  lib64/ld-linux-x86-64.so.2 lib/x86_64-linux-gnu/ld-linux-x86-64.so.2 \
  lib/x86_64-linux-gnu/libc.so.6 lib/x86_64-linux-gnu/libm.so.6 \
  lib/x86_64-linux-gnu/libdl.so.2 lib/x86_64-linux-gnu/librt.so.1 \
  lib/x86_64-linux-gnu/libpthread.so.0 lib/x86_64-linux-gnu/libgcc_s.so.1 \
  | tar -C "$IR" -xf -
printf 'root:x:0:0:root:/:/bin/sh\n' > "$IR/etc/passwd"; printf 'root:x:0:\n' > "$IR/etc/group"
printf '127.0.0.1 localhost\n' > "$IR/etc/hosts"; printf 'hosts: files dns\n' > "$IR/etc/nsswitch.conf"
# the config, users and queries come FROM THE Talaria IMAGE so that they cannot diverge
for f in config.xml users.xml q.sql; do debugfs -R "cat /$f" disk-ch.img > "$IR/$f" 2>/dev/null; done
(cd "$IR" && find . | cpio -o -H newc --quiet | gzip -1) > build/initramfs-ch.gz
printf '%s\n' "$IR_STAMP" > build/initramfs-ch.stamp
rm -rf "$IR"
fi
# A disk for the database files — its own, clean on every run: on Talaria the volume
# is also brought to an empty table (the final DROP in bench.sql), and both sides
# must start from the same state.
DATA=build/data-linux.img
rm -f "$DATA"
truncate -s 1500M "$DATA"
# The journal STAYS: our ext2 has one (JBD2, ordered), and comparing a journalled
# file system with a non-journalled one would swap one unfairness for another.
mkfs.ext4 -q -F "$DATA"
LOG=build/serial-linux-vm.log; : > "$LOG"
docker run --rm -i --device /dev/kvm -v "$REPO":/w talaria-qemu2 bash -s "$SECS" "$SMP" "$MEM" <<'INNER'
set -u; SECS=$1; SMP=$2; MEM=$3
timeout $((SECS + 20)) qemu-system-x86_64 -M pc -enable-kvm -cpu host -overcommit cpu-pm=on \
  -smp "$SMP" -m "$MEM" -kernel /w/bench/linux-guest/vmlinuz \
  -initrd /w/services/clickhouse/build/initramfs-ch.gz \
  -append "console=ttyS0 quiet args=local --config-file=/config.xml --queries-file /q.sql --time --send_logs_level=none" \
  -drive id=d0,file=/w/services/clickhouse/build/data-linux.img,format=raw,if=none \
  -device virtio-blk-pci,drive=d0 \
  -display none -serial file:/w/services/clickhouse/build/serial-linux-vm.log -no-reboot >/tmp/q.log 2>&1 &
QP=$!
for i in $(seq 1 "$SECS"); do sleep 1; grep -qaE "\[guest\] clickhouse exited" /w/services/clickhouse/build/serial-linux-vm.log && { echo "the guest finished in $i s"; break; }; done
kill $QP 2>/dev/null; wait $QP 2>/dev/null
INNER
echo "--- tail ---"; tail -20 "$LOG"
