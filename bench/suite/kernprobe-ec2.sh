#!/bin/sh
# KERNEL KNOBS ON ONE STAND (ARM64-PLAN §12 item 3): phase A as the board takes it (both
# sides), then one phase per cmdline tail on the SERVER over A's ladder, torn down at the end.
#
#   ARCH=arm64 AWS_DEFAULT_REGION=... STAND_DIR=... kernprobe-ec2.sh <workload> "<tail B>" ["<tail C>" ...]
#   env: REUSE=1 (a live stand), KEEP=1 (do not tear down), DUR= REPEAT= (A and the phases),
#        CASES="select1 pk" (the phases' cases), ROLES="server" (whom the tails go to)
#   A tail of "=" repeats the phase without touching the cmdline (the drift check).
# Why a tail per phase: between bring-ups even the median moves (README §3.1); a kernel
# knob is judged only against phase A of its own bring-up. PostgreSQL's ceiling fell from
# 42 k to 25 k req/s between the boards of 2026-10-05 and 2026-10-22 while Linux's stayed —
# the knobs that undo the defaults of that fortnight (talaria.dmap=1 / =0, talaria.uacheck=0)
# are the first bisection, and they need no new AMI.
set -u
WL=${1:?workload}; shift
HERE=$(cd "$(dirname "$0")" && pwd)
export STAND_DIR=${STAND_DIR:-$HOME/.talaria-lab/stand}
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
STAMP=$(date +%Y%m%d-%H%M%S)
ALL="$HERE/results/kernprobe-$([ "${ARCH:-x86_64}" = arm64 ] && echo a64- || true)$WL-$STAMP"
mkdir -p "$ALL"; LOG="$ALL/log.txt"; : > "$LOG"
say() { echo "$*" | tee -a "$LOG"; }
say "== kernprobe -> $ALL (region $AWS_DEFAULT_REGION, stand $STAND_DIR), tails: $*"
t0=$(date +%s)
if [ "${REUSE:-0}" = 1 ] && [ -f "$STAND_DIR/stand.env" ]; then
    say "== REUSE=1: the live stand in $STAND_DIR is used as it is"
else
    if [ -f "$STAND_DIR/stand.env" ]; then say "== a live stand found — tearing it down first"; "$HERE/infra/stand-down.sh" >> "$LOG" 2>&1 || true; fi
    if ! "$HERE/infra/stand-up.sh" "$WL" >> "$LOG" 2>&1; then say "== STAND ERROR (see $STAND_DIR/mk-ami-$WL.log)"; "$HERE/infra/stand-down.sh" >> "$LOG" 2>&1; exit 1; fi
    say "   the stand came up in $(( $(date +%s) - t0 )) s"
fi
say "== A ($(date +%H:%M:%S)): both sides"
OUTDIR="$ALL/A" DUR=${DUR:-30} REPEAT=${REPEAT:-1} "$HERE/run-remote.sh" "$WL" >> "$LOG" 2>&1 || say "   RUN ERROR A"
i=0
for tail in "$@"; do
    i=$((i + 1)); tag="P$i"
    say "== $tag ($(date +%H:%M:%S)): \"$tail\" -> ${ROLES:-server}"
    ROLES=${ROLES:-server} CONSOLE=server DUR=${DUR:-30} REPEAT=${REPEAT:-1} \
        "$HERE/ab-phase.sh" "$ALL" "$tag" "$tail" "$WL" ${CASES:-} >> "$LOG" 2>&1 || say "   RUN ERROR $tag"
    echo "$tag $tail" >> "$ALL/phases.txt"
    python3 "$HERE/lib/compare.py" "$ALL/A" "$ALL/$tag" talaria > "$ALL/compare-$tag.txt" 2>&1 || true
done
if [ "${KEEP:-0}" = 1 ]; then say "== KEEP=1: the stand stays up"; else
    "$HERE/infra/stand-down.sh" >> "$LOG" 2>&1 || say "   CLEANUP ERROR"
fi
say "== done in $(( $(date +%s) - t0 )) s"
for f in "$ALL"/compare-P*.txt; do [ -f "$f" ] && { echo "--- $(basename "$f")"; head -n 30 "$f"; }; done | tee -a "$LOG"
