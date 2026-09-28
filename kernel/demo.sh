#!/bin/sh
# A ~10-minute Talaria OS demo: four scenes, each one a SKIP if its artifacts are
# missing. Run from the host: kernel/demo.sh   (docker + /dev/kvm)
#   1) hello-talaria  — your own binary: no build needed if already built (make -C examples/hello-talaria)
#   2) futex latency  — the headline number: cross-core futex/condvar at 0% CPU
#   3) Redis          — a real server answers over the network from ring 3
#   4) PostgreSQL     — a full-cycle DBMS: psql from the host, persistence
set -u
REPO=$(cd "$(dirname "$0")/.." && pwd)
K=kernel/target/x86_64-talaria/release/talaria
[ -f "$REPO/$K" ] || { echo "no kernel $K — build it (kernel/BUILD.md)"; exit 1; }
# The base TFS1 volume is a derived artifact (it is not in the repository): build
# it if not built yet. Seconds, and the demo stops depending on a regression run.
[ -f "$REPO/kernel/disk.img" ] || (cd "$REPO/kernel" && python3 mkfs.py >/dev/null 2>&1) \
  || echo "WARNING: disk.img not built (no kernel/user/mhello?) — scene 2 may fail"
line() { printf '\n\033[1m== %s ==\033[0m\n' "$1"; }

line "1/4 hello-talaria: your own binary -> talaria-pack -> talaria.mode=cell"
if [ -f "$REPO/examples/hello-talaria/disk.img" ]; then
  "$REPO/examples/hello-talaria/run.sh"
else
  echo "SKIP: examples/hello-talaria/disk.img missing — make -C examples/hello-talaria"
fi

line "2/4 Synchronisation without busy-spin: futex/condvar on unmodified musl"
docker run --rm -i --device /dev/kvm -v "$REPO":/w talaria-qemu2 sh -c '
  timeout 60 qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 2 -m 512 \
    -overcommit cpu-pm=on \
    -kernel /w/'"$K"' -append "console=ttyS0 talaria.test=futexlat" \
    -display none -serial file:/tmp/o.log -no-reboot \
    -drive id=d0,file=/w/kernel/disk.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/dev/null 2>&1
  grep -aE "FUTEXLAT" /tmp/o.log | head -8'
echo "(the waiter sleeps IN THE KERNEL in mwait: woken by a store, no IPI/syscall — PRODUCT.md §3)"

line "3/4 Redis 7.2 in ring 3: RESP over TCP"
if [ -f "$REPO/services/redis/disk-redis.img" ]; then
  "$REPO/services/redis/run.sh" | grep -aE '^\+|^\$|^:|reply|PING|OK' | head -12
else
  echo "SKIP: services/redis/disk-redis.img missing (see services/redis/README.md)"
fi

line "4/4 PostgreSQL 16: psql from the host, DDL/DML"
if [ -f "$REPO/services/postgresql/disk-pg.img" ]; then
  "$REPO/services/postgresql/run.sh" 60 2>/dev/null | sed '/--- serial/q' | tail -20
else
  echo "SKIP: services/postgresql/disk-pg.img missing (see services/postgresql/README.md)"
fi

line "Demo finished"
echo "next: docs/PRODUCT.md (limits/numbers), docs/USER-GUIDE.md §6 (your own app),"
echo "      kernel/test.sh (regression), examples/hello-talaria (template)"
