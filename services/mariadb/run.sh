#!/bin/sh
# Run MariaDB on Talaria (tap network 10.0.0.2:3306) + a probe with the static mariadb client from the qemu container.
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
SECS=${1:-60}; EXTRA=${2:-}
docker run --rm -i --device /dev/kvm --device /dev/net/tun --device /dev/vhost-net \
  --cap-add NET_ADMIN -v "$REPO":/w talaria-qemu2 bash -s "$K" "$SECS" "$EXTRA" <<'INNER'
set -u; K=$1; SECS=$2; EXTRA=$3
mkdir -p /w/services/mariadb/build   # without the directory QEMU cannot open the serial file and exits silently
ip tuntap add tap0 mode tap 2>/dev/null
ip addr add 10.0.0.1/24 dev tap0 2>/dev/null; ip link set tap0 up 2>/dev/null
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 1 -m 1024 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=proc $EXTRA" -display none -serial file:/w/services/mariadb/build/serial.log -no-reboot \
  -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
  -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
  -drive id=d0,file=/w/services/mariadb/disk-mdb.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 &
QP=$!
# Client: the static mariadb (build/bc/client/mariadb, built WITHOUT_SERVER + WITH_SSL=system + openssl-libs-static),
# or a python probe speaking the MySQL protocol (mysqlprobe.py) if the client is not built.
MCLI=/w/services/mariadb/build/bc/client/mariadb
if [ -x "$MCLI" ]; then CLI="$MCLI -h 10.0.0.2 -P 3306 -u root --protocol=tcp -e"; else CLI="python3 /w/services/mariadb/mysqlprobe.py 10.0.0.2 3306"; fi
for i in $(seq 1 $SECS); do
  sleep 1
  if $CLI "select 1" >/dev/null 2>&1; then echo "mariadb: the server answers after $i s ($( [ -x "$MCLI" ] && echo static mariadb client || echo mysqlprobe.py ))"; break; fi
done
$CLI "select version(); create database if not exists talaria; use talaria; create table if not exists t(id int auto_increment primary key, v varchar(32)) engine=innodb; insert into t(v) values ('talaria'),('bare metal'); select * from t;" 2>&1 | head -20
kill $QP 2>/dev/null; wait $QP 2>/dev/null
INNER
echo "--- serial (tail) ---"; tail -"$TAIL" "$REPO/services/mariadb/build/serial.log"
