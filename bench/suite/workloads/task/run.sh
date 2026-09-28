#!/bin/sh
# The "Low-latency data transfer" challenge (~/sources/task) as a workload of the suite.
#
# This is NOT a client-server test case, so the tlgen generator has nothing to do with it:
# the ends (producer/consumer) come from the challenge itself and it also takes the timestamps.
# Our part is the "middle": sender/receiver, the same static musl binaries,
# running either on Talaria or on Linux, in two VMs on one host
# (bench/task-harness/run_case_vm.sh).
#
#   run.sh <talaria|linux> <results directory> [label]
#   env: RATE=500000 LOSS=1 LOSS_BURST=8 FEC_R=2 FEC_D=1 COUNT=... REPEAT=3
#
# The default configuration is the one that was asked for: fecnack mode, redundancy
# r2d1, losses in bursts of 8, half a million messages per second, one receiver.
set -eu
STAND=${1:?stand}
OUT=${2:?directory}
ONLY=${3:-}
HERE=$(cd "$(dirname "$0")" && pwd)
SUITE=$(cd "$HERE/../.." && pwd)
REPO=$(cd "$SUITE/../.." && pwd)
TASK=${TASK:-$HOME/sources/task}

RATE=${RATE:-500000}
LOSS=${LOSS:-1}
LOSS_BURST=${LOSS_BURST:-8}
FEC_R=${FEC_R:-2}
FEC_D=${FEC_D:-1}
COUNT=${COUNT:-$((RATE * 8))}
REPEAT=${REPEAT:-3}
MODE=${MODE:-fecnack}
LABEL=${LABEL:-fecnack_r${FEC_R}d${FEC_D}_b${LOSS_BURST}_${RATE}}
[ -z "$ONLY" ] || [ "$ONLY" = "$LABEL" ] || { echo "   $LABEL: skipped"; exit 0; }

[ -x "$TASK/transport/bin-musl/sender" ] || { echo "no musl build of sender — run bench/task-harness/build_musl.sh"; exit 1; }
[ -x "$TASK/harness/bin/producer" ]      || { echo "no producer — run make -C $TASK/harness"; exit 1; }

mkdir -p "$OUT"
r=1
while [ "$r" -le "$REPEAT" ]; do
    pfx="$OUT/.task-$STAND-$LABEL-$r"
    echo "   $STAND/$LABEL repeat $r: rate $RATE, losses $LOSS% in bursts of $LOSS_BURST, $COUNT messages"
    STAND="$STAND" TASK="$TASK" LOSS_BURST="$LOSS_BURST" FEC_R="$FEC_R" FEC_D="$FEC_D" \
        "$REPO/bench/task-harness/run_case_vm.sh" "$RATE" "$LOSS" "$COUNT" mixed "$pfx" "$MODE" \
        > "$pfx.log" 2>&1 || { echo "     the run did not work, tail:"; tail -5 "$pfx.log" | sed 's/^/       /'; r=$((r+1)); continue; }

    # The challenge's report -> the suite's common format. The latencies are taken from the CSV
    # (all samples) rather than from the printed percentiles: repeats can be merged and
    # tails recomputed only from samples.
    python3 - "$pfx" "$STAND" "$LABEL" "$RATE" "$LOSS" "$LOSS_BURST" "$r" "$OUT" "$SUITE" <<'PY'
import os, subprocess, sys
pfx, stand, label, rate, loss, burst, rep, out, suite = sys.argv[1:10]
csv = pfx + ".csv"
report = pfx + ".report"
lat = []
if os.path.exists(csv):
    with open(csv) as f:
        head = f.readline().strip().split(",")
        try:
            col = head.index("latency_ns")
        except ValueError:
            col = len(head) - 1
        for line in f:
            p = line.rstrip("\n").split(",")
            if len(p) > col:
                try: lat.append(p[col])
                except Exception: pass
counts = {}
if os.path.exists(report):
    for line in open(report):
        if ":" in line:
            k, v = [x.strip() for x in line.split(":", 1)]
            if k == "received": counts["done"] = int(v)
            elif k == "expected": counts["sent"] = int(v)
            elif k == "dropped": counts["lost"] = int(v)
if not lat:
    print("     no latency samples (%s) — skipping" % csv); sys.exit(0)
args = [sys.executable, os.path.join(suite, "lib", "mkresult.py"),
        os.path.join(out, "%s-%s-r%s-%s.json" % (stand, label, rate, rep)),
        "--name", "task/%s" % label, "--stand", stand, "--op", "fecnack",
        "--unit", "ns", "--throughput", rate,
        "--tags", "workload=task,case=%s,rate=%s,loss=%s,burst=%s,repeat=%s" % (label, rate, loss, burst, rep),
        "--counts", ",".join("%s=%s" % kv for kv in counts.items())]
p = subprocess.run(args, input="\n".join(lat), text=True, capture_output=True)
print("    " + (p.stdout.strip() or p.stderr.strip()))
PY
    r=$((r + 1))
done
