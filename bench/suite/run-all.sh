#!/bin/sh
# Run the WHOLE suite locally: for each workload, both stands.
#
#   run-all.sh [list of workloads]    by default all that have a meta.env
#   env: the same as run-local.sh (DUR/WARM/CONNS/REPEAT), plus SKIP="..."
#
# The local numbers are for debugging (see the header of run-local.sh): the point of the run is "everything
# comes up, the protocols parse, the reports are assembled", not publication.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
LIST=${*:-}
[ -n "$LIST" ] || LIST=$(cd "$HERE/workloads" && ls -d */ 2>/dev/null | tr -d /)
SKIP=${SKIP:-}
STAMP=$(date +%Y%m%d-%H%M%S)
ALL="$HERE/results/all-$STAMP"
mkdir -p "$ALL"
echo "== a full run of the suite -> $ALL"
: > "$ALL/log.txt"

for wl in $LIST; do
    case " $SKIP " in *" $wl "*) echo "-- $wl: skipped"; continue ;; esac
    [ -f "$HERE/workloads/$wl/meta.env" ] || { echo "-- $wl: no meta.env"; continue; }
    echo "== $wl" | tee -a "$ALL/log.txt"
    t0=$(date +%s)
    if OUTDIR="$ALL/$wl" "$HERE/run-local.sh" "$wl" both >> "$ALL/log.txt" 2>&1; then
        st=OK
    else
        st=ERROR
    fi
    echo "   $wl: $st in $(( $(date +%s) - t0 )) s" | tee -a "$ALL/log.txt"
done

python3 "$HERE/lib/report.py" "$ALL" > "$ALL/report-all.txt" 2>&1 || true
echo "== done: $ALL"
