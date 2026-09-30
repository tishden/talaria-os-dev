#!/bin/sh
# The arm64 soak (docs/ARM64-PLAN.md §4.13, §10.3 step 4): the modes whose verdict depends
# on what two cores see of each other's writes — run N times each under KVM on a real
# weakly-ordered machine, every failing boot's console kept. Under TCG on an x86 host the
# machine underneath is TSO and the loop proves only the logic; the numbers and the
# ordering answers are the stand's (infra/aws/kvm-stand-aarch64.sh runs this).
#
#   kernel/soak-aarch64.sh [N=50] [outdir]      # the modes below, N boots each
#   MODES="shmrace memrace" kernel/soak-aarch64.sh 200
#
# It needs what the gate needs: the image (kernel/build-aarch64.sh), the binaries
# (make -C kernel/user aarch64) and the ext2 images the gate leaves in
# kernel/target/a64-disks (run kernel/test-aarch64.sh once first).
set -u
N=${1:-50}
REPO=$(cd "$(dirname "$0")/.." && pwd)
OUT=${2:-$REPO/kernel/target/a64-soak}
IMG=kernel/target/aarch64-talaria/release/talaria.img
TD=kernel/target/a64-disks
[ -f "$REPO/$IMG" ] || { echo "there is no $IMG — run kernel/build-aarch64.sh first"; exit 1; }
[ -f "$REPO/$TD/ext2.img" ] || { echo "there is no $TD/ext2.img — run kernel/test-aarch64.sh once first"; exit 1; }
mkdir -p "$OUT"
ACCEL="-cpu max"
[ "$(uname -m)" = "aarch64" ] && [ -c /dev/kvm ] && ACCEL="-enable-kvm -cpu host"
KVMDEV=""; [ -c /dev/kvm ] && KVMDEV="--device /dev/kvm"
SMP=${TAL_SMP:-4}
# one SECS APPEND [DISK] -> the console in $LOG
LOG=$(mktemp)
trap 'rm -f "${LOG:?}"' EXIT
one() {
  D=""
  if [ -n "${3:-}" ]; then
    cp "$REPO/$TD/$3.img" "$REPO/$TD/soak.img"
    D="-drive id=d0,file=/w/$TD/soak.img,format=raw,if=none -device virtio-blk-device,drive=d0"
  fi
  QEMU="timeout $1 qemu-system-aarch64 -M virt,gic-version=3,its=on $ACCEL -smp $SMP -m 1024 -nographic \
    -kernel /w/$IMG -append \"console=ttyAMA0 $2 talaria.smp=$SMP talaria.poweroff=1\" $D"
  if command -v qemu-system-aarch64 >/dev/null 2>&1; then
    sh -c "$(echo "$QEMU" | sed "s|/w/|$REPO/|g")" > "$LOG" 2>&1
  else
    docker run --rm $KVMDEV -v "$REPO:/w" -w /w talaria-qemu-aarch64 sh -c "$QEMU" > "$LOG" 2>&1
  fi
}
# The table: name | the command line | the disk ("" — none) | the verdict that must be there.
# A trap, a panic or a spoilt register anywhere fails the boot whatever the verdict says.
TABLE='ring|talaria.test=ring talaria.ringn=2000000||0 corrupted, 0 with a wrong length
shmrace|talaria.test=shmrace talaria.shmdelay=2000||shmrace: ALL OK
memrace|talaria.test=memrace|ext2|memrace: ALL OK
fputest|talaria.test=fputest|ext2|fputest: ALL OK
futexlat|talaria.test=futexlat talaria.futexhang=3||FUTEXLAT: ALL OK
pthreads|talaria.test=pthreads||PTHREADS: counter=200000 .we expected 200000. -> OK
sigtest|talaria.test=sigtest|ext2|sigtest: ALL OK
forkap|talaria.mode=proc|forkap|crashme: forkap ALL OK
schedapi|talaria.test=schedapi|ext2|schedapi: ALL OK'
BAD='\[trap\]|PANIC|is spoilt|\[futex\] IT HAS HUNG|AN UNEXPECTED SIGSEGV|an unknown number'
: > "$OUT/summary.txt"
echo "== the arm64 soak: $N boots of each mode, $SMP cores, $ACCEL =="
echo "$TABLE" | while IFS='|' read -r name append disk want; do
  case " ${MODES:-$(echo "$TABLE" | cut -d'|' -f1 | tr '\n' ' ')} " in *" $name "*) ;; *) continue;; esac
  fails=0; i=1; t0=$(date +%s)
  while [ $i -le "$N" ]; do
    one 300 "$append" "$disk"
    if grep -aqE "$want" "$LOG" && ! grep -aqE "$BAD" "$LOG"; then :; else
      fails=$((fails+1)); cp "$LOG" "$OUT/$name-fail-$i.log"
    fi
    [ $i = 1 ] && cp "$LOG" "$OUT/$name-first.log"
    i=$((i+1))
  done
  echo "  soak: $name — $fails of $N boots failed ($(( $(date +%s) - t0 )) s)"
  echo "$name $fails $N" >> "$OUT/summary.txt"
done
[ -f "$OUT/summary.txt" ] && ! awk '$2 != 0 { bad = 1 } END { exit !bad }' "$OUT/summary.txt"
