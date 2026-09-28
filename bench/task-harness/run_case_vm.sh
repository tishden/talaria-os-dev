#!/bin/bash
# The Low-latency data transfer challenge harness (~/sources/task) in TWO VMs: the "middle"
# (sender/receiver, the udp backend, the same static musl binaries) is two guests in QEMU
# (tap+bridge), the ends (producer/consumer) run on the host in the same container, and the
# harness's shm rings are passed into the guests through ivshmem (host shared memory = BAR2).
# The ends share one clock (the host's) — the same method as the Linux baseline (loopback).
#   STAND=talaria (default) — the guests are the Talaria kernel (microvm, pcie=on, virtio-mmio);
#   STAND=linux   — the guests are Linux (the host's vmlinuz + the initramfs from linux-guest/, -M pc, virtio-net-pci).
# Usage: run_case_vm.sh RATE LOSS COUNT TYPE OUT MODE   (as in transport/bench/run_case.sh)
#   env: TASK (=~/sources/task), SLOTS=8192, LOSS_FIXED, NACK_DELAY_US, FEC_*, MAX_DGRAM, LAT_BD,
#        CORE_VM1=1 CORE_VM2=2 CORE_CONS=3 CORE_PROD=4 HELPER_CORES=5,6 RTPRIO=0, KEEP=1 (do not delete tmp)
set -euo pipefail
RATE=$1; LOSS=$2; COUNT=$3; TYPE=$4; OUT=$5; MODE=${6:-base}
TASK=${TASK:-$HOME/sources/task}
REPO=$(cd "$(dirname "$0")/../.." && pwd)
STAND=${STAND:-talaria}
K=kernel/target/x86_64-talaria/release/talaria
LK=bench/linux-guest/vmlinuz; LI=bench/task-harness/linux-guest/initramfs.gz
SLOTS=${SLOTS:-8192}; PORT=${PORT:-5701}
FEC_R=${FEC_R:-2}; FEC_D=${FEC_D:-1}; FEC_FLUSH_US=${FEC_FLUSH_US:-100}; FEC_SIMD=${FEC_SIMD:-1}
LOSS_BURST=${LOSS_BURST:-1}; LOSS_FIXED=${LOSS_FIXED:-0}; MAX_DGRAM=${MAX_DGRAM:-1400}; WIREV3=${WIREV3:-1}
case "$MODE" in
  base)    NACK=0; FEC_K=0; NACK_DELAY_US=${NACK_DELAY_US:-0} ;;
  nack)    NACK=1; FEC_K=0; NACK_DELAY_US=${NACK_DELAY_US:-0} ;;
  fec)     NACK=0; FEC_K=8; NACK_DELAY_US=${NACK_DELAY_US:-0} ;;
  fecnack) NACK=1; FEC_K=8; NACK_DELAY_US=${NACK_DELAY_US:-300} ;;
  *) echo "MODE: base|nack|fec|fecnack"; exit 1 ;;
esac
RECV_FEC=$([ "$FEC_K" -gt 0 ] && echo 1 || echo 0)
# A layout for 4C/8T (HT pairs 0-4 1-5 2-6 3-7): the spinners on different physical cores — VM1 1, VM2 2,
# consumer 3, producer 4 (the neighbour of the system core 0); QEMU/vhost on helpers 5,6,7. IO=talaria (+2 cell vCPUs):
# the cells on 5,6 (the HT neighbours of their vCPUs), helpers 0,7.
CORE_VM1=${CORE_VM1:-1}; CORE_VM2=${CORE_VM2:-2}; CORE_CONS=${CORE_CONS:-3}; CORE_PROD=${CORE_PROD:-4}
if [ "${IO:-udp}" = talaria ]; then HELPER_CORES=${HELPER_CORES:-0,7}; CORE_CELL1=${CORE_CELL1:-5}; CORE_CELL2=${CORE_CELL2:-6}; else HELPER_CORES=${HELPER_CORES:-5,6,7}; fi
SB="$TASK/transport/bin-musl/sender"; RB="$TASK/transport/bin-musl/receiver"
[ -x "$SB" ] && [ -x "$RB" ] || { echo "no $SB/$RB — run bench/task-harness/build_musl.sh"; exit 1; }
[ -x "$TASK/harness/bin/producer" ] || { echo "no harness/bin/producer — run make -C $TASK/harness"; exit 1; }
W=$(mktemp -d /tmp/talaria-harness.XXXX)
# disks (TFS2): sender and receiver with their manifests
# IO=udp (the guest's kernel stack: Talaria netsys or Linux) | IO=talaria (the zero-syscall rings
# of a Talaria cell: -smp 2, talaria.fast=1, static MACs, STAND=talaria only)
IO=${IO:-udp}
if [ "$IO" = talaria ]; then
  SEND_IO="--io talaria --dpdk-ip 10.0.0.2 --dst-mac 52:54:00:12:34:57"; RECV_IO="--io talaria --dpdk-ip 10.0.0.3 --dst-mac 52:54:00:12:34:56"
  SEND_ENV="env=TALARIA_MAC=52:54:00:12:34:56"; RECV_ENV="env=TALARIA_MAC=52:54:00:12:34:57"
else SEND_IO="--io udp"; RECV_IO="--io udp"; SEND_ENV=""; RECV_ENV=""; fi
cat > "$W/m_send.txt" <<M
binary=/sender
args=sender --dst 10.0.0.3 --port $PORT --slots $SLOTS --wire-v3 $WIREV3 --loss-pct $LOSS --loss-burst $LOSS_BURST --loss-fixed $LOSS_FIXED --nack $NACK --fec-k $FEC_K --fec-r $FEC_R --fec-d $FEC_D --fec-flush-us $FEC_FLUSH_US --fec-simd $FEC_SIMD --tx-accum-us 0 --max-datagram $MAX_DGRAM --lat-breakdown ${LAT_BD:-0} $SEND_IO
$SEND_ENV
restart=0
M
cat > "$W/m_recv.txt" <<M
binary=/receiver
args=receiver --port $PORT --shm /fanout_rx --slots $SLOTS --nack $NACK --fec $RECV_FEC --fec-simd $FEC_SIMD --max-datagram $MAX_DGRAM --lat-breakdown ${LAT_BD:-0} --nack-delay-us $NACK_DELAY_US --loss-pct $LOSS --loss-burst $LOSS_BURST --loss-fixed $LOSS_FIXED $RECV_IO
$RECV_ENV
restart=0
M
python3 "$REPO/kernel/mkfs2.py" "$W/d_send.img" "$SB:sender" "$W/m_send.txt:cell.manifest" >/dev/null
python3 "$REPO/kernel/mkfs2.py" "$W/d_recv.img" "$RB:receiver" "$W/m_recv.txt:cell.manifest" >/dev/null
# for the Linux guest the arguments travel through the kernel cmdline (init parses app=/args=)
sed -n 's/^args=[^ ]* //p' "$W/m_send.txt" > "$W/a_send.txt"; sed -n 's/^args=[^ ]* //p' "$W/m_recv.txt" > "$W/a_recv.txt"
[ "$STAND" = linux ] && { [ -f "$REPO/$LI" ] || { echo "no $LI — run linux-guest/mk_initramfs.sh"; exit 1; }; }
cp "$TASK/harness/bin/producer" "$TASK/harness/bin/consumer" "$W/"
echo "{\"mode\":\"$MODE\",\"rate\":$RATE,\"loss\":$LOSS,\"loss_fixed\":$LOSS_FIXED,\"loss_burst\":$LOSS_BURST,\"io\":\"$IO\",\"count\":$COUNT,\"type\":\"$TYPE\",\"stand\":\"$STAND-2vm\"}" > "$OUT.case.json"
docker run --rm -i --pid=host --device /dev/kvm --device /dev/net/tun --device /dev/vhost-net --cap-add NET_ADMIN --cap-add SYS_NICE \
  --shm-size=256m -v "$REPO":/w -v "$W":/h -e STAND="$STAND" -e IO="$IO" -e LK="$LK" -e LI="$LI" -e HELPER_CORES="$HELPER_CORES" -e CORE_CELL1="${CORE_CELL1:-7}" -e CORE_CELL2="${CORE_CELL2:-0}" -e RTPRIO="${RTPRIO:-0}" \
  talaria-qemu2 bash -s "$K" "$RATE" "$COUNT" "$TYPE" "$SLOTS" "$CORE_VM1" "$CORE_VM2" "$CORE_CONS" "$CORE_PROD" <<'INNER'
set -u
K=$1; RATE=$2; COUNT=$3; TYPE=$4; SLOTS=$5; CVM1=$6; CVM2=$7; CCONS=$8; CPROD=$9
cd /h
# network: a bridge + two taps (VM1 10.0.0.2, VM2 10.0.0.3)
ip link add br0 type bridge 2>/dev/null; ip link set br0 up
for t in tap1 tap2; do ip tuntap add $t mode tap; ip link set $t master br0; ip link set $t up; done
# the harness's shm rings are files in /dev/shm, and they are also the guests' ivshmem backends (size 2^n)
rm -f /dev/shm/fanout_ring /dev/shm/fanout_rx
python3 - <<'PY'
import os
for n in ("fanout_ring","fanout_rx"):
    fd=os.open("/dev/shm/"+n, os.O_CREAT|os.O_RDWR, 0o600); os.ftruncate(fd, 8<<20); os.close(fd)
PY
HELPER=${HELPER_CORES}
boot() { # NAME DISK TAP MAC IP CORE SHM APP ARGSFILE — QEMU as a whole on the helper cores, the vCPU is pinned to $6 afterwards
  if [ "$STAND" = linux ]; then
    taskset -c "$HELPER" qemu-system-x86_64 -name "$1",debug-threads=on -M pc -enable-kvm -cpu host -smp 1 -m 512 \
      -kernel "/w/$LK" -initrd "/w/$LI" -append "console=ttyS0 quiet ip=$5 app=$8 args=$(cat /h/$9)" \
      -display none -serial file:/h/$1.serial -no-reboot \
      -object memory-backend-file,id=hm,mem-path=/dev/shm/$7,size=8M,share=on -device ivshmem-plain,memdev=hm \
      -netdev tap,id=n0,ifname=$3,script=no,downscript=no,vhost=on${POLL_US:+,poll-us=$POLL_US} -device virtio-net-pci,netdev=n0,mac=$4 >/h/$1.qemu.log 2>&1 &
  else
    SMP=1; FASTOPT=""; if [ "$IO" = talaria ]; then SMP=2; FASTOPT="talaria.fast=1"; fi
    taskset -c "$HELPER" qemu-system-x86_64 -name "$1",debug-threads=on -M microvm,pcie=on,rtc=off -enable-kvm -cpu host -smp $SMP -m 512 \
      -kernel "/w/$K" -append "console=ttyS0 talaria.mode=cell talaria.pci=1 $FASTOPT talaria.ip=$5/24 talaria.gw=10.0.0.1 talaria.dns=10.0.0.1" \
      -display none -serial file:/h/$1.serial -no-reboot \
      -object memory-backend-file,id=hm,mem-path=/dev/shm/$7,size=8M,share=on -device ivshmem-plain,memdev=hm \
      -netdev tap,id=n0,ifname=$3,script=no,downscript=no,vhost=on${POLL_US:+,poll-us=$POLL_US} -device virtio-net-device,netdev=n0,mac=$4 \
      -drive id=d0,file=/h/$2,format=raw,if=none -device virtio-blk-device,drive=d0 >/h/$1.qemu.log 2>&1 &
  fi
  echo $!
}
Q2=$(boot recv d_recv.img tap2 52:54:00:12:34:57 10.0.0.3 "$CVM2" fanout_rx /receiver a_recv.txt)
Q1=$(boot send d_send.img tap1 52:54:00:12:34:56 10.0.0.2 "$CVM1" fanout_ring /sender a_send.txt)
pin_vcpu() { # QPID CORE [CORE_CELL]: the "CPU 0/KVM" thread -> CORE, "CPU 1/KVM" (the datapath cell) -> CORE_CELL; the other QEMU/vhost threads go to the helper cores
  for _ in $(seq 1 100); do
    local found=0
    for t in /proc/$1/task/*; do
      c=$(cat "$t/comm" 2>/dev/null)
      case "$c" in
        "CPU 0/KVM") taskset -pc "$2" "$(basename "$t")" >/dev/null; [ "${RTPRIO:-0}" != 0 ] && chrt -f -p "$RTPRIO" "$(basename "$t")" 2>/dev/null; found=1 ;;
        "CPU 1/KVM") taskset -pc "${3:-$2}" "$(basename "$t")" >/dev/null ;;
      esac
    done
    [ $found = 1 ] && return 0; sleep 0.1
  done; echo "vCPU thread of $1 not found" >&2
}
sleep 0.5; pin_vcpu "$Q1" "$CVM1" "${CORE_CELL1:-7}"; pin_vcpu "$Q2" "$CVM2" "${CORE_CELL2:-0}"
# vhost-net kthreads (comm "vhost-<pid>", with an empty cmdline — pgrep -f does not see them) -> the helper cores,
# otherwise they share a core with the busy-polling vCPU (CFS 50/50, 4 ms slices -> tails in ms, drops)
VH=""; for p in /proc/[0-9]*; do c=$(cat $p/comm 2>/dev/null); case "$c" in vhost-*) v=${p#/proc/}; taskset -pc "$HELPER" "$v" >/dev/null 2>&1 && VH="$VH $v";; esac; done
echo "pinned: vcpu1=$CVM1 vcpu2=$CVM2 cells=${CORE_CELL1:-}/${CORE_CELL2:-} vhost=[$VH ] -> $HELPER" >&2
# wait for the receiver:/sender: banners (the process started and created/awaits the ring)
for _ in $(seq 1 300); do grep -q '^receiver:' /h/recv.serial 2>/dev/null && grep -q '^sender:' /h/send.serial 2>/dev/null && break; sleep 0.1; done
grep -q '^receiver:' /h/recv.serial || { echo "receiver did not start"; tail -20 /h/recv.serial; kill $Q1 $Q2; exit 1; }
grep -q '^sender:' /h/send.serial || { echo "sender did not start"; tail -20 /h/send.serial; kill $Q1 $Q2; exit 1; }
sleep 0.5
RT=""; [ "${RTPRIO:-0}" != 0 ] && RT="chrt -f ${RTPRIO:-0}"
$RT taskset -c "$CCONS" ./consumer --shm /fanout_rx --slots "$SLOTS" --count "$COUNT" --csv /h/out.csv > /h/out.report 2> /h/consumer.log &
CPID=$!
sleep 0.2
$RT taskset -c "$CPROD" ./producer --count "$COUNT" --rate "$RATE" --type "$TYPE" --slots "$SLOTS" 2> /h/producer.log
wait $CPID
sleep 1
kill $Q1 $Q2 2>/dev/null; wait 2>/dev/null
INNER
cp "$W/out.report" "$OUT.report"; cp "$W/out.csv" "$OUT.csv" 2>/dev/null || true
cp "$W/send.serial" "$OUT.sender.log"; cp "$W/recv.serial" "$OUT.receiver.log"
cp "$W/producer.log" "$OUT.producer.log" 2>/dev/null || true; cp "$W/consumer.log" "$OUT.consumer.log" 2>/dev/null || true
[ "${KEEP:-0}" = 1 ] || rm -rf "$W"
cat "$OUT.report"
