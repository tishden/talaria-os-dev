#!/bin/sh
# Run PostgreSQL on Talaria (tap network 10.0.0.2) + a psql probe from the "host" (inside the qemu container).
# run.sh [secs] [append]. Serial log -> build/serial.log
set -u
# THE LOG TAIL IS A PARAMETER, NOT A CONSTANT. The gates (`kernel/validate.sh`)
# look for evidence in THIS output, and the `stat:` diagnostic block grew to
# fifty lines and pushed the results and the exit code out of the tail: a
# healthy workload looked like a failure. A human needs the tail, a gate needs
# the whole log — hence TAIL=.
TAIL=${TAIL:-30}
REPO=$(cd "$(dirname "$0")/../.." && pwd)
K=kernel/target/x86_64-talaria/release/talaria
# MEM=MB: the guest's RAM (default 1024; above ~3 GB the microvm puts RAM above 4 GB — the high area).
SECS=${1:-40}; EXTRA=${2:-}
# THE NUMBER OF CORES IS A PARAMETER (default 1). It was added to investigate a
# forked PostgreSQL background process crashing with `#PF err=0x15` on `SMP=2
# talaria.smp=2` (an instruction fetch from an mmap region). That defect is
# closed — the COW refcount table did not cover the frame, so `fork` made a
# shared frame writable and freed it under a live owner (CHANGELOG v1.13, the
# performance programme; gate `forktls`) — and PostgreSQL is measured on two
# cores in `bench/suite`. The knob stays: it is how that class is reproduced.
docker run --rm -i --device /dev/kvm --device /dev/net/tun --device /dev/vhost-net \
  --cap-add NET_ADMIN -e SMP -e MEM -v "$REPO":/w talaria-qemu2 bash -s "$K" "$SECS" "$EXTRA" <<'INNER'
set -u; K=$1; SECS=$2; EXTRA=$3; SMP=${SMP:-1}
mkdir -p /w/services/postgresql/build   # without the directory QEMU cannot open the serial file and exits silently
ip tuntap add tap0 mode tap 2>/dev/null
ip addr add 10.0.0.1/24 dev tap0 2>/dev/null; ip link set tap0 up 2>/dev/null
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp ${SMP:-1} -m ${MEM:-1024} \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=proc $EXTRA" -display none -serial file:/w/services/postgresql/build/serial.log -no-reboot \
  -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
  -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
  -drive id=d0,file=/w/services/postgresql/disk-pg.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 &
QP=$!
PSQL=/w/services/postgresql/build/pg-static/opt/pg/bin/psql
for i in $(seq 1 $SECS); do
  sleep 1
  if PGCONNECT_TIMEOUT=5 timeout 8 $PSQL -h 10.0.0.2 -U postgres -tAc "select 1" >/dev/null 2>&1; then echo "psql: the server answers after $i s"; break; fi
done
PGCONNECT_TIMEOUT=5 timeout 20 $PSQL -h 10.0.0.2 -U postgres -c "select version()" 2>&1 | head -5
PGCONNECT_TIMEOUT=5 timeout 20 $PSQL -h 10.0.0.2 -U postgres -c "create table if not exists t(id serial primary key, v text, ts timestamptz default now()); insert into t(v) values ('talaria'),('bare metal'); select * from t;" 2>&1 | head -12
kill $QP 2>/dev/null; wait $QP 2>/dev/null
INNER
echo "--- serial (tail) ---"; tail -"$TAIL" "$REPO/services/postgresql/build/serial.log"
# Persistence check: run again WITHOUT rebuilding the disk (mkdisk.sh) — the rows of table t must still be there.
