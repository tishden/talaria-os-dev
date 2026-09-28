#!/bin/sh
# The actor model (SObjectizer-5) as a workload of the suite.
#
# The test case is in-process: it measures message delivery and the wakeup of an
# actor thread, there is no network. The same static musl binary runs on
# two stands under IDENTICAL VM discipline (QEMU/KVM, -smp 4, vCPUs pinned to
# their physical cores) — see services/actors/README.md.
#
#   run.sh <talaria|linux> <results directory> [scenario label]
#
# A test case label is "scenario/dispatcher/placement": the actor model has no single
# number, it has a load shape, and on different shapes the sides swap
# places. We show them all.
set -eu
STAND=${1:?stand}
OUT=${2:?directory}
ONLY=${3:-}
HERE=$(cd "$(dirname "$0")" && pwd)
SUITE=$(cd "$HERE/../.." && pwd)
REPO=$(cd "$SUITE/../.." && pwd)
A="$REPO/services/actors"
mkdir -p "$OUT"

[ -x "$A/build/actor-bench" ] || { echo "no services/actors/build/actor-bench — run services/actors/build.sh"; exit 1; }

echo "   actors/$STAND: the scenario matrix (each one is a separate VM boot)"
"$A/matrix.sh" "$STAND" > "$OUT/.actors-$STAND.log" 2>&1 || true
SRC="$A/build/matrix-$STAND.txt"
[ -f "$SRC" ] || SRC="$A/build/results-$STAND.txt"
[ -f "$SRC" ] || { echo "   no results ($SRC)"; exit 1; }
cp "$SRC" "$OUT/.actors-$STAND.txt"

python3 - "$OUT" "$STAND" "$SUITE" "$ONLY" <<'PY'
import os, subprocess, sys
out, stand, suite, only = sys.argv[1:5]
src = os.path.join(out, ".actors-%s.txt" % stand)
for line in open(src, encoding="utf-8", errors="replace"):
    if not line.startswith("RESULT "):
        continue
    kv = dict(p.split("=", 1) for p in line.split()[1:] if "=" in p)
    label = "%s_%s_%s" % (kv.get("scen", "?"), kv.get("disp", "?"), kv.get("pin", "?"))
    if only and only != label:
        continue
    # actor-bench gives ready percentiles in nanoseconds; there are no raw samples.
    pcts = "n=%s,p50=%s,p90=%s,p99=%s,p99_9=%s,max=%s" % (
        kv.get("n", 0), kv.get("p50", 0), kv.get("p90", 0),
        kv.get("p99", 0), kv.get("p999", 0), kv.get("max", 0))
    args = [sys.executable, os.path.join(suite, "lib", "mkresult.py"),
            os.path.join(out, "%s-%s-r0-1.json" % (stand, label)),
            "--name", "actors/%s" % label, "--stand", stand,
            "--op", kv.get("scen", "?"), "--unit", "ns",
            "--throughput", kv.get("thr", "0"), "--wall", kv.get("secs", "0"),
            "--pcts", pcts,
            "--counts", "done=%s,sent=%s" % (kv.get("n", 0), kv.get("n", 0)),
            "--tags", "workload=actors,case=%s,disp=%s,pin=%s" % (label, kv.get("disp", "?"), kv.get("pin", "?"))]
    pr = subprocess.run(args, input="", text=True, capture_output=True)
    print("    " + (pr.stdout.strip() or pr.stderr.strip()))
PY
