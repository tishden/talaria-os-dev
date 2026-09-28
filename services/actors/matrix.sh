#!/bin/sh
# The actor-bench matrix: every combination is a separate VM boot (hangs are
# isolated; `so` combined-lock may get stuck on Talaria — the runner's timeout survives it).
#
# WHY TWO KINDS OF REPEATS. The matrix varies between runs MORE than the
# difference we look for (one and the same build gave 38/17/14, 41/12/16, 34/19/16),
# so one run proves nothing. But "repeat" can mean two different things, and
# they were not separated before:
#   * a BOOT — a new VM: the vCPUs land on physical cores anew, caches warm up
#     anew, the pool threads are placed anew;
#   * a REPEAT WITHIN A BOOT — the same scenario once more in the same machine.
# The difference between the two spreads says what buys precision. If the noise
# is mostly between boots, extra repeats within a boot are useless, and vice versa.
#
# usage: matrix.sh [talaria|linux|both] [boots] [repeats per boot]
# Results: build/results-{talaria,linux}.txt (RESULT lines with boot= and rep=)
set -u
D=$(cd "$(dirname "$0")" && pwd)
STAND=${1:-both}; BOOTS=${2:-3}; REPS=${3:-3}
# Kernel parameters under test go SIDE BY SIDE (`EXTRA="talaria.stealbal=1"`): the
# matrix is also the regression gate for scheduler changes, and those can only be
# judged with the same instrument in the same window.
EXTRA=${EXTRA:-}
# The boot timeout grows with the number of repeats: a scenario with a 10 s
# deadline repeated five times no longer fits into the old 90 s, and that would
# look like a hang — that is, like a DEFECT that is not there.
TMO=$((60 + 30 * REPS))

# A SUBSET OF THE MATRIX (`ONLY="fanout tsync spread | ring tsync spread"`).
# A scheduler change should be re-checked where it is at risk, not with an hour
# of the full matrix per variant: the analysis goes by SPECIFIC points, and a
# point that collapsed must be re-measured first and quickly.
MATRIX=${ONLY:-"
pingpong so none      | pingpong so spread
pingpong so-simple none | pingpong so-simple spread
pingpong tsync none   | pingpong tsync spread
pingpong raw none     | pingpong raw spread
ring so spread        | ring so-simple spread | ring tsync spread | ring tsync none
fanout so spread      | fanout so-simple spread | fanout tsync spread | fanout tsync none
pipeline so spread    | pipeline so-simple spread | pipeline tsync spread
flood so spread       | flood so-simple spread | flood tsync spread | flood raw spread
"}

run_stand() { # $1 = talaria|linux
  st=$1
  # THE ACCUMULATOR IS A SEPARATE FILE. `run.sh` and `run_linux.sh` each write
  # `results-$st.txt` themselves (through `tee`) on EVERY boot: accumulating into
  # it would wipe everything collected on each combination. It was caught only
  # because the file had lines both with and without `boot=`.
  out="$D/build/matrix-$st.txt"
  : > "$out"
  b=0
  while [ "$b" -lt "$BOOTS" ]; do
    b=$((b + 1))
    echo "$MATRIX" | tr '|' '\n' | while read -r scen disp pin; do
      [ -n "${scen:-}" ] || continue
      echo "== $st boot $b/$BOOTS: $scen $disp $pin x$REPS =="
      if [ "$st" = talaria ]; then
        L=$D/build/serial-talaria.log
        "$D/mkdisk.sh" "$scen" "$disp" "$pin" 0 "$REPS" >/dev/null 2>&1
        "$D/run.sh" $TMO "$EXTRA" >/dev/null 2>&1
      else
        L=$D/build/serial-linux.log
        "$D/run_linux.sh" $TMO "$scen $disp $pin 0 $REPS" >/dev/null 2>&1
      fi
      # boot= in the line itself: without it the repeats of different boots are
      # indistinguishable, and separating them is the whole point of this run.
      if grep -aq "^RESULT" "$L" 2>/dev/null; then
        grep -a "^RESULT" "$L" | sed "s/^RESULT /RESULT boot=$b /" >> "$out"
      else
        echo "HUNG boot=$b scen=$scen disp=$disp pin=$pin" >> "$out"
      fi
      tail -1 "$out"
    done
  done
  { grep -am1 "^MACH" "$D/build/serial-$st.log" 2>/dev/null; cat "$out"; } > "$D/build/results-$st.txt"
}

case "$STAND" in
  talaria) run_stand talaria ;;
  linux)   run_stand linux ;;
  both)    run_stand talaria; run_stand linux ;;
esac
echo "== summary =="
python3 "$D/summary.py"
