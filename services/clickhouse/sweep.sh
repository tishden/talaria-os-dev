#!/bin/sh
# A sweep over a kernel PARAMETER: one and the same bench.sql under Talaria with
# different `talaria.*` values, the median over the repeats in the file. Variants are
# separated by a vertical bar; an empty variant means the defaults.
#
#   sweep.sh "|talaria.huge=2|talaria.nosteal=1" [runs]     env: SMP=N
#   sweep.sh "talaria.slice=8000000|talaria.slice=500000" 3
#
# Run it ONLY back to back with the other measurements: the spread between sessions on
# one machine reaches 2× (10.5 ms in the morning and 22 in the evening — the same code).
set -eu
cd "$(dirname "$0")"
VARIANTS=${1:-"|talaria.huge=2"}; RUNS=${2:-2}; SMP=${SMP:-4}
# THE RUN BUDGET IS A PARAMETER. It is not "generous": on four cores the set hit
# 300 s, and the last ten queries (the whole TABLE part) did not make it. The run
# still looked complete — hence discarding by the number of measurements.
SECS=${SECS:-300}
# the set and the manifest in measuring mode — exactly as in compare.sh
printf 'rm /q.sql\nwrite %s/bench.sql q.sql\nq\n' "$PWD" | debugfs -w disk-ch.img >/dev/null 2>&1
M=$(mktemp)
debugfs -R "cat /cell.manifest" disk-ch.img 2>/dev/null \
  | sed 's|--queries-file /q.sql$|--queries-file /q.sql --time --send_logs_level=none|' > "$M"
grep -q '^binary=' "$M" || { echo "could not read /cell.manifest from the image"; rm -f "$M"; exit 1; }
printf 'rm /cell.manifest\nwrite %s cell.manifest\nq\n' "$M" | debugfs -w disk-ch.img >/dev/null 2>&1
rm -f "$M"
W=$(mktemp -d)
n=0
echo "$VARIANTS" | tr '|' '\n' | while IFS= read -r v; do
  n=$((n + 1)); echo "$v" > "$W/name.$n"
  : > "$W/v.$n"
  i=0; while [ $i -lt "$RUNS" ]; do i=$((i+1))
    SMP=$SMP ./run.sh "$SECS" "$v" >/dev/null 2>&1
    # EVERY RUN IN ITS OWN FILE. The measurements of all runs of a variant used to be
    # glued into one, and the analysis cut the stream into pieces of 34 BY POSITION.
    # A run that gave other than 34 numbers (ClickHouse failed on one query, the guest
    # did not come up) was discarded only if it was the LAST one — from the middle it
    # shifted all the following runs by a position, and the table silently started
    # attributing one test case's time to another. It was caught only by impossible
    # numbers (`INSERT` 653 -> 161 ms, `reading the table` 60 -> 217).
    tr -d "\r" < build/serial.log | grep -aE "^[0-9]+\.[0-9]+$" > "$W/v.$n.$i" || echo "  [$v] run without measurements (see build/serial.log)"
    # THE CAUSE, NOT ONLY THE FACT. An incomplete run is not only "slow": the image is
    # persistent and the set ends with `DROP TABLE` — a run that died earlier leaves ten
    # million rows in it. A few such runs in a row, and ClickHouse stops at `Code: 243
    # NOT_ENOUGH_SPACE`, i.e. the table part (exactly what everything is measured for)
    # is not executed AT ALL.
    tr -d "\r" < build/serial.log | grep -aoE "Code: [0-9]+\. DB::Exception: [^(]*" | tail -1 > "$W/err.$n.$i" || true
    # the large-page counters of the last run — otherwise "no gain" and "never handed
    # out" look the same (this has already bitten once)
    tr -d "\r" < build/serial.log | grep -aoE "2 MB huge pages=[0-9]+ \(reservations [0-9]+, broken up [0-9]+[^)]*\)" | tail -1 > "$W/huge.$n" || true
  done
done
python3 - "$W" <<'PY'
import sys, os, statistics, glob
NAMES = [("aggregation count+sum, 10 M", [0,1,2]), ("GROUP BY 7 keys, 2 M", [3,4,5]),
         ("GROUP BY 100 k keys, 2 M", [6,7,8]), ("arithmetic sum(x*x), 5 M", [9,10,11]),
         ("uniqExact 500 k, 2 M", [12,13,14]), ("strings concat+length, 1 M", [15,16,17]),
         ("sorting ORDER BY hash, 2 M", [18,19,20]), ("DROP/CREATE MergeTree", [21,22]),
         ("INSERT 500 k into MergeTree", [23]), ("reading the table count+sum", [24,25,26]),
         ("GROUP BY on the table", [27,28,29]), ("LIKE filter on the table", [30,31,32])]
# The 34th measurement is the final `DROP TABLE`: not a test case but MEASUREMENT HYGIENE.
# The Linux guest lives in an initramfs and starts every run with an empty volume, while
# our image is persistent: without an explicit DROP the table accumulates parts between
# runs, and the ClickHouse loader sooner or later refuses to accept them ("Cannot check
# modification time" was seen on an empty part left by an earlier TRUNCATE — although
# both e2fsck and our own `stat` see that part). The sides must be compared in the SAME
# state, so both now start empty.
W = sys.argv[1]
ks = sorted({int(p.rsplit('/',1)[1].split('.')[1]) for p in glob.glob(os.path.join(W,'v.*'))})
def load(k):
    # A RUN IS THE UNIT OF DISCARDING, not a position in the stream: an incomplete run is
    # thrown out whole and NAMED aloud, instead of shifting its neighbours.
    runs=[]; bad=0
    for p in sorted(glob.glob(os.path.join(W,f"v.{k}.*"))):
        v=[float(x) for x in open(p)]
        if len(v)==34: runs.append(v)
        else:
            bad+=1
            ep=p.replace("/v.","/err.")
            why=open(ep).read().strip() if os.path.exists(ep) else ""
            print(f"  DISCARDED run {os.path.basename(p)}: {len(v)} measurements, expected 34" + (f" — {why}" if why else ""))
    return runs
data={k:load(k) for k in ks}
name={k:(open(os.path.join(W,f"name.{k}")).read().strip() or "defaults") for k in ks}
print("variants: " + ", ".join(f"[{name[k]}] runs {len(data[k])}" for k in ks))
print(f"{'test case':<36}" + "".join(f"{name[k][:11]:>12}" for k in ks))
tot={k:0.0 for k in ks}
for nm, idx in NAMES:
    row=""
    for k in ks:
        r=data[k]
        v=statistics.median([x[i] for x in r for i in idx])*1000 if r else float('nan')
        tot[k]+=v; row+=f"{v:>12.1f}"
    print(f"{nm:<36}{row}")
print(f"{'TOTAL, ms':<36}" + "".join(f"{tot[k]:>12.1f}" for k in ks))
for k in ks:
    h=open(os.path.join(W,f"huge.{k}")).read().strip() if os.path.exists(os.path.join(W,f"huge.{k}")) else ""
    if h: print(f"  [{name[k]}] {h}")
PY
rm -rf "$W"
