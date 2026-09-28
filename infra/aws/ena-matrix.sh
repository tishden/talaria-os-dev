#!/bin/bash
# ena-matrix.sh: run a series of ENA datapath configurations with one command.
#
# Why a separate script: the p50 spread BETWEEN instance launches is ~10 µs —
# comparable with the effect being looked for. So configurations run NOT one by one
# but as a series (REPS passes over the whole list), and the medians of the series
# must be compared, not individual launches. Each run is its own instance from a
# shared AMI (the cmdline tail travels in user-data), so the order within a series
# need not be shuffled: the noise smears over all configurations equally.
#
# Usage:
#   ENVF=~/.talaria-lab/lab.env PEM=~/.talaria-lab/talaria-v11.pem \
#     infra/aws/ena-matrix.sh [type] [packets] [repeats]
#   env: CONFIGS="..." — your own list (one cmdline tail per line)
set -u
TYPE=${1:-c6i.large}
N=${2:-20000}
REPS=${3:-3}
HERE=$(cd "$(dirname "$0")" && pwd)
W=$(dirname "${ENVF:?no ENVF}")
OUT="$W/matrix-$(date +%H%M%S).log"

# Default: the baseline (LLQ + write-combining, as the reference driver) and one
# difference from it per line — so that each line answers exactly one question.
DEFAULT_CONFIGS='talaria.ena=llq
talaria.ena=llq talaria.ena=nowc
talaria.ena=llq talaria.ena=accel
talaria.ena=llq talaria.ena=nocqdb
talaria.ena=llq talaria.ena=intr'
CONFIGS=${CONFIGS:-$DEFAULT_CONFIGS}

echo "== ENA matrix: type $TYPE, $N packets, $REPS repeats; log $OUT"
r=1
while [ "$r" -le "$REPS" ]; do
  # The configuration list is read from descriptor 3, NOT from stdin: ena-rtt.sh
  # goes over ssh, and ssh drains stdin entirely — with a plain `while read` the
  # very first run ate the rest of the list, and the pass stopped after one
  # configuration (that is exactly how it was caught).
  while IFS= read -r cfg <&3; do
    [ -n "$cfg" ] || continue
    echo "--- pass $r: $cfg" | tee -a "$OUT"
    "$HERE/ena-rtt.sh" "$cfg" "$TYPE" "$N" </dev/null 2>&1 | tee -a "$OUT" | grep -aE "p50|txbench:|LLQ|did not come up|error"
  done 3<<EOF
$CONFIGS
EOF
  r=$((r + 1))
done

echo "== summary (p50 per configuration) =="
awk '/^--- pass/ { cfg = substr($0, index($0, ": ") + 2) }
     /p50/ { print cfg " | " $0 }' "$OUT"
