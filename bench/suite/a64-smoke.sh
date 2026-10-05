#!/bin/bash
# The arm64 scoreboard's images, checked under TCG before a Graviton stand is paid for
# (docs/ARM64-PLAN.md phase 9). For each workload the SAME data image mk-ami.sh puts into
# the AMI (`ARCH=arm64 infra/mk-ami.sh <wl> --local-only`) boots on `virt` as the server
# role, takes the workload's warm-up and one short closed-loop run of every test case from
# x86's static tlgen in the container (the protocol is the same bytes); once, the image's
# OTHER role — tlgen.a64 as the job receiver on Talaria arm64 — runs a job against the server
# and its report must say sane numbers (CNTVCT_EL0 calibrated, the open loop held).
#
#   bench/suite/a64-smoke.sh [redis nginx mongoose postgresql mysql mongodb]   (none = all)
#
# What it does not judge: any latency (TCG). Logs: kernel/target/a64-smoke/<wl>.{con,gen}.log
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
WLS=${*:-redis nginx mongoose postgresql mysql mongodb}
LOGD=$REPO/kernel/target/a64-smoke

if [ "${TAL_IN:-0}" != 1 ]; then
  mkdir -p "$LOGD"
  [ -x "$HERE/tlgen/tlgen" ] || { echo "no bench/suite/tlgen/tlgen (make -C bench/suite/tlgen)"; exit 1; }
  make -s -C "$HERE/tlgen" a64 || exit 1
  for w in $WLS; do
    ARCH=arm64 OUT="$LOGD/$w.img" "$HERE/infra/mk-ami.sh" "$w" --local-only > "$LOGD/$w.mk.log" 2>&1 \
      || { echo "  FAIL  $w: the image did not build:"; tail -3 "$LOGD/$w.mk.log"; exit 1; }
  done
  exec docker run --rm --cap-add NET_ADMIN --device /dev/net/tun -e TAL_IN=1 -v "$REPO:/w" -w /w \
    talaria-qemu-aarch64 bash bench/suite/a64-smoke.sh $WLS
fi

# ── inside the container ─────────────────────────────────────────────────────
PASS=0; FAIL=0
ok()  { echo "  PASS  $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL  $1"; FAIL=$((FAIL+1)); }
LOGD=/w/kernel/target/a64-smoke
KIMG=/w/kernel/target/aarch64-talaria/release/talaria.img
TLGEN=/w/bench/suite/tlgen/tlgen
ip link add br0 type bridge; ip addr add 10.0.0.1/24 dev br0; ip link set br0 up
for t in tap0 tap1; do ip tuntap add $t mode tap; ip link set $t master br0; ip link set $t up; done

# guest TAP MAC IMAGE SMP MEM APPEND CONSOLE: a guest in the background on a copy of IMAGE; sets $G.
guest() {
  cp "$3" "/tmp/$1.img"
  qemu-system-aarch64 -M virt,gic-version=3,its=on -cpu max -smp "$4" -m "$5" -display none \
    -serial "file:$7" -kernel "$KIMG" -append "console=ttyAMA0 talaria.mode=proc talaria.smp=$4 $6" \
    -netdev tap,id=n0,ifname="$1",script=no,downscript=no -device virtio-net-device,netdev=n0,mac="$2" \
    -drive id=d0,file="/tmp/$1.img",format=raw,if=none -device virtio-blk-device,drive=d0 >/dev/null 2>&1 &
  G=$!
}
port_open() { python3 -c "import socket,sys; socket.create_connection((sys.argv[1], int(sys.argv[2])), 2).close()" "$1" "$2" 2>/dev/null; }
# report_ok FILE: measured > 0, no errors — the run talked the protocol
report_ok() { python3 - "$1" <<'P'
import json, sys
try: r = json.load(open(sys.argv[1]))
except Exception as e: print("not JSON: %s" % e); sys.exit(1)
c = r.get("counts", {})
m = c.get("measured", 0)
e = sum(c.get(k, 0) for k in ("lost", "timeouts", "conn_errors", "app_errors"))
p50 = r.get("latency", {}).get("service", {}).get("p50", 0) / 1000
print("measured %s, errors %s, service p50 %.0f us, counter %.1f MHz" % (m, e, p50, r.get("tsc_hz", 0) / 1e6))
sys.exit(0 if m > 0 and e == 0 else 1)
P
}

first=1
for w in $WLS; do
  unset PORT PROTO SMP MEM CASES WARMUP_CMD
  . /w/bench/suite/workloads/$w/meta.env
  CON=$LOGD/$w.con.log; GEN=$LOGD/$w.gen.log; : > "$GEN"
  # TCG: a database's first start is minutes
  # SMOKE_APPEND: TCG's knob, not the stand's — under TCG every PostgreSQL backend spins in
  # epoll and a fork's handshake took 30 s; netmix parks the non-polling (kernel/test-aarch64-db.sh).
  guest tap0 52:54:00:12:34:56 "$LOGD/$w.img" "${SMP:-1}" "${MEM:-512}" "${SMOKE_APPEND-talaria.netmix=1000000} talaria.manifest=/cell.manifest talaria.ip=10.0.0.2/24 talaria.gw=10.0.0.1" "$CON"
  SRV=$G; t0=$SECONDS; up=0
  while [ $((SECONDS - t0)) -lt 600 ] && kill -0 $SRV 2>/dev/null; do
    port_open 10.0.0.2 "$PORT" && { up=1; break; }; sleep 3
  done
  if [ $up = 1 ]; then ok "$w: the server listens on :$PORT after $((SECONDS - t0)) s (the arm64 tree, the shared manifest)"
  else bad "$w: no listener on :$PORT in $((SECONDS - t0)) s"; grep -aE '\[trap\]|PANIC|exit_group|not found|refus' "$CON" | tail -4 | cut -c1-160 | sed 's/^/      /'
       kill $SRV 2>/dev/null; wait $SRV 2>/dev/null; continue; fi
  # A listener is not a server: PostgreSQL listens while it still answers 57P03 "starting up".
  # The first case is retried until one request is answered (TCG: minutes for a database).
  c=${CASES%%|*}; args=${c#*:}; t0=$SECONDS
  while [ $((SECONDS - t0)) -lt 600 ]; do
    eval "$TLGEN --proto $PROTO --host 10.0.0.2 --port $PORT $args --conns 1 --rate 0 --duration 1 --warmup 0 --timeout 30 --spin --out /tmp/r.json" >> "$GEN" 2>&1
    report_ok /tmp/r.json >/dev/null && break; sleep 5
  done
  echo "      the protocol answers after $((SECONDS - t0)) s more"
  if [ -n "${WARMUP_CMD:-}" ]; then
    GUEST_IP=10.0.0.2 PORT=$PORT TLGEN=$TLGEN sh -c "$WARMUP_CMD"
  fi
  IFS='|'; set -- $CASES; unset IFS
  for c in "$@"; do
    label=${c%%:*}; args=${c#*:}
    eval "$TLGEN --proto $PROTO --host 10.0.0.2 --port $PORT $args --conns 2 --rate 0 --duration 3 --warmup 1 --timeout 30 --spin --name $w-$label --stand smoke --out /tmp/r.json" >> "$GEN" 2>&1
    if v=$(report_ok /tmp/r.json); then ok "$w/$label: $v"; else bad "$w/$label: $v"; fi
  done
  if [ $first = 1 ]; then
    first=0
    # The image's other role: tlgen.a64 on Talaria arm64 as the job receiver, a job against the server,
    # with the stand's generator knob (terraform's talaria_loadgen_idle: it sleeps on the receive ring).
    guest tap1 52:54:00:12:34:57 "$LOGD/$w.img" 1 512 "talaria.manifest=/loadgen.manifest talaria.netidle=1 talaria.ip=10.0.0.3/24 talaria.gw=10.0.0.1" "$LOGD/$w.loadgen.con.log"
    LG=$G; t0=$SECONDS
    while [ $((SECONDS - t0)) -lt 300 ] && ! port_open 10.0.0.3 9100; do sleep 3; done
    c=${CASES%%|*}; args=${c#*:}
    eval "set -- $args"
    python3 /w/bench/suite/lib/job.py 10.0.0.3 9100 /tmp/j.json --proto "$PROTO" --host 10.0.0.2 --port "$PORT" "$@" \
      --conns 2 --rate 200 --duration 4 --warmup 1 --timeout 30 --spin --name "$w-loadgen" --stand smoke >> "$GEN" 2>&1
    if v=$(report_ok /tmp/j.json); then
      # the open loop at 200 req/s for 4 s: the generator must have sent ~800 and its clock must
      # agree with the wall (a miscalibrated CNTVCT sends at the wrong rate)
      if python3 -c "import json,sys; r=json.load(open('/tmp/j.json')); s=r['counts']['sent']; sys.exit(0 if 500 <= s <= 1100 else 1)"; then
        ok "the loadgen role: tlgen.a64 on Talaria arm64 ran a job against $w ($v)"
      else bad "the loadgen role: the open loop sent $(python3 -c "import json; print(json.load(open('/tmp/j.json'))['counts']['sent'])") at 200/s x 4 s"; fi
    else bad "the loadgen role: $v"; fi
    kill $LG 2>/dev/null; wait $LG 2>/dev/null
  fi
  kill $SRV 2>/dev/null; wait $SRV 2>/dev/null
  grep -aqE '\[trap\]|PANIC' "$CON" && bad "$w: a trap or a panic in the console" || true
done
chown -R --reference=/w/kernel "$LOGD" 2>/dev/null
echo "A64 SMOKE TOTAL: PASS=$PASS FAIL=$FAIL"
[ $FAIL = 0 ]
