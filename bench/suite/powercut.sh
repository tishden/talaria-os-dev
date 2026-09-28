#!/bin/sh
# A SURVIVAL PROBE: does a database on Talaria survive a HARD power-off?
#
# The scenario is the same as stopping an instance on EC2 without a handled power
# button: a fresh image -> warm-up -> 10 s of load -> N s of idling ->
# QEMU killed (the power cord pulled) -> a second boot of the same image. The probe
# passes if the server listens on the port again: that means the FS journal and the database journal
# agree about what was on the disk.
#
#   bench/suite/powercut.sh <workload> <idle, s> [extra kernel cmdline]
#   bench/suite/powercut.sh mysql 180
#
# The workload is a directory from workloads/ with PORT/CASES/WARMUP_CMD in meta.env
# (mysql, postgresql, mongodb, redis). The disk load is the first test case
# at a rate of 2000 req/s over 4 connections. The image and the serial logs stay in
# results/ (disk-pc-<workload>.img, serial-pc-<workload>-{1,2}.log).
#
# A normal shutdown (the ACPI button -> SIGTERM -> sync -> S5) is checked
# separately, by the pwrbtn gate in kernel/test.sh; here it is the abnormal one.
set -u
HERE=$(cd "$(dirname "$0")" && pwd); REPO=$(cd "$HERE/../.." && pwd); cd "$REPO"
. "$HERE/lib/vm.sh"
WL=${1:?workload}; IDLE=${2:?idle, s}; EXTRA=${3:-}
WD=$HERE/workloads/$WL; TLGEN=$HERE/tlgen/tlgen; GUEST_IP=10.0.0.2
[ -f "$WD/meta.env" ] || { echo "no $WD/meta.env"; exit 2; }
. "$WD/meta.env"
[ -n "${WARMUP_CMD:-}" ] || { echo "$WL: meta.env has no WARMUP_CMD — a workload without a database on disk, the probe is not about it"; exit 2; }
case1=${CASES%%|*}; case_args=${case1#*:}
vm_tap_up >/dev/null
TREE=$(mktemp -d); "$WD/tree.sh" "$TREE" >/dev/null; cp "$WD/manifest" "$TREE/cell.manifest"
IMG=$HERE/results/disk-pc-$WL.img
kernel/mkext2.sh "$TREE" "$IMG" "${DISK_MB:-768}M" >/dev/null; rm -rf "$TREE"
log=$HERE/results/serial-pc-$WL-1.log
# THE GUEST'S PATHS ARE RELATIVE TO THE REPOSITORY. The image and the log go into the container as
# `/w/<path>` (see lib/vm.sh), and an absolute path turned into `/w//home/...`:
# QEMU could not create the console journal and exited at once, while from the outside it looked
# like "the first boot did not come up within 120 s". The same trick as in run-local.sh.
vm_up_talaria "${IMG#$REPO/}" proc "${SMP:-2}" "${MEM:-1024}" "${log#$REPO/}" "$EXTRA" || { echo "QEMU did not come up"; exit 1; }
vm_wait_port $GUEST_IP "$PORT" "${BOOT_WAIT:-120}" || { echo "the first boot did not come up"; vm_down; exit 1; }
export TLGEN GUEST_IP PORT; sh -c "$WARMUP_CMD" || echo "the warm-up did not work"
echo "-- the warm-up is done $(date +%T)"
eval "\"\$TLGEN\" --proto \"\$PROTO\" --host \$GUEST_IP --port \$PORT $case_args --conns 4 --rate 2000 --duration 10 --warmup 1 --timeout 5 --spin --out /dev/null" >/dev/null 2>&1 || echo "the load did not work"
echo "-- the load is done $(date +%T), idling for $IDLE s"
sleep "$IDLE"
tail -3 "$log" | cut -c1-160
vm_down; echo "-- THE POWER CORD IS PULLED $(date +%T)"
log2=$HERE/results/serial-pc-$WL-2.log
vm_up_talaria "${IMG#$REPO/}" proc "${SMP:-2}" "${MEM:-1024}" "${log2#$REPO/}" "$EXTRA" || { echo "QEMU-2 did not come up"; exit 1; }
if vm_wait_port $GUEST_IP "$PORT" "${BOOT_WAIT:-120}"; then echo "== $WL: THE SECOND BOOT — the server is listening, the probe PASSED"; r=0
else echo "== $WL: THE SECOND BOOT — the server DID NOT COME UP, the probe FAILED"; r=1; fi
/usr/bin/grep -a 'ERROR\|ext2:\|Aborting\|ready for connections\|THE JOURNAL' "$log2" | cut -c1-200 | head -30
vm_down
echo "POWERCUT-$WL-DONE rc=$r"; exit $r
