#!/bin/sh
# THE SCALING LADDER — the instrument that judges parallel reading.
#
# WHY A SEPARATE INSTRUMENT. Two attempts at this gap in a row failed for the same
# reason: the change turned out SMALLER THAN THE NOISE OF THE SET. The set
# (`compare.sh`, `sweep.sh`) does not suit this question for three reasons at once:
#   * it measures ABSOLUTE milliseconds, which on this machine differ between runs by
#     up to 2× — they can only be compared back to back, and even that is arguable;
#   * its table lives ON DISK, so the read path, the block cache and the persistent
#     image enter the measurement (the `NOT_ENOUGH_SPACE` trap);
#   * it runs ONE configuration (4 cores), while the disease is exactly in the
#     DEPENDENCE on the number of cores: the third core gives us nothing, the fourth does.
#
# WHAT THIS ONE MEASURES. One and the same query over a table IN MEMORY (`ENGINE =
# Memory`: no files, no decompression, no marks) on a ladder of 1..4 cores, with EQUAL
# numbers of cores and threads — the configuration where the collapse was seen. The
# main number is not milliseconds but the SPEEDUP within one side (T1/TN): it does not
# depend on whether the machine is fast or slow today, so it is comparable between
# sessions. The spread is printed next to it, and a point where the difference of the
# sides is smaller than the sum of the spreads is NOT JUDGED (no winner by lot).
#
# NEXT TO THE OUTCOME — THE MECHANISM. A gate on a ratio of two numbers cannot tell
# "the change did not help" from "the change did not engage": the per-core tick
# layout and the number of migrations on wakeup are printed. Without them "no gain"
# and "the mechanism never switched on" look the same (this already bit us with large pages).
#
#   scaling.sh [runs] [talaria.* parameters]    env: SIDES=both|talaria|linux
#                                                   CORES="1 2 3 4" MEM=MB
#   scaling.sh 3 "talaria.wakebal=2"           # a variant against Linux
#   SIDES=talaria scaling.sh 3 "|talaria.wakebal=2|talaria.wakebal=3"
#                                              # variants against each other
set -eu
cd "$(dirname "$0")"
RUNS=${1:-3}; VARIANTS=${2:-}
SIDES=${SIDES:-both}; CORES=${CORES:-"1 2 3 4"}; MEM=${MEM:-3072}
ROWS=${ROWS:-10000000}; REP=${REP:-15}; NREP=${NREP:-5}; NROWS=${NROWS:-400000000}
SECS=${SECS:-180}
W=$(mktemp -d); trap 'rm -rf "$W"' EXIT

# The set for point N. Every measured query carries its own `max_threads`, so the
# ladder over threads and the ladder over cores are set by ONE parameter and cannot
# diverge.
mkq() { # $1 = N, $2 = file
  N=$1; F=$2
  {
    echo "DROP TABLE IF EXISTS m;"
    echo "CREATE TABLE m (id UInt64, k UInt32, s String) ENGINE = Memory;"
    echo "INSERT INTO m SELECT number, number % 100, concat('s', toString(number % 1000)) FROM numbers($ROWS);"
    # warm-up: the first query on a fresh table pays for placing the pool threads
    echo "SELECT count() FROM m WHERE s LIKE 's1%' SETTINGS max_threads = $N;"
    i=0; while [ $i -lt "$REP" ]; do i=$((i+1))
      echo "SELECT count() FROM m WHERE s LIKE 's1%' SETTINGS max_threads = $N;"
    done
    # CONTROL: pure multi-threaded counting without a single read and almost no memory.
    # If it sags too, the scheduler in general is to blame, not table reading.
    i=0; while [ $i -lt "$NREP" ]; do i=$((i+1))
      echo "SELECT count(), sum(number) FROM numbers_mt($NROWS) SETTINGS max_threads = $N;"
    done
    echo "DROP TABLE m;"
  } > "$F"
}
NQ=$((1 + 1 + 1 + 1 + REP + NREP + 1))   # how many times --time must print

put_q() { # put the set into the Talaria image (the Linux guest takes it from there too)
  printf 'rm /q.sql\nwrite %s q.sql\nq\n' "$1" | debugfs -w disk-ch.img >/dev/null 2>&1
}

# One probe: side, number of cores, parameter variant, repeat number.
probe() { # $1=side $2=N $3=extra $4=i  -> writes $W/t.<side>.<N>.<v>.<i>
  side=$1; N=$2; extra=$3; i=$4; tag=$5
  if [ "$side" = talaria ]; then
    SMP=$N MEM=$MEM ./run.sh "$SECS" "$extra" >/dev/null 2>&1
    L=build/serial.log
  else
    # The initramfs CARRIES /q.sql. It cannot be reused between the steps of the
    # ladder: every step has its own set (`max_threads`), and the guest would
    # silently measure SOMEONE ELSE's step — exactly the disease that already cost a
    # session ("readiness was read from an uncleared log of the previous run").
    # Rebuild on the first repeat of EVERY step, reuse after that.
    KEEP_IR=$([ "$i" -gt 1 ] && echo 1 || echo 0) SMP=$N MEM=$MEM \
      ./run-linux-vm.sh "$SECS" >/dev/null 2>&1
    L=build/serial-linux-vm.log
  fi
  tr -d "\r" < "$L" | grep -aE "^[0-9]+\.[0-9]+$" > "$W/t.$side.$N.$tag.$i" || true
  tr -d "\r" < "$L" | grep -aoE "Code: [0-9]+\. DB::Exception: [^(]*" | tail -1 > "$W/e.$side.$N.$tag.$i" || true
  if [ "$side" = talaria ]; then
    # THE MECHANISM, NOT ONLY THE OUTCOME: the distribution of work over cores and migrations.
    #
    # THE LAYOUT IS TAKEN IN CYCLES, NOT TICKS. A tick is a poor unit of work: the tick
    # period is NOT ONE PER MACHINE (on a core with a network waiter it is five times
    # shorter, see `smp::want_period_ns`), and a core with a short period looks twice
    # as "busy" as its neighbour under the same load. Cycles do not suffer from this.
    tr -d "\r" < "$L" | grep -aoE "^stat: core [0-9]+: cycles [0-9]+" | sed 's/.*cycles //' > "$W/c.$N.$tag.$i" || true
    # BUSY CORES AGAINST READY TASKS — a direct answer to "nowhere to put it or
    # nothing to put" (`talaria.runhist=1`).
    tr -d "\r" < "$L" | grep -a "ready tasks at a tick\|CORES busy under them" > "$W/r.$N.$tag.$i" || true
    tr -d "\r" < "$L" | grep -ao "transplants at a wakeup=[0-9]*" | tail -1 > "$W/m.$N.$tag.$i" || true
    # LARGE PAGES next to it: "did not help" and "never handed out" must look
    # different — this already bit us twice.
    tr -d "\r" < "$L" | grep -aoE "2 MB huge pages=[0-9]+ \(reservations [0-9]+, broken up [0-9]+" | tail -1 > "$W/h.$N.$tag.$i" || true
  fi
}

echo "ladder: cores/threads [$CORES], repeats $RUNS, rows $ROWS, LIKE measurements $REP, control numbers_mt $NREP"
vi=0
echo "${VARIANTS:-}" | tr '|' '\n' | while IFS= read -r v; do
  vi=$((vi + 1)); echo "$v" > "$W/vname.$vi"
  for N in $CORES; do
    mkq "$N" "$W/q.sql"; put_q "$W/q.sql"
    i=0; while [ $i -lt "$RUNS" ]; do i=$((i+1))
      echo "  Talaria [${v:-defaults}] cores $N, run $i"
      probe talaria "$N" "$v" "$i" "$vi"
    done
  done
done
if [ "$SIDES" != talaria ]; then
  for N in $CORES; do
    mkq "$N" "$W/q.sql"; put_q "$W/q.sql"
    i=0; while [ $i -lt "$RUNS" ]; do i=$((i+1))
      echo "  Linux cores $N, run $i"
      probe linux "$N" "" "$i" 0
    done
  done
fi

NQ=$NQ REP=$REP NREP=$NREP python3 - "$W" "$SIDES" "$CORES" <<'PY'
import sys, os, glob, statistics
W, SIDES, CORES = sys.argv[1], sys.argv[2], sys.argv[3].split()
NQ, REP, NREP = int(os.environ["NQ"]), int(os.environ["REP"]), int(os.environ["NREP"])
LIKE = list(range(4, 4 + REP))                 # the warm-up (3) is not counted
NUMB = list(range(4 + REP, 4 + REP + NREP))
def runs(side, N, tag):
    """A RUN IS THE UNIT OF DISCARDING, and it names the cause from the workload's log."""
    out = []
    for p in sorted(glob.glob(os.path.join(W, f"t.{side}.{N}.{tag}.*"))):
        v = [float(x) for x in open(p)]
        if len(v) == NQ: out.append(v)
        else:
            ep = p.replace("/t.", "/e.")
            why = open(ep).read().strip() if os.path.exists(ep) else ""
            print(f"  DISCARDED {os.path.basename(p)}: {len(v)} measurements, expected {NQ}" + (f" — {why}" if why else ""))
    return out
def stat(rs, idx):
    """the median and the SPREAD (between runs): without it a ratio reads as the truth."""
    if not rs: return (float('nan'), float('nan'))
    per = [statistics.median([r[i] for i in idx]) * 1000 for r in rs]
    return (statistics.median(per), max(per) - min(per) if len(per) > 1 else 0.0)
vnames = {}
for p in glob.glob(os.path.join(W, "vname.*")):
    vnames[int(p.rsplit(".", 1)[1])] = (open(p).read().strip() or "defaults")
sides = [("talaria", k, vnames[k]) for k in sorted(vnames)]
if SIDES != "talaria": sides.append(("linux", 0, "Linux VM"))
for what, idx in (("LIKE on an in-memory table", LIKE), ("control: numbers_mt", NUMB)):
    print(f"\n=== {what} ===")
    print(f"{'side':<22}" + "".join(f"{'N='+n:>18}" for n in CORES) + f"{'speedup 1->max':>18}")
    tab = {}
    for side, tag, name in sides:
        cells, sp = [], None
        base = None
        for n in CORES:
            m, d = stat(runs(side, n, tag), idx)
            tab[(side, tag, n)] = (m, d)
            if base is None: base = m
            cells.append(f"{m:>10.1f}±{d:<6.1f}")
        last = tab[(side, tag, CORES[-1])][0]
        sp = base / last if last == last and last else float('nan')
        print(f"{name:<22}" + "".join(cells) + f"{sp:>17.2f}x")
    # THE SPEEDUP PER STEP — what the instrument was written for: the disease was not
    # "slow in general" but "a particular step gives nothing".
    print(f"{'speedup vs N=1':<22}")
    for side, tag, name in sides:
        b = tab[(side, tag, CORES[0])][0]
        print(f"  {name:<20}" + "".join(f"{(b/tab[(side,tag,n)][0]):>17.2f}x" for n in CORES))
    if SIDES != "talaria" and len(sides) > 1:
        print(f"{'verdict per step':<22}")
        for side, tag, name in sides[:-1]:
            row = ""
            for n in CORES:
                t, td = tab[(side, tag, n)]; l, ld = tab[("linux", 0, n)]
                # A DIFFERENCE SMALLER THAN THE SUM OF THE SPREADS — THE POINT IS NOT JUDGED.
                row += f"{'not judged':>18}" if abs(t - l) < td + ld else f"{t/l:>17.2f}x"
            print(f"  {name:<20}{row}")
for side, tag, name in sides:
    if side != "talaria": continue
    for n in CORES:
        cs = sorted(glob.glob(os.path.join(W, f"c.{n}.{tag}.*")))
        if not cs: continue
        busy = []
        for line in open(cs[-1]):
            try: busy.append(int(line.strip()) // 1000000)   # millions of cycles
            except Exception: pass
        mv = ""
        mp = os.path.join(W, f"m.{n}.{tag}.{len(cs)}")
        for p in sorted(glob.glob(os.path.join(W, f"m.{n}.{tag}.*")))[-1:]: mv = open(p).read().strip()
        hg = ""
        for p in sorted(glob.glob(os.path.join(W, f"h.{n}.{tag}.*")))[-1:]: hg = open(p).read().strip()
        if busy:
            print(f"  [{name}] N={n}: millions of cycles per core {busy} (min/max {min(busy)/max(max(busy),1):.2f}), {mv}")
        for p in sorted(glob.glob(os.path.join(W, f"r.{n}.{tag}.*")))[-1:]:
            for line in open(p): print(f"  [{name}] N={n}: {line.strip()}")
        if hg:
            print(f"  [{name}] N={n}: {hg})")
PY
