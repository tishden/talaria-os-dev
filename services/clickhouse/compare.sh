#!/bin/sh
# ClickHouse comparison: Talaria against Linux on ONE query set.
#
# The protocol (without it the numbers mean nothing — the run-to-run spread reaches
# 2×): every query is repeated three times in the file, there are several runs, and
# the MEDIAN of all measurements is taken. Logs are off: serial output costs
# milliseconds per write, hundreds of them add up per run — otherwise the console is measured.
#   compare.sh [runs]     env: SMP=N
set -eu
cd "$(dirname "$0")"
RUNS=${1:-2}; SMP=${SMP:-4}
W=$(mktemp -d)
# Talaria: the set goes into the image, the manifest is switched to measuring mode
# (`--time` prints the query time, logs are off: serial output costs milliseconds
# per write, otherwise the speed of the console is measured, not of the queries).
# SPACE IN THE IMAGE — BEFORE THE RUN. The image is persistent, ClickHouse's DROP
# TABLE in an Atomic database is lazy (the parts stay in store/ for 8 more minutes),
# and a run killed early cleans up nothing at all. A few runs in a row, and INSERT
# fails with `Code: 243 NOT_ENOUGH_SPACE`: the table part of the set is not executed,
# the run yields 24 measurements instead of 34 and SILENTLY drops out of the median
# (that is how variants were once "measured" on a full disk). The only cure is a
# fresh image (`mkdisk.sh`), and that must be said in words, not by a table of two
# runs instead of three.
FREE_MB=$(debugfs -R stats disk-ch.img 2>/dev/null | awk '/^Free blocks:/{f=$3} /^Block size:/{b=$3} END{print int(f*b/1048576)}')
if [ "${FREE_MB:-0}" -lt 300 ]; then
  echo "disk-ch.img has ${FREE_MB} MB free — less than 300: an INSERT of 10 M rows will not fit (NOT_ENOUGH_SPACE)."
  echo "Recreate the image: services/clickhouse/mkdisk.sh (parts of dropped tables pile up in store/)."
  exit 1
fi
printf 'rm /q.sql\nwrite %s/bench.sql q.sql\nq\n' "$PWD" | debugfs -w disk-ch.img >/dev/null 2>&1
M=$(mktemp)
debugfs -R "cat /cell.manifest" disk-ch.img 2>/dev/null \
  | sed 's|--queries-file /q.sql$|--queries-file /q.sql --time --send_logs_level=none|' > "$M"
# The check is mandatory: if the manifest could not be read, writing an EMPTY file
# would leave the image without an entry point, and the next run would silently do nothing.
if ! grep -q '^binary=' "$M"; then echo "could not read /cell.manifest from the image"; rm -f "$M"; exit 1; fi
printf 'rm /cell.manifest\nwrite %s cell.manifest\nq\n' "$M" | debugfs -w disk-ch.img >/dev/null 2>&1
rm -f "$M"
: > "$W/tal.txt"
i=0; while [ $i -lt "$RUNS" ]; do i=$((i+1))
  SMP=$SMP ./run.sh 300 >/dev/null 2>&1
  tr -d "\r" < build/serial.log | grep -aE "^[0-9]+\.[0-9]+$" > "$W/tal.$i" || echo "  Talaria run $i has no measurements (see build/serial.log)"
  # THE CAUSE, NOT ONLY THE FACT: an incomplete run does not enter the median, and
  # that must be said together with the workload's last error (as in sweep.sh).
  n=$(wc -l < "$W/tal.$i")
  if [ "$n" != 34 ]; then
    echo "  Talaria run $i: $n measurements instead of 34 — DISCARDED; workload: $(tr -d "\r" < build/serial.log | grep -aoE "Code: [0-9]+\. DB::Exception: [^(]*" | tail -1)"
  fi
  cat "$W/tal.$i" >> "$W/tal.txt"
done
# Linux runs IN THE SAME KIND OF VIRTUAL MACHINE (run-linux-vm.sh): the same QEMU/KVM,
# as many cores and as much memory, the same binary, config and query set.
# Otherwise the comparison is unfair in our favour: under a hypervisor a TLB miss
# walks the nested tables, and Linux on bare hardware does not pay for that.
: > "$W/lin.txt"
i=0; while [ $i -lt "$RUNS" ]; do i=$((i+1))
  KEEP_IR=$([ $i -gt 1 ] && echo 1 || echo 0) SMP=$SMP ./run-linux-vm.sh 300 >/dev/null 2>&1
  tr -d "\r" < build/serial-linux-vm.log | grep -aE "^[0-9]+\.[0-9]+$" > "$W/lin.$i" || echo "  Linux VM run $i has no measurements"
  n=$(wc -l < "$W/lin.$i")
  [ "$n" = 34 ] || echo "  Linux VM run $i: $n measurements instead of 34 — DISCARDED"
  cat "$W/lin.$i" >> "$W/lin.txt"
done
python3 - "$W/tal.txt" "$W/lin.txt" <<'PY'
import sys, statistics
NAMES = [("aggregation count+sum, 300 M", [0,1,2]), ("GROUP BY 7 keys, 60 M", [3,4,5]),
         ("GROUP BY 100 k keys, 16 M", [6,7,8]), ("arithmetic sum(x*x), 120 M", [9,10,11]),
         ("uniqExact 500 k, 24 M", [12,13,14]), ("strings concat+length, 12 M", [15,16,17]),
         ("sorting ORDER BY hash, 50 M", [18,19,20]), ("DROP/CREATE MergeTree", [21,22]),
         ("INSERT 10 M into MergeTree", [23]), ("reading the table count+sum", [24,25,26]),
         ("GROUP BY on the table", [27,28,29]), ("LIKE filter on the table", [30,31,32])]
# The 34th measurement is the final `DROP TABLE`: not a test case but MEASUREMENT HYGIENE.
# The Linux guest lives in an initramfs and starts every run with an empty volume, while
# our image is persistent: without an explicit DROP the table accumulates parts between
# runs, and the ClickHouse loader sooner or later refuses to accept them ("Cannot check
# modification time" was seen on an empty part left by an earlier TRUNCATE — although
# both e2fsck and our own `stat` see that part). The sides must be compared in the SAME
# state, so both now start empty.
def load(p):
    v=[float(x) for x in open(p)]
    return [v[i:i+34] for i in range(0,len(v),34) if len(v[i:i+34])==34]
t,l = load(sys.argv[1]), load(sys.argv[2])
print(f"runs: Talaria {len(t)}, Linux {len(l)}")
print(f"{'test case':<36}{'Talaria, ms':>12}{'Linux VM, ms':>13}{'ratio':>8}")
for name, idx in NAMES:
    tv=statistics.median([r[i] for r in t for i in idx])*1000
    lv=statistics.median([r[i] for r in l for i in idx])*1000
    print(f"{name:<36}{tv:>12.1f}{lv:>11.1f}{tv/lv if lv else 0:>8.2f}")
PY
rm -rf "$W"
