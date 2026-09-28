#!/bin/sh
# A DURABILITY PROBE ON LIVE EBS: does an ACKNOWLEDGED write survive
# a hard stop of the instance (`stop-instances --force` — the power pulled, without
# the ACPI button and without a sync)?
#
#   infra/powercut-ec2.sh [seconds of load before the stop]   # default 20
#   env: STAND_DIR= (the mongodb stand must be up, brought by stand-up.sh)
#
# Why separate from bench/suite/powercut.sh. The local probe kills QEMU, while
# its writes are already in the HOST's page cache — the "power" there is pulled from the
# guest, not from the disk. Here the disk is real: EBS without a volatile cache (VWC=0), and
# it is on it that the kernel may (or may not) consider an ordinary write
# durable without FUA (`talaria.fua=`, fs.rs `blk_write_durable`). The class of
# error being looked for is "fsync returned success without doing what it promised", the worst
# in the project's history; so it is NOT "the server came up" that is checked but THE NUMBERS:
#
# 1. From the Linux generator pymongo inserts documents _id=0,1,2,... with
#    writeConcern {w:1, j:true} — every insert is acknowledged ONLY after
#    an fdatasync of the WiredTiger journal, i.e. after a durable write by the kernel.
#    The number of acknowledged inserts is written to the generator's disk after EVERY ack.
# 2. In the middle of the inserts the Talaria server is stopped with --force.
# 3. The server starts again (a DHCP flake on reboot — up to three attempts).
# 4. Every _id below the acknowledged count must be in place: count == acked. Fewer means
#    the kernel lied about durability, the probe FAILED; the change is inadmissible.
#
# The serial of both boots (before the stop and after) stays in results/powercut-ec2-<stamp>/.
set -u
SECS=${1:-20}
HERE=$(cd "$(dirname "$0")" && pwd)
SUITE=$(cd "$HERE/.." && pwd)
STAND_DIR=${STAND_DIR:-$HOME/.talaria-lab/stand}
[ -f "$STAND_DIR/stand.env" ] || { echo "the stand is not up"; exit 2; }
. "$STAND_DIR/stand.env"
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-$STAND_REGION}
ID=${talaria_server_id:?no talaria_server_id in stand.env}
SRV=${talaria_server_ip:?no talaria_server_ip}
GEN=${linux_loadgen_pub:?no linux_loadgen_pub}
OUT=$SUITE/results/powercut-ec2-$(date +%Y%m%d-%H%M%S); mkdir -p "$OUT"
SSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=20 -i $STAND_KEY ec2-user@$GEN"
say() { echo "$*" | tee -a "$OUT/log.txt"; }
console() { aws ec2 get-console-output --instance-id "$ID" --latest --query Output --output text 2>/dev/null || true; }

say "== a durability probe on EBS: server $ID ($SRV), generator $GEN, load $SECS s -> $OUT"

# ── 0. the power button must be DISABLED, otherwise the stop is a normal one ──
# `stop-instances --force` on Nitro STILL presses the ACPI button, and the kernel
# shuts the machine down normally: SIGTERM -> mongod exits by itself ("interrupted at
# shutdown" on the generator) -> sync -> S5. The first probe "passed" exactly
# like that — the journal was clean on remount (start 0),
# i.e. a normal shutdown was being checked, not a power cut. With `talaria.pwrbtn=0`
# the kernel does not listen to the button, and the hypervisor, tired of waiting, cuts the power hard.
# The cmdline tail is changed through talaria-cmdline.sh (a server reboot,
# 2-3 min) and restored at the end.
cur=$(aws ec2 describe-instance-attribute --instance-id "$ID" --attribute userData --query 'UserData.Value' --output text | base64 -d)
RESTORE=
if ! printf '%s' "$cur" | tr ' ' '\n' | /usr/bin/grep -aq '^talaria\.pwrbtn=0$'; then
    RESTORE=$(printf '%s\n' "$cur" | tr ' ' '\n' | /usr/bin/grep -av '^talaria\.\(smp\|manifest\|stat\)=' | /usr/bin/grep -av '^$' | tr '\n' ' ' | sed 's/ *$//')
    say "== the power button: setting talaria.pwrbtn=0 (a server reboot; \"$RESTORE\" will be restored afterwards)"
    "$HERE/talaria-cmdline.sh" "$RESTORE talaria.pwrbtn=0" | tail -3 | tee -a "$OUT/log.txt"
    . "$STAND_DIR/stand.env"
fi
restore() {
    [ -n "$RESTORE" ] || return 0
    say "== restoring the cmdline tail: \"$RESTORE\""
    "$HERE/talaria-cmdline.sh" "$RESTORE" | tail -2 | tee -a "$OUT/log.txt"
}

# ── the generator: pymongo and the scripts ──────────────────────────────────
# pymongo STRICTLY 3.x: 4.x requires wire version 9 (MongoDB 4.4+), while the stand's
# mongod — 4.2 (wire 7): «Server reports wire version 7, but this version of
# PyMongo requires at least 9" — and the probe stopped the server without a single
# acknowledged insert.
$SSH 'python3 -c "import pymongo,sys; sys.exit(0 if int(pymongo.version.split(\".\")[0]) < 4 else 1)" 2>/dev/null || { python3 -m pip --version >/dev/null 2>&1 || sudo dnf install -y -q python3-pip >/dev/null; python3 -m pip install -q --user "pymongo<4"; }' \
    || { say "pymongo<4 could not be installed on the generator"; exit 2; }
$SSH 'cat > pc-insert.py' <<'PY'
import sys, time, pymongo
host, secs, out = sys.argv[1], float(sys.argv[2]), sys.argv[3]
c = pymongo.MongoClient(host, 27017, serverSelectionTimeoutMS=10000, socketTimeoutMS=10000)
col = c.pcut.t
col.drop()
wc = col.with_options(write_concern=pymongo.WriteConcern(w=1, j=True))
n = 0; t0 = time.time(); worst = 0.0
open(out, "w").write("0\n")
try:
    while time.time() - t0 < secs:
        t = time.time()
        wc.insert_one({"_id": n, "t": t})
        d = time.time() - t
        if d > worst: worst = d
        n += 1                                   # acknowledged: _id 0..n-1
        f = open(out, "w"); f.write("%d\n" % n); f.flush(); f.close()
except Exception as e:
    print("stopped after %d acknowledged: %s" % (n, e))
el = time.time() - t0
print("acknowledged %d in %.1f s (%.0f/s, worst %.1f ms)" % (n, el, n / max(el, 0.001), worst * 1000))
PY
$SSH 'cat > pc-check.py' <<'PY'
import sys, pymongo
host, acked = sys.argv[1], int(sys.argv[2])
c = pymongo.MongoClient(host, 27017, serverSelectionTimeoutMS=10000)
col = c.pcut.t
have = col.count_documents({"_id": {"$lt": acked}})
total = col.count_documents({})
mx = col.find_one(sort=[("_id", -1)])
print("acknowledged %d, of them present %d, total %d, the highest _id %s" % (acked, have, total, mx and mx["_id"]))
missing = [i for i in range(acked) if col.count_documents({"_id": i}) == 0] if have != acked else []
if missing: print("LOST acknowledged _id:", missing[:20], "..." if len(missing) > 20 else "")
sys.exit(0 if have == acked else 1)
PY
$SSH 'cat > pc-wait.py' <<'PY'
import sys, time, socket
host, secs = sys.argv[1], float(sys.argv[2])
t0 = time.time()
while time.time() - t0 < secs:
    try:
        s = socket.create_connection((host, 27017), timeout=3); s.close(); print("the port opened in %.0f s" % (time.time() - t0)); sys.exit(0)
    except Exception: time.sleep(2)
print("the port did not open within %.0f s" % secs); sys.exit(1)
PY

# ── 1. inserts with j:true ───────────────────────────────────────────────────
$SSH "python3 pc-wait.py $SRV 30" || { say "the server is not listening on 27017 before the probe"; exit 2; }
# The generator must SURVIVE the cut: with the button disabled Nitro waits ~4-5 min
# before pulling the power (the first honest probe: a stop after 267 s with the
# generator running for 80 s — by the moment of the cut there were no inserts any more, a weaker probe
# than intended). We keep the inserts going for SECS + 10 min; the cut happens within that.
$SSH "rm -f pc-acked.txt pc-insert.log; nohup python3 pc-insert.py $SRV $((SECS + 600)) pc-acked.txt > pc-insert.log 2>&1 &"
sleep 3
say "   the inserts are running: acknowledged $($SSH cat pc-acked.txt 2>/dev/null | tr -d '\n') after 3 s"
sleep "$SECS"
say "   after $SECS s: acknowledged $($SSH cat pc-acked.txt 2>/dev/null | tr -d '\n')"

# ── 2. the power is pulled ───────────────────────────────────────────────────
console > "$OUT/serial-before-cut.log"
T0=$(date +%s)
aws ec2 stop-instances --force --instance-ids "$ID" >/dev/null
say "== stop --force issued $(date +%T)"
aws ec2 wait instance-stopped --instance-ids "$ID"
say "   stopped in $(( $(date +%s) - T0 )) s"
# the generator is still spinning (waiting for the server) — finish it off so that it does not hold pc-acked.txt
$SSH "pkill -f pc-insert.py; sleep 1" >/dev/null 2>&1 || true
sleep 3
ACKED=$($SSH cat pc-acked.txt | tr -d '\n')
say "   the generator: $($SSH cat pc-insert.log | tail -2 | tr '\n' ' ')"
say "   ACKNOWLEDGED BEFORE THE STOP: $ACKED"
[ "${ACKED:-0}" -gt 0 ] || { say "not a single acknowledged insert — the probe is about nothing"; exit 2; }
if $SSH cat pc-insert.log | /usr/bin/grep -aq 'InterruptedAtShutdown'; then
    say "== THE SHUTDOWN WAS A NORMAL ONE (mongod got a SIGTERM from the button) — this is NOT a power cut, the probe is about the wrong thing"; restore; exit 2
fi

# ── 3. the second boot (a DHCP flake — up to three attempts) ─────────────────
attempt=1; up=0
while [ $attempt -le 3 ]; do
    T0=$(date +%s)
    aws ec2 start-instances --instance-ids "$ID" >/dev/null
    aws ec2 wait instance-running --instance-ids "$ID"
    say "== start #$attempt $(date +%T)"
    if $SSH "python3 pc-wait.py $SRV 300" | tee -a "$OUT/log.txt"; then up=1; break; fi
    console > "$OUT/serial-boot-$attempt.log"
    say "   did not come up; in serial: $(/usr/bin/grep -a -c 'dhcp TIMEOUT' "$OUT/serial-boot-$attempt.log") x dhcp TIMEOUT — restarting"
    aws ec2 stop-instances --instance-ids "$ID" >/dev/null; aws ec2 wait instance-stopped --instance-ids "$ID"
    attempt=$((attempt + 1))
done
pub=$(aws ec2 describe-instances --instance-ids "$ID" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
sed -i "s/^talaria_server_pub=.*/talaria_server_pub=$pub/" "$STAND_DIR/stand.env"
[ $up = 1 ] || { say "== the server DID NOT COME UP after the stop — the probe FAILED (the FS/database journal)"; console > "$OUT/serial-after-cut.log"; restore; exit 1; }

# ── 4. the count ─────────────────────────────────────────────────────────────
sleep 5
if $SSH "python3 pc-check.py $SRV $ACKED" | tee -a "$OUT/log.txt"; then r=0; say "== THE PROBE PASSED: all $ACKED acknowledged inserts are in place after stop --force"
else r=1; say "== THE PROBE FAILED: acknowledged inserts were LOST — the kernel lied about durability"; fi
sleep 30   # the EC2 console lags
console > "$OUT/serial-after-cut.log"
/usr/bin/grep -a 'ext2:\|nvme:\|THE JOURNAL\|recover\|WiredTiger.*[Rr]ecover' "$OUT/serial-after-cut.log" | cut -c1-200 | head -20 | sed 's/^/     /' | tee -a "$OUT/log.txt"
restore
echo "POWERCUT-EC2-DONE rc=$r acked=$ACKED"
exit $r
