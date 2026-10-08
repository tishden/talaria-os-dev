#!/bin/bash
# Local client-path probe (ARM64-PLAN §11 item 2a): one Talaria redis server; the closed-loop
# ceiling measured by (H) tlgen in the container and (T) tlgen inside a Talaria guest (the loadgen
# role, job.py on :9100). Runs INSIDE talaria-qemu2 with NET_ADMIN, /dev/kvm.
#   env: KERN (path under /w), IMG (/s/...), LGAPPEND (extra cmdline for the loadgen guest),
#        CONNS, DUR, REP, OP, LGSMP, SRVSMP; LGSOCK=1 + LGCMDS="stat;perf top" — the Talaria
#        generator's console on a socket, the commands sent after the runs (lib/lgperf.py; the
#        guest needs talaria.console=poll in LGAPPEND), the output into /s/gp-perf.log
# Run (/s = a directory with gp-redis.img from `OUT=... infra/mk-ami.sh redis --local-only` and
# lg-initramfs.gz from `lib/mk-linux-guest.sh <dir with tlgen> <spec: tlgen --serve 9100 on
# 10.0.0.4> ... virtio_net virtio_pci`):
#   docker run --rm --cap-add NET_ADMIN --device /dev/net/tun --device /dev/kvm --device /dev/vhost-net \
#     -v "$REPO:/w" -v "$DIR:/s" talaria-qemu2 bash /w/bench/suite/genpath.sh
# WHAT IT CANNOT JUDGE: the order of the generators on EC2 — the local virtual NIC reorders them
# (ARM64-PLAN §11 item 2a).
set -u
KERN=${KERN:-/w/kernel/target/x86_64-talaria/release/talaria}
IMG=${IMG:-/s/gp-redis.img}
CONNS=${CONNS:-16}; DUR=${DUR:-8}; REP=${REP:-3}; OP=${OP:-ping}; LGSMP=${LGSMP:-1}; SRVSMP=${SRVSMP:-1}
LGAPPEND=${LGAPPEND:-}
TLGEN=/w/bench/suite/tlgen/tlgen
ip link add br0 type bridge; ip addr add 10.0.0.1/24 dev br0; ip link set br0 up
for t in tap0 tap1 tap2; do ip tuntap add $t mode tap; ip link set $t master br0; ip link set $t up; done
guest() { # TAP MAC SMP APPEND CONSOLE
  cp "$IMG" "/tmp/$1.img"
  taskset -c "$6" qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp "$3" -m 1024 -display none \
    $( [ "$1" = tap1 ] && [ -n "${LGSOCK:-}" ] && echo "-chardev socket,id=c0,path=/tmp/lg.sock,server=on,wait=off,logfile=$5 -serial chardev:c0" || echo "-serial file:$5" ) -kernel "$KERN" -append "console=ttyS0 talaria.mode=proc talaria.smp=$3 $4" \
    -netdev tap,id=n0,ifname="$1",script=no,downscript=no,vhost=$( [ "$1" = tap1 ] && echo on || echo off) -device virtio-net-device,netdev=n0,mac="$2" \
    -drive id=d0,file="/tmp/$1.img",format=raw,if=none -device virtio-blk-device,drive=d0 >/dev/null 2>&1 &
  G=$!
}
port_open() { python3 -c "import socket,sys; socket.create_connection((sys.argv[1], int(sys.argv[2])), 2).close()" "$1" "$2" 2>/dev/null; }
rate() { python3 -c "
import json,sys; r=json.load(open(sys.argv[1])); c=r['counts']; s=r['latency']['service']
print('%8.0f req/s  p50 %6.1f  p99 %7.1f us  errors %d  sys %s' % (r['throughput_rps'], s['p50']/1000, s['p99']/1000, sum(c.get(k,0) for k in ('lost','timeouts','conn_errors','app_errors')), r.get('syscalls')))" "$1" 2>&1; }
guest tap0 52:54:00:12:34:56 "$SRVSMP" "talaria.manifest=/cell.manifest talaria.ip=10.0.0.2/24 talaria.gw=10.0.0.1" /s/gp-srv.log 2-3; SRV=$G
guest tap1 52:54:00:12:34:57 "$LGSMP" "talaria.manifest=/loadgen.manifest talaria.netidle=1 talaria.ip=10.0.0.3/24 talaria.gw=10.0.0.1 $LGAPPEND" /s/gp-lg.log 4-5; LG=$G
taskset -c 6-7 qemu-system-x86_64 -M pc -enable-kvm -cpu host -smp "$LGSMP" -m 1024 -display none -serial file:/s/gp-linux.log -no-reboot \
  -kernel /w/bench/linux-guest/vmlinuz -initrd /s/lg-initramfs.gz -append "console=ttyS0 quiet" \
  -netdev tap,id=n0,ifname=tap2,script=no,downscript=no,vhost=on -device virtio-net-pci,netdev=n0,mac=52:54:00:12:34:58 >/dev/null 2>&1 &
LX=$!
t0=$SECONDS; until port_open 10.0.0.2 6379 || [ $((SECONDS-t0)) -gt 120 ]; do sleep 1; done
t0=$SECONDS; until port_open 10.0.0.3 9100 || [ $((SECONDS-t0)) -gt 120 ]; do sleep 1; done
t0=$SECONDS; until port_open 10.0.0.4 9100 || [ $((SECONDS-t0)) -gt 120 ]; do sleep 1; done
"$TLGEN" --proto resp --host 10.0.0.2 --port 6379 --op set --keyspace 100000 --valsize 100 --conns 4 --rate 0 --duration 3 --warmup 0 --spin --out /dev/null >/dev/null 2>&1
ARGS="--proto resp --host 10.0.0.2 --port 6379 --op $OP --keyspace 100000 --conns $CONNS --rate 0 --duration $DUR --warmup 1 --timeout 30"
for r in $(seq 1 $REP); do
  taskset -c 4-5 "$TLGEN" $ARGS --out /tmp/h.json >/dev/null 2>&1; echo "H (container tlgen) #$r: $(rate /tmp/h.json)"
  python3 /w/bench/suite/lib/job.py 10.0.0.3 9100 /tmp/t.json $ARGS >/dev/null 2>&1; echo "T (Talaria tlgen)   #$r: $(rate /tmp/t.json)"
  python3 /w/bench/suite/lib/job.py 10.0.0.4 9100 /tmp/l.json $ARGS >/dev/null 2>&1; echo "L (Linux VM tlgen)  #$r: $(rate /tmp/l.json)"
done
[ -n "${LGSOCK:-}" ] && LGCMDS="${LGCMDS:-perf top;perf record 4000}" python3 /w/bench/suite/lib/lgperf.py /s/gp-perf.log
cp /tmp/t.json /s/gp-t.json; cp /tmp/h.json /s/gp-h.json
kill $SRV $LG $LX 2>/dev/null; wait 2>/dev/null
