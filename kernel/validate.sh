#!/bin/sh
# Talaria's heavy validator (B4/B5/B6/B8): the open-source compatibility, the latency SLA,
# the soak, the fuzzing. It is separated from the quick regression test.sh. Run from the host:
#   kernel/validate.sh   (it brings up the docker talaria-qemu2 itself;
#                         TAL_IMAGE= names another stand image)
set -u
# THE GATE NEEDS THE WHOLE LOG RATHER THAN A TAIL. The launchers of the workloads
# (`services/*/run.sh`) print the last N lines — that is enough for a human, while the gate
# looks there for the sums, the results and the exit code. The block of the diagnostics
# `stat:` has grown to fifty lines and pushed them out: a healthy ClickHouse and JVM showed a FAIL. Let them print everything.
export TAIL=1000000
REPO=$(cd "$(dirname "$0")/.." && pwd)
# TAL_KERNEL= a candidate build, as in test.sh (say, one from another target directory).
K=${TAL_KERNEL:-kernel/target/x86_64-talaria/release/talaria}
# The stand image — a knob for the same reason as in test.sh: the image pins the QEMU
# version the numbers were measured against.
IMAGE=${TAL_IMAGE:-talaria-qemu2}
[ -f "$REPO/$K" ] || { echo "there is no kernel $K — build it first"; exit 1; }

( docker run --rm -i --device /dev/kvm --device /dev/net/tun --device /dev/vhost-net \
  --cap-add NET_ADMIN -v "$REPO":/w "$IMAGE" bash -s "$K" <<'INNER'
set -u
K=$1
PASS=0; FAIL=0
ip tuntap add tap0 mode tap 2>/dev/null
ip addr add 10.0.0.1/24 dev tap0 2>/dev/null; ip link set tap0 up 2>/dev/null

boot_disk() { # MODE DISK [net]
  rm -f /tmp/o.log
  if [ "${3:-}" = net ]; then
    qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 2 -m 512 \
      -kernel "/w/$K" -append "console=ttyS0 talaria.mode=$1" -display none -serial file:/tmp/o.log -no-reboot \
      -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
      -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
      -drive id=d0,file=/w/$2,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!
  else
    qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 1 -m 512 \
      -kernel "/w/$K" -append "console=ttyS0 talaria.mode=$1" -display none -serial file:/tmp/o.log -no-reboot \
      -drive id=d0,file=/w/$2,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!
  fi
}
gate() { # PAT NAME
  if grep -qE "$1" /tmp/o.log; then echo "  PASS  $2"; PASS=$((PASS+1)); else echo "  FAIL  $2"; FAIL=$((FAIL+1)); fi
}

echo "== B6: the open-source compatibility (6 services) =="
for spec in \
  "busybox|services/busybox/disk-busybox.img|busybox 1.36.1 on Talaria OS|busybox awk" \
  "lua|services/lua/disk-lua.img|Lua OK on Talaria|lua 5.4" \
  "sqlite|services/sqlite/disk-sqlite.img|SQLite 3.46.1 on Talaria OS|sqlite :memory:" \
  "quickjs|services/quickjs/disk-quickjs.img|QuickJS OK on Talaria|quickjs engine"; do
  mode=$(echo "$spec"|cut -d'|' -f1); disk=$(echo "$spec"|cut -d'|' -f2); pat=$(echo "$spec"|cut -d'|' -f3); nm=$(echo "$spec"|cut -d'|' -f4)
  # WE WAIT FOR A SIGN RATHER THAN FOR A FIXED NUMBER OF SECONDS. A `sleep 11` on a loaded
  # host (the 18th of September: a load average of 25 on eight cores, a build of an AMI was
  # going on next to it) does not live to the service's line, and a healthy busybox shows a
  # FAIL — exactly that "different truth in two runs in a row" because of which the initial
  # point of session 37 was considered incomplete. We wait for the sign for up to 40 s and
  # leave as soon as it is there: on a free machine the gate has become FASTER rather than slower.
  q=$(boot_disk "$mode" "$disk")
  for i in $(seq 1 40); do sleep 1; grep -aq "$pat" /tmp/o.log 2>/dev/null && break; done
  kill $q 2>/dev/null; wait $q 2>/dev/null
  gate "$pat" "opensource: $nm"
done
# mongoose (the network): an HTTP answer
q=$(boot_disk mongoose services/mongoose/disk-mongoose.img net)
for i in $(seq 1 30); do sleep 1; grep -aq "mongoose" /tmp/o.log 2>/dev/null && break; done
MG=0
for i in $(seq 1 10); do
  MG=$(printf 'GET / HTTP/1.0\r\n\r\n' | timeout 4 socat -T3 - tcp:10.0.0.2:80 2>/dev/null | tr -d '\0' | grep -c "200 OK")
  [ "$MG" -ge 1 ] && break; sleep 1
done
kill $q 2>/dev/null; wait $q 2>/dev/null
[ "$MG" -ge 1 ] && { echo "  PASS  opensource: mongoose HTTP"; PASS=$((PASS+1)); } || { echo "  FAIL  opensource: mongoose HTTP"; FAIL=$((FAIL+1)); }
# redis (the network): a RESP PING/SET/GET
q=$(boot_disk redis services/redis/disk-redis.img net)
for i in $(seq 1 30); do sleep 1; grep -aq "redis" /tmp/o.log 2>/dev/null && break; done
RS=""
for i in $(seq 1 10); do
  RS=$( (printf 'PING\r\nSET k v1\r\nGET k\r\n'; sleep 2) | timeout 4 bash -c 'cat >&3; cat <&3' 3<>/dev/tcp/10.0.0.2/6379 2>/dev/null | tr -d '\0')
  echo "$RS" | grep -q "PONG" && break; sleep 1
done
kill $q 2>/dev/null; wait $q 2>/dev/null
echo "$RS" | grep -q "PONG" && echo "$RS" | grep -q "v1" && { echo "  PASS  opensource: redis RESP (PING/SET/GET)"; PASS=$((PASS+1)); } || { echo "  FAIL  opensource: redis RESP"; FAIL=$((FAIL+1)); }

echo "== B5: the soak (the allocator's churn without a leak) =="
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 1 -m 512 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.test=soak" -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 &
QP=$!; sleep 10; kill $QP 2>/dev/null
gate "soak: ALL CHECKS PASSED" "soak: 20k iterations of the churn — 0 leak/0 crashes (B5)"

echo "== B4: the latency SLA (a burst -> the p50/p99 telemetry) =="
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 2 -m 512 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=fast" -display none -serial file:/tmp/o.log -no-reboot \
  -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
  -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 >/tmp/q.log 2>&1 &
QF=$!; sleep 5
for i in $(seq 1 40); do printf 'P%s' $i | timeout 2 socat -T1 - udp:10.0.0.2:9999 2>/dev/null >/dev/null; done
sleep 1; kill $QF 2>/dev/null
# the datapath's latency: the min (the best round trip including the cell) must be < 50 us
LAT=$(grep "telem: pkts=" /tmp/o.log | tail -1)
echo "  measured: $LAT"
MINNS=$(echo "$LAT" | sed -n 's/.*min=\([0-9]*\)c.*/\1/p')
# the threshold in TSC cycles (~3.6GHz): 50 us ~= 180000 cycles. The datapath must fit.
if [ -n "$MINNS" ] && [ "$MINNS" -gt 0 ] && [ "$MINNS" -lt 180000 ]; then
  echo "  PASS  latency-SLA: the datapath's min ${MINNS} cycles < 180000 cycles (50 us), the p50/p99 have been measured (B4)"; PASS=$((PASS+1))
else
  echo "  FAIL  latency-SLA: min=${MINNS} cycles (there is no measurement or it is above the threshold)"; FAIL=$((FAIL+1))
fi

echo "== B8: the fuzzing of the network layer (the kernel survives) =="
q=$(boot_disk mongoose services/mongoose/disk-mongoose.img net)
for i in $(seq 1 30); do sleep 1; printf 'GET / HTTP/1.0\r\n\r\n' | timeout 2 socat -T1 - tcp:10.0.0.2:80 2>/dev/null | grep -q "200 OK" && break; done
# broken/partial/random requests
printf 'GET /partialnocrlf' | timeout 2 socat -T1 - tcp:10.0.0.2:80 >/dev/null 2>&1
head -c 1024 /dev/urandom | timeout 2 socat -T1 - tcp:10.0.0.2:80 >/dev/null 2>&1
printf 'BADMETHOD / HTTP/9.9\r\n\r\n' | timeout 2 socat -T1 - tcp:10.0.0.2:80 >/dev/null 2>&1
printf 'GET / HTTP/1.1\r\nContent-Length: 999999\r\n\r\nxx' | timeout 2 socat -T1 - tcp:10.0.0.2:80 >/dev/null 2>&1
# after the chaos — a valid request must be served (the kernel has not crashed)
AFTER=$(printf 'GET / HTTP/1.0\r\n\r\n' | timeout 4 socat -T3 - tcp:10.0.0.2:80 2>/dev/null | tr -d '\0' | grep -c "200 OK")
grep -qE "CRASH DUMP|THE EXCEPTION" /tmp/o.log && CRASHED=1 || CRASHED=0
kill $q 2>/dev/null; wait $q 2>/dev/null
[ "$AFTER" -ge 1 ] && [ "$CRASHED" = 0 ] && { echo "  PASS  fuzz: the kernel survived the broken requests and served a valid one (B8)"; PASS=$((PASS+1)); } || { echo "  FAIL  fuzz: after=$AFTER crashed=$CRASHED (B8)"; FAIL=$((FAIL+1)); }

echo ""
echo "VALIDATION: PASS=$PASS FAIL=$FAIL"
[ $FAIL = 0 ] && echo "GREEN" || echo "RED"
INNER
) 2>&1 | tee /tmp/validate-docker.out
# ── B10: the DBMSs on Talaria (the host: the run.sh bring docker/QEMU up themselves; the built artefacts are needed) ──
echo "== B10: PostgreSQL / MariaDB / MongoDB on Talaria (the process mode, ext2 RW, the network) =="
# A DEFECT OF THE STAND MUST NAME ITSELF. A gate that failed because of the host looks like
# a regression of the product and costs a session of analysis (session 37: three gates in a
# row, each one green on its own). The sign in the serial is unambiguous: a disk command
# did not complete within 10 s — that is QEMU waiting for an `fdatasync` of a fresh image
# on the runner's spinning disk, while the kernel declares the command lost and takes the
# FS into read-only. It is cured by a sync of the image right after the build
# (`kernel/mkext2.sh`), and here it is said out loud so that the cause is not looked for in the kernel.
stand_defect() { # stand_defect <the serial log>
  [ -f "$1" ] || return 0
  if /usr/bin/grep -aq "a disk command did not complete within 10 s" "$1"; then
    echo "  ^^^ A DEFECT OF THE STAND RATHER THAN OF THE PRODUCT: the host did not manage to put the image onto the disk"
    echo "      (the guest's barrier = an fdatasync of the image at QEMU's side; the runner is on a spinning disk)."
    echo "      The cure: a sync of the image after the mkdisk (kernel/mkext2.sh), repeat the gate."
    /usr/bin/grep -a "A DISK DEFECT\|a disk command did not complete" "$1" | tail -2 | sed "s/^/      | /"
  fi
}
b10() { # b10 NAME DIR ARTIFACT RUNARGS PATTERN
  # the build's artefact OR a ready image (KEEPIMG=1)
  if [ -e "$REPO/services/$2/$3" ] || { [ "${KEEPIMG:-0}" = 1 ] && ls "$REPO/services/$2"/disk-*.img >/dev/null 2>&1; }; then
    # KEEPIMG=1 — do not rebuild the image (ready .img files lie on the remote stand, while
    # the DBMSs' build tree is not there: building PostgreSQL/MariaDB/MongoDB on the stand
    # takes hours, while an image is carried over in minutes).
    if [ "${KEEPIMG:-0}" != 1 ]; then "$REPO/services/$2/mkdisk.sh" >/dev/null 2>&1; fi
    OUT=$("$REPO/services/$2/run.sh" $4 2>&1)
    if echo "$OUT" | grep -qE "$5"; then echo "  PASS  $1 (B10)"; else echo "  FAIL  $1 (B10): no /$5/"; echo "$OUT" | tail -5; stand_defect "$REPO/services/$2/build/serial.log"; fi
  else echo "  SKIP  $1 (B10): there is no $3 — build it with build*.sh"; fi
}
b10 "postgresql: psql from the host, CREATE/INSERT/SELECT" postgresql build/pg-static/opt/pg/bin/postgres "40" "bare metal"
b10 "mariadb: the mariadb client, CREATE DATABASE/TABLE InnoDB, INSERT/SELECT" mariadb build/b/sql/mariadbd "45" "bare metal"
b10 "mongodb: an OP_MSG insert/find (a dynamic ELF, WiredTiger)" mongodb build/root/usr/bin/mongod "100" "mongoprobe: ALL OK"
# ── B11: the JVM (Temurin 21, musl) on Talaria: the start + a multithreaded bench ──
# The same template as for the DBMSs: without an unpacked JDK it is a SKIP. What is checked
# is not "it started" but a SPECIFIC result: the scaling over the cores and the latency of a thread's wake-up.
echo "== B11: the JVM (OpenJDK 21) on Talaria =="
if ls "$REPO/services/jvm/build/jdk-21"* >/dev/null 2>&1; then
  ( cd "$REPO/services/jvm" && ./build-bench.sh >/dev/null 2>&1
    JVM_MAIN="Bench all" ./mkdisk.sh 900M >/dev/null 2>&1
    OUT=$(SMP=4 ./run.sh 400 2>&1)
    echo "$OUT" | grep -a "RESULT" | sed 's/^/  | /'
    SPD=$(echo "$OUT" | sed -n 's/.*RESULT scale speedup4 \([0-9.]*\).*/\1/p' | tail -1)
    PP=$(echo "$OUT" | sed -n 's/.*RESULT pingpong p50 \([0-9.]*\).*/\1/p' | tail -1)
    OKJ=$(echo "$OUT" | grep -ac "openjdk version\|RESULT info cores")
    DONE=$(echo "$OUT" | grep -ac "BENCH DONE")
    # The threshold of the speedup is 2.2x rather than 3.0x: the measured distribution is
    # bimodal (3.6-4.0x or 2.4-2.6x, about a third of the runs on the lower mode —
    # docs/MEASUREMENTS.md §7). The gate must catch the ABSENCE of
    # parallelism rather than lose a lottery of the layout.
    if [ "${OKJ:-0}" -ge 1 ] && [ "${DONE:-0}" -ge 1 ] && [ -n "$SPD" ] && [ -n "$PP" ] \
       && awk "BEGIN{exit !($SPD > 2.2)}" && awk "BEGIN{exit !($PP < 10)}"; then
      echo "  PASS  jvm: the WHOLE set has passed, speedup4=$SPD (>2.2), park/unpark p50=$PP us (<10) (B11)"
    else
      echo "  FAIL  jvm: the set to the end=$DONE speedup4=$SPD park_p50=$PP (B11)"; echo "$OUT" | tail -5
      stand_defect "$REPO/services/jvm/build/serial.log"
    fi )
else
  echo "  SKIP  jvm (B11): there is no services/jvm/build/jdk-21* — see services/jvm/README.md"
fi
# ── B12: ClickHouse (the official binary) on Talaria ──
# The NORMAL kernel: the non-relocatable ET_EXEC of ClickHouse takes 2 MB..526 MB, where the
# kernel's image is loaded — it runs at its high alias and a strict process space keeps none of
# its identity (docs/ARM64-PLAN.md §10.16 stage 7; the high-base build it needed is gone).
echo "== B12: ClickHouse 25.8 (the official binary) on Talaria =="
if ls "$REPO"/services/clickhouse/build/clickhouse-common-static-*/usr/bin/clickhouse >/dev/null 2>&1; then
  ( cd "$REPO/services/clickhouse" && ./mkdisk.sh 1500M >/dev/null 2>&1
    SMP=2 ./run.sh 300 >/dev/null 2>&1
    # WE READ THE LOG RATHER THAN THE LAUNCHER'S OUTPUT. `run.sh` prints a TAIL of forty
    # lines, while the `stat:` block has grown to fifty over the past sessions and pushed
    # both the sums and the exit code out of that tail: the gate failed on a healthy
    # ClickHouse. That is the same class as the other breakages of the harness — "the
    # measurement has stopped measuring but looks like a regression of the product".
    OUT=$(cat build/serial.log 2>/dev/null)
    echo "$OUT" | grep -aE "^[0-9]+\s|^25\." | head -4 | sed 's/^/  | /'
    # 10 million rows must add up to a known sum, MergeTree must insert 200 thousand rows
    # and count them back, and the process must exit with 0
    if echo "$OUT" | grep -qa "49999995000000" && echo "$OUT" | grep -qa "19999900000" \
       && echo "$OUT" | grep -qa "28572" && echo "$OUT" | grep -qa "exit_group(the code=0)"; then
      echo "  PASS  clickhouse: the aggregation of 10 million rows + MergeTree (an insert of 200 thousand, a merge, a read), the exit 0 (B12)"
    else
      echo "  FAIL  clickhouse (B12)"; echo "$OUT" | grep -av "futex\]" | tail -6
      stand_defect "$REPO/services/clickhouse/build/serial.log"
    fi )
else
  echo "  SKIP  clickhouse (B12): there is no services/clickhouse/build/clickhouse-common-static-* — see services/clickhouse/README.md"
fi
