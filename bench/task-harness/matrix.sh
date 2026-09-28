#!/bin/bash
# The matrix: stands {talaria,linux} (two VMs) × modes {base/0%, nack/1%, fecnack/1%} × REPEATS,
# plus the solution's Linux loopback baseline (one host, transport/bench/run_case.sh). Results go to OUTDIR,
# the summary to summary.py. Usage: matrix.sh [OUTDIR] ; env RATE=200000 COUNT=400000 REPEATS=2 STANDS="talaria linux" MODES="base nack fecnack"
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
TASK=${TASK:-$HOME/sources/task}
OUTDIR=${1:-/tmp/talaria-harness-matrix}; mkdir -p "$OUTDIR"
RATE=${RATE:-200000}; COUNT=${COUNT:-400000}; REPEATS=${REPEATS:-2}
STANDS=${STANDS:-"talaria linux"}; MODES=${MODES:-"base nack fecnack"}
loss_of() { case $1 in base) echo 0;; *) echo 1;; esac; }
for r in $(seq 1 "$REPEATS"); do
  for m in $MODES; do
    L=$(loss_of $m)
    if [ "${LOOPBACK:-1}" = 1 ]; then
      ( cd "$TASK" && LOSS_FIXED=${LOSS_FIXED:-3} transport/bench/run_case.sh "$RATE" "$L" "$COUNT" mixed "$OUTDIR/loopback_${m}_r$r" "$m" >/dev/null 2>&1 )
      echo "loopback $m r$r: $(grep -E 'p50|drop_rate' "$OUTDIR/loopback_${m}_r$r.report" | tr -s ' ' | tr '\n' ' ')"
    fi
    for st in $STANDS; do
      STAND=$st LOSS_FIXED=${LOSS_FIXED:-3} "$HERE/run_case_vm.sh" "$RATE" "$L" "$COUNT" mixed "$OUTDIR/${st}_${m}_r$r" "$m" >/dev/null 2>&1
      echo "$st $m r$r: $(grep -E 'p50|drop_rate' "$OUTDIR/${st}_${m}_r$r.report" 2>/dev/null | tr -s ' ' | tr '\n' ' ')"
    done
  done
done
python3 "$HERE/summary.py" "$OUTDIR"
