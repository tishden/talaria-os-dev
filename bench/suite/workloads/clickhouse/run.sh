#!/bin/sh
# ClickHouse as a workload of the suite: QUERY TIME, not queries per second.
#
# The test case here is different in nature: an analytical engine is measured by the time
# of a heavy query, not by a protocol round trip, and there is no client-server network in it
# at all (`clickhouse local` reads a set of queries from disk and prints the time of
# each). So the tlgen generator takes no part, and the result is brought to the
# common format (lib/mkresult.py) separately for each query class.
#
#   run.sh <talaria|linux> <results directory> [label]
#   env: REPEAT=3 SMP=4
#
# The discipline is the same as for the rest: both sides in an IDENTICAL VM, the same
# binary, the same query set, logs off (output to serial costs
# milliseconds per write, and otherwise the speed of the console is measured).
#
# THE QUERY SIZE IS SET BY THE MEASUREMENT QUANTUM. `--time` prints seconds with three
# decimals, i.e. a step of exactly 1 ms. The previous set fitted into 5-40 ms per
# test case — and half the points measured rounding rather than the system: `like` 10 against
# 7 ms and `groupby_tbl` 5 against 4 are indistinguishable from the quantum, and the rule "if the
# repeats disagree the point is not judged" removed them in batches (57 points per run).
# Now each test case takes 150-600 ms: the quantum became a fraction of a percent, and the difference
# between the sides reads directly. The absolute numbers are NOT COMPARABLE with runs
# from before September 2026 — only Talaria against Linux WITHIN one run may be compared.
set -eu
STAND=${1:?stand}
OUT=${2:?directory}
ONLY=${3:-}
HERE=$(cd "$(dirname "$0")" && pwd)
SUITE=$(cd "$HERE/../.." && pwd)
REPO=$(cd "$SUITE/../.." && pwd)
CH="$REPO/services/clickhouse"
REPEAT=${REPEAT:-3}
SMP=${SMP:-4}
mkdir -p "$OUT"

[ -f "$CH/disk-ch.img" ] || { echo "no $CH/disk-ch.img — run services/clickhouse/mkdisk.sh"; exit 1; }

# THE IMAGE IS PREPARED FOR MEASUREMENT, not for a demonstration: the query set is put into
# the image, and the manifest is switched to measurement mode (`--time` prints the query
# time; logs are turned off — output to serial costs milliseconds per write, and
# otherwise the speed of the console is measured, not of the queries). Without this a run passes
# successfully and produces NOT A SINGLE measurement — which is exactly how it went the first time.
# The Linux side (run-linux-vm.sh) gets the same flags through the cmdline.
if [ "$STAND" = talaria ]; then
    printf 'rm /q.sql\nwrite %s/bench.sql q.sql\nq\n' "$CH" | debugfs -w "$CH/disk-ch.img" >/dev/null 2>&1
    M=$(mktemp)
    debugfs -R "cat /cell.manifest" "$CH/disk-ch.img" 2>/dev/null \
        | sed 's|--queries-file /q.sql$|--queries-file /q.sql --time --send_logs_level=none|' > "$M"
    # The check is mandatory: writing an EMPTY manifest would leave the image without an
    # entry point, and the next run would silently do nothing.
    if ! grep -q '^binary=' "$M"; then echo "the /cell.manifest could not be read from the image"; rm -f "$M"; exit 1; fi
    printf 'rm /cell.manifest\nwrite %s cell.manifest\nq\n' "$M" | debugfs -w "$CH/disk-ch.img" >/dev/null 2>&1
    rm -f "$M"
fi

r=1
while [ "$r" -le "$REPEAT" ]; do
    echo "   clickhouse/$STAND repeat $r"
    if [ "$STAND" = talaria ]; then
        SMP=$SMP "$CH/run.sh" 300 >/dev/null 2>&1 || true
        LOG="$CH/build/serial.log"
    else
        KEEP_IR=$([ "$r" -gt 1 ] && echo 1 || echo 0) SMP=$SMP "$CH/run-linux-vm.sh" 300 >/dev/null 2>&1 || true
        LOG="$CH/build/serial-linux-vm.log"
    fi
    cp "$LOG" "$OUT/.ch-$STAND-$r.log" 2>/dev/null || true
    r=$((r + 1))
done

# Measurements: `--time` prints the query time on its own line. The test cases are
# query CLASSES (see compare.sh); each is repeated three times in the set.
python3 - "$OUT" "$STAND" "$REPEAT" "$SUITE" <<'PY'
import os, re, subprocess, sys
out, stand, repeat, suite = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
CLASSES = [("agg300m", "count+sum aggregation, 300 million", [0, 1, 2]),
           ("groupby7", "GROUP BY 7 keys, 60 million", [3, 4, 5]),
           ("groupby100k", "GROUP BY 100 thousand keys, 16 million", [6, 7, 8]),
           ("arith", "arithmetic sum(x*x), 120 million", [9, 10, 11]),
           ("uniq", "uniqExact 500 thousand, 24 million", [12, 13, 14]),
           ("strings", "strings concat+length, 12 million", [15, 16, 17]),
           ("sort", "sorting ORDER BY hash, 50 million", [18, 19, 20]),
           ("ddl", "DROP/CREATE MergeTree", [21, 22]),
           ("insert10m", "INSERT of 10 million into MergeTree", [23]),
           ("scan", "table scan count+sum", [24, 25, 26]),
           ("groupby_tbl", "GROUP BY over the table", [27, 28, 29]),
           ("like", "a LIKE filter over the table", [30, 31, 32])]
per_class = {k: [] for k, _, _ in CLASSES}
for r in range(1, repeat + 1):
    p = os.path.join(out, ".ch-%s-%d.log" % (stand, r))
    if not os.path.exists(p):
        continue
    vals = [float(m) for m in re.findall(r"^([0-9]+\.[0-9]+)$",
            open(p, "rb").read().decode("utf-8", "replace").replace("\r", ""), re.M)]
    for key, _, idx in CLASSES:
        for i in idx:
            if i < len(vals):
                per_class[key].append(vals[i] * 1e3)   # seconds -> milliseconds
for key, human, _ in CLASSES:
    v = per_class[key]
    if not v:
        print("     %s: no measurements" % key)
        continue
    args = [sys.executable, os.path.join(suite, "lib", "mkresult.py"),
            os.path.join(out, "%s-%s-r0-1.json" % (stand, key)),
            "--name", "clickhouse/%s" % key, "--stand", stand, "--op", "query",
            "--unit", "ms", "--wall", "0",
            "--tags", "workload=clickhouse,case=%s,description=%s" % (key, human.replace(",", " "))]
    pr = subprocess.run(args, input="\n".join("%.6f" % x for x in v), text=True, capture_output=True)
    print("    " + (pr.stdout.strip() or pr.stderr.strip()))
PY
