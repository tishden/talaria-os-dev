#!/bin/sh
# Talaria soak run: MariaDB (talaria.mode=proc, InnoDB on ext2) under continuous
# client load for N seconds. Criteria (all mandatory):
#   1) the server answers the whole run: no window of failures > 60 s;
#   2) query latency does not degrade: p50 of the last hundred <= 3x p50 of the first hundred;
#   3) consistency: the final count(*) >= the number of acknowledged inserts;
#   4) the kernel is alive: no crash dump in serial.
# Run: kernel/soak.sh [seconds=1800]   (needs the services/mariadb artifacts:
# disk-mdb.img + the static client build/bc/client/mariadb — otherwise SKIP)
set -u
REPO=$(cd "$(dirname "$0")/.." && pwd)
K=kernel/target/x86_64-talaria/release/talaria
SECS=${1:-1800}
MDB=services/mariadb
[ -f "$REPO/$K" ] || { echo "SKIP: no kernel $K"; exit 0; }
[ -f "$REPO/$MDB/disk-mdb.img" ] || { echo "SKIP: no $MDB/disk-mdb.img (build services/mariadb)"; exit 0; }
[ -x "$REPO/$MDB/build/bc/client/mariadb" ] || { echo "SKIP: no static mariadb client"; exit 0; }
mkdir -p "$REPO/$MDB/build"

docker run --rm -i --device /dev/kvm --device /dev/net/tun --device /dev/vhost-net \
  --cap-add NET_ADMIN -v "$REPO":/w talaria-qemu2 bash -s "$K" "$SECS" <<'INNER'
set -u; K=$1; SECS=$2
MCLI="/w/services/mariadb/build/bc/client/mariadb -h 10.0.0.2 -P 3306 -u root --protocol=tcp --connect-timeout=5 -e"
SER=/w/services/mariadb/build/soak-serial.log
LAT=/tmp/lat.log
ip tuntap add tap0 mode tap 2>/dev/null
ip addr add 10.0.0.1/24 dev tap0 2>/dev/null; ip link set tap0 up 2>/dev/null
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 1 -m 1024 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=proc" -display none -serial "file:$SER" -no-reboot \
  -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
  -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
  -drive id=d0,file=/w/services/mariadb/disk-mdb.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 &
QP=$!
# wait for the server to be ready (WT/InnoDB recovery: up to 120 s)
up=0
for i in $(seq 1 120); do
  sleep 1
  if $MCLI "select 1" >/dev/null 2>&1; then up=$i; break; fi
done
if [ $up = 0 ]; then echo "SOAK FAIL: the server did not come up within 120 s"; tail -5 "$SER"; kill $QP 2>/dev/null; exit 1; fi
echo "soak: the server answers after ${up} s; load for ${SECS} s..."
$MCLI "create database if not exists soak; use soak; drop table if exists t; create table t(id int auto_increment primary key, ts bigint, v varchar(64)) engine=innodb" || { echo "SOAK FAIL: DDL"; kill $QP; exit 1; }

: > $LAT
START=$(date +%s); INS_OK=0; Q=0; FAIL_STREAK=0; MAX_STREAK=0
while [ $(( $(date +%s) - START )) -lt $SECS ]; do
  T0=$(date +%s%N)
  # fixed work per iteration (insert by PK): latency must not depend on the table
  # size (count(*) is not allowed here — a full scan grows with the row count)
  if $MCLI "use soak; insert into t(ts,v) values ($(date +%s), 'soak-row')" >/dev/null 2>&1; then
    T1=$(date +%s%N); echo $(( (T1 - T0) / 1000000 )) >> $LAT   # ms per insert
    INS_OK=$((INS_OK+1)); FAIL_STREAK=0
  else
    FAIL_STREAK=$((FAIL_STREAK+1)); [ $FAIL_STREAK -gt $MAX_STREAK ] && MAX_STREAK=$FAIL_STREAK
    sleep 1
  fi
  Q=$((Q+1))
done
CNT=$($MCLI "select count(*) from soak.t" 2>/dev/null | tail -1)
kill $QP 2>/dev/null; wait $QP 2>/dev/null

p50() { sort -n | awk '{a[NR]=$1} END {if (NR) print a[int((NR+1)/2)]; else print 0}'; }
B50=$(head -100 $LAT | p50); E50=$(tail -100 $LAT | p50)
echo "soak: queries=$Q ok=$INS_OK max-failure-window=${MAX_STREAK}s count=$CNT p50(start)=${B50}ms p50(end)=${E50}ms"
R=0
[ $MAX_STREAK -le 60 ] || { echo "SOAK FAIL: failure window ${MAX_STREAK} s > 60 s"; R=1; }
[ "${CNT:-0}" -ge $INS_OK ] 2>/dev/null || { echo "SOAK FAIL: count=$CNT < acknowledged inserts $INS_OK"; R=1; }
[ $INS_OK -gt 0 ] || { echo "SOAK FAIL: not a single successful insert"; R=1; }
if [ "$B50" -gt 0 ] && [ "$E50" -gt $((B50 * 3)) ]; then echo "SOAK FAIL: latency degraded ${B50}ms -> ${E50}ms (>3x)"; R=1; fi
if grep -aq "the kernel has been halted" "$SER"; then echo "SOAK FAIL: kernel crash dump in serial"; R=1; fi
[ $R = 0 ] && echo "SOAK OK: ${SECS} s, ${INS_OK} inserts, latency stable, kernel alive"
exit $R
INNER
RC=$?
echo "--- serial (tail) ---"; tail -5 "$REPO/$MDB/build/soak-serial.log" 2>/dev/null
exit $RC
