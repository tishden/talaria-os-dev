#!/bin/sh
# THE WAKEUP PRIMITIVE: Talaria against Linux, ONE AND THE SAME static binary.
#
# Why. Three leftovers of the benchmark set — `actors ring so-simple`, the JVM
# `queue`/`sync reentrant` and "the price of a thread-pool turnaround" in
# ClickHouse — are the same operation: a thread goes to sleep on a futex/condvar,
# another wakes it, and the work continues on ANOTHER core. It cannot be compared
# through the workloads: each has its own framework on top. Here it is measured
# bare — `kernel/user/futexlat` gives four numbers (futex and condvar, same-core
# and cross-core), and both sides get ONE file, one VM discipline and the same vCPU pinning.
#
# An asymmetry that must not be forgotten when reading the numbers: on Talaria the
# workload runs on guest cores 0..1, on Linux too — but under Linux its own
# scheduler stands between the thread and the core. That is the subject of the comparison, not a nuisance.
#
#   compare.sh [repeats]      env: CPU=host,+x2apic
set -eu
D=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$D/../.." && pwd)
REP=${1:-3}
CPUM=${CPU:-host}
K=${KERNEL:-kernel/target/x86_64-talaria/release/talaria}
LK=$REPO/bench/linux-guest/vmlinuz
[ -f "$LK" ] || { echo "no $LK — run bench/linux-guest/mk_guest.sh"; exit 1; }
[ -f "$REPO/kernel/user/futexlat" ] || { echo "no kernel/user/futexlat"; exit 1; }
mkdir -p "$D/build"

# The initramfs for Linux: the same futexlat file that is embedded into the Talaria
# kernel through include_bytes! (its sha256 is printed — check it if the numbers surprise you).
docker run --rm -v "$REPO":/w talaria-alpine-build sh -c \
  'gcc -O2 -static -o /w/bench/syncprim/build/linux-init /w/bench/syncprim/linux-init.c'
IR=$(mktemp -d)
cp "$D/build/linux-init" "$IR/init"; chmod +x "$IR/init"
cp "$REPO/kernel/user/futexlat" "$IR/futexlat"; chmod +x "$IR/futexlat"
mkdir -p "$IR/proc" "$IR/dev"
(cd "$IR" && find . | cpio -o -H newc --quiet | gzip -1) > "$D/build/initramfs-futexlat.gz"
rm -rf "$IR"
echo "binary (both sides): $(sha256sum "$REPO/kernel/user/futexlat" | cut -c1-16)"

: > "$D/build/tal.txt"; : > "$D/build/lin.txt"
i=0
while [ "$i" -lt "$REP" ]; do
    i=$((i + 1))
    docker run --rm -i --device /dev/kvm -v "$REPO":/w talaria-qemu2 bash -s "$K" "$CPUM" <<'INNER' >> "$D/build/tal.txt"
set -u; K=$1; CPUM=$2
rm -f /tmp/o.log
# THE SAME DISCIPLINE AS IN THE ACTOR HARNESS (services/actors/run.sh and run_linux.sh):
# QEMU itself on cores 4-7, vCPU n pinned to physical core n. Otherwise physical cores are
# compared rather than systems, and both sides must get the same layout.
taskset -c 4-7 qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPUM" -smp 2 -m 512 \
  -overcommit cpu-pm=on -kernel "/w/$K" -append "console=ttyS0 talaria.test=futexlat talaria.futexhang=3" \
  -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 &
q=$!
sleep 2
for t in /proc/$q/task/*; do
  c=$(cat "$t/comm" 2>/dev/null)
  case "$c" in "CPU "*) n=${c#CPU }; n=${n%%/*}; taskset -pc "$n" "${t##*/}" >/dev/null 2>&1 ;; esac
done
for s in $(seq 1 60); do sleep 1; grep -qa "FUTEXLAT: ALL OK\|CRASH DUMP" /tmp/o.log && break; done
kill $q 2>/dev/null; wait $q 2>/dev/null
grep -a "^FUTEXLAT " /tmp/o.log || true
INNER
    docker run --rm -i --device /dev/kvm -v "$REPO":/w talaria-qemu2 bash -s <<'INNER' >> "$D/build/lin.txt"
set -u
rm -f /tmp/l.log
taskset -c 4-7 qemu-system-x86_64 -M pc -enable-kvm -cpu host -smp 2 -m 512 \
  -overcommit cpu-pm=on -kernel /w/bench/linux-guest/vmlinuz \
  -initrd /w/bench/syncprim/build/initramfs-futexlat.gz -append "console=ttyS0 quiet" \
  -display none -serial file:/tmp/l.log -no-reboot >/tmp/q.log 2>&1 &
q=$!
# vCPU threads -> their own physical cores (the same discipline as on the Talaria side)
sleep 2
for t in /proc/$q/task/*; do
  c=$(cat "$t/comm" 2>/dev/null)
  case "$c" in "CPU "*) n=${c#CPU }; n=${n%%/*}; taskset -pc "$n" "${t##*/}" >/dev/null 2>&1 ;; esac
done
for s in $(seq 1 60); do sleep 1; grep -qa "FUTEXLAT: ALL OK\|Kernel panic" /tmp/l.log && break; done
kill $q 2>/dev/null; wait $q 2>/dev/null
grep -a "^FUTEXLAT " /tmp/l.log || true
INNER
    echo "  run $i of $REP"
done

python3 - "$D/build/tal.txt" "$D/build/lin.txt" <<'PY'
import re, sys, statistics
def load(p):
    d = {}
    for ln in open(p, encoding="utf-8", errors="replace"):
        m = re.match(r"FUTEXLAT (.+?): RTT p50=(\d+) ns p99=(\d+) ns min=(\d+)", ln.strip())
        if m:
            d.setdefault(m.group(1), []).append((int(m.group(2)), int(m.group(3)), int(m.group(4))))
    return d
t, l = load(sys.argv[1]), load(sys.argv[2])
if not t or not l:
    print("no measurements: Talaria %d phases, Linux %d phases (see build/*.txt)" % (len(t), len(l))); sys.exit(1)
med = lambda v, i: int(statistics.median(x[i] for x in v))
print(f"{'phase':<20}{'Talaria p50':>12}{'Linux p50':>11}{'ratio':>7}{'Talaria p99':>13}{'Linux p99':>11}{'ratio':>7}  runs")
for k in ("futex same-core", "cond same-core", "futex cross-core", "cond cross-core"):
    if k not in t or k not in l: continue
    tp, lp = med(t[k], 0), med(l[k], 0)
    tq, lq = med(t[k], 1), med(l[k], 1)
    print(f"{k:<20}{tp:>10} ns{lp:>9} ns{tp/max(lp,1):>7.2f}{tq:>11} ns{lq:>9} ns{tq/max(lq,1):>7.2f}  {len(t[k])}/{len(l[k])}")
PY
