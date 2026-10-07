#!/bin/sh
# THE WHOLE SUITE IN ONE COMMAND: six tlgen workloads + five external harnesses + the summary.
#
#   run-everything.sh [harness repeats]     (1 by default)
#
# Why separate from run-all.sh: that one runs only the tlgen workloads, while the decision
# is made from ONE table that also has ClickHouse, the JVM, the actors and
# task. Assembling it by hand every time means forgetting
# one directory out of five every time and comparing the wrong thing.
#
# BEFORE A RUN (hygiene):
#   pgrep -af 'run-all.sh|run-local.sh'   # nothing
#   ps -C tlgen -o pid=,etime=            # NOT ONE (tlgen runs with --spin and eats a core)
#   docker ps --filter label=tlbench=1    # empty
#   make -C bench/suite/tlgen             # the instrument
# And never run the regression and the suite at the same time.
#
# REPEATS. The actor, ClickHouse and JVM harnesses are noisier between runs than
# the difference being looked for in them: three runs of the actor matrix in a row on ONE kernel
# gave 38/17/14, 41/12/16 and 34/19/16, and the JVM total changed from 9/7/9 to 7/13/5 without
# a single change. So they are not judged by one run: the second argument
# sets the number of repeats, and all the directories go into the summary together.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
cd "$REPO"
REP=${1:-1}
STAMP=$(date +%Y%m%d-%H%M%S)
OUT="$HERE/results/everything-$STAMP.md"

echo "== start $(date +%H:%M)"
"$HERE/run-all.sh" mongoose nginx redis postgresql mysql mongodb || echo "!! the tlgen suite finished with an error"
DIRS=$(ls -dt "$HERE"/results/all-* | head -1)
echo "== tlgen done $(date +%H:%M) -> $DIRS"

r=1
while [ "$r" -le "$REP" ]; do
    for w in clickhouse jvm actors task; do
        echo "== $w (repeat $r) $(date +%H:%M)"
        "$HERE/run-local.sh" "$w" both >"$HERE/results/.log-$w-$r.txt" 2>&1 || echo "   $w: ERROR (see results/.log-$w-$r.txt)"
        d=$(ls -dt "$HERE"/results/"$w"-* 2>/dev/null | head -1)
        [ -n "$d" ] && DIRS="$DIRS $d"
    done
    r=$((r + 1))
done

echo "== summarising: $DIRS"
python3 "$HERE/lib/summary.py" $DIRS --md "$OUT"
echo "$DIRS" > "$HERE/results/everything-$STAMP.dirs"
echo "DONE $(date +%H:%M) -> $OUT"
