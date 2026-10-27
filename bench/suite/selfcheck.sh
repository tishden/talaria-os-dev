#!/bin/sh
# THE SELF-CHECK OF THE bench/suite HARNESS — a regression of the instrument itself, without guests and without
# EC2, in half a minute on the host. Every check here is a former defect that
# once cost a run (or a session), and every one must FAIL LOUDLY:
#
#  1. the generator's report PARSES and contains a non-empty `counts.sent` — on EC2
#     an old tlgen appended `{"error": 1}` after the report, and all ten points
#     of the Talaria side silently dropped out of the table;
#  2. the service latency is no larger than the open loop — otherwise the instrument lies;
#  3. the job receiver (`--serve`) survives a SECOND job and gives the same
#     counters (`sent`) as the first — it used to die on the 2nd job;
#     and a second mongo insert job does NOT REPEAT the first one's keys — the key's
#     nonce was "once per process", and the suite measured the round trip of a duplicate key;
#  4. a connection drop / a closed port / garbage instead of a report DO NOT BRING A RUN DOWN:
#     job.py writes a stub with the reason (code 3 — there is no generator, 0 — anything else);
#     a silent server does not
#     hang the generator (connect and the protocol handshake hit --timeout);
#     a break of the PATH to the generator (the runner's VPN) with a JOB_RETRY_S budget is retried
#     rather than costing a point;
#  5. report.py on a broken file shouts "NOT PARSED", tolerates a tail after the JSON,
#     and does not take `report.json`/`floor.json` in the directory for points;
#  6. a point with a single error is judged and marked, an overloaded point is
#     rejected WITH A REASON; the sum over two runs does not depend on their order;
#  6b. the ">1/>4 ms" column counts from hist_ns, does NOT pass the generator's debt off as
#     server events (on the top steps of the ladder the open loop
#     counted 1.9 million "answers longer than 4 ms" with a service maximum of
#     a quarter of a millisecond), and a percentile with only a few requests beyond it
#     is judged neither for nor against;
#  6d. the phase's console is captured DURING the run and spliced: the
#     `get-console-output` window is 64 KB, and a single snapshot at the end keeps only
#     the last minutes (a whole first diagnostic pass was lost that way);
#     the splice does not duplicate the overlap and does not stay silent about a window gap;
#  7. the accumulator of the actor matrix's results is NOT the file the runner writes
#     (a `tee` used to overwrite what had been accumulated on every boot);
#  8. the ClickHouse Linux guest's initramfs is rebuilt when q.sql changes:
#     the content stamp changes with one byte in the query set.
#
# Run: bench/suite/selfcheck.sh   (needs tlgen.host — `make -C tlgen host`,
#         python3, debugfs/mke2fs for item 8)
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
T=$(mktemp -d)
# SELFCHECK_KEEP=1 keeps the working directory (reports, logs, tables) for analysis.
trap 'kill $(jobs -p) 2>/dev/null; if [ "${SELFCHECK_KEEP:-0}" = 1 ]; then echo "the working directory was kept: $T"; else rm -rf "$T"; fi' EXIT
PASS=0; FAIL=0
ok()   { PASS=$((PASS + 1)); echo "  PASS  $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL  $1"; }
check() { if eval "$2"; then ok "$1"; else fail "$1"; fi; }
TLGEN=$HERE/tlgen/tlgen.host
[ -x "$TLGEN" ] || { echo "no $TLGEN — run make -C bench/suite/tlgen host"; exit 1; }
PORT=$((40000 + $$ % 20000)); JPORT=$((PORT + 1)); DPORT=$((PORT + 2)); CPORT=$((PORT + 3))
jq_() { python3 -c "import json,sys; r=json.load(open(sys.argv[1])); print(eval(sys.argv[2]))" "$@"; }
wait_port() { i=0; while ! python3 -c "import socket,sys; socket.create_connection(('127.0.0.1',int(sys.argv[1])),timeout=0.2).close()" "$1" 2>/dev/null; do i=$((i+1)); [ $i -lt 50 ] || return 1; sleep 0.1; done; }

echo "== 1-2. the generator's report: parsing, counts.sent, service <= open_loop =="
python3 "$HERE/lib/fakeresp.py" --port $PORT >"$T/fr.log" 2>&1 & FR=$!
wait_port $PORT || { fail "fakeresp did not come up"; cat "$T/fr.log"; }
"$TLGEN" --proto resp --host 127.0.0.1 --port $PORT --conns 4 --rate 1000 --duration 1.5 --warmup 0.3 --epoll \
  --out "$T/r1.json" >"$T/tl1.log" 2>&1
check "the tlgen report parses as JSON, schema=1" "jq_ $T/r1.json \"r['schema']==1\" | grep -q True"
check "counts.sent is non-empty (>1000 in 1.5 s at 1000/s)" "jq_ $T/r1.json \"r['counts']['sent']>1000\" | grep -q True"
check "counts.measured > 0 and rc=0, error empty" "jq_ $T/r1.json \"r['counts']['measured']>0 and r.get('rc')==0 and not r.get('error')\" | grep -q True"
check "both histograms are in place and service.p50 <= open_loop.p50" \
  "jq_ $T/r1.json \"0 < r['latency']['service']['p50'] <= r['latency']['open_loop']['p50']\" | grep -q True"
check "service.p99 <= open_loop.p99" "jq_ $T/r1.json \"r['latency']['service']['p99'] <= r['latency']['open_loop']['p99']\" | grep -q True"

echo "== 3. the job receiver: a second job in the same process =="
"$TLGEN" --serve $JPORT >"$T/serve.log" 2>&1 & SV=$!
wait_port $JPORT || fail "tlgen --serve did not come up"
ARGS="--proto resp --host 127.0.0.1 --port $PORT --conns 4 --rate 1000 --duration 1.5 --warmup 0.3 --epoll --stand talaria --tags workload=self,case=ping"
python3 "$HERE/lib/job.py" 127.0.0.1 $JPORT "$T/j1.json" $ARGS >"$T/j1.log" 2>&1; RC1=$?
python3 "$HERE/lib/job.py" 127.0.0.1 $JPORT "$T/j2.json" $ARGS >"$T/j2.log" 2>&1; RC2=$?
check "job.py: both jobs returned 0" "[ $RC1 = 0 ] && [ $RC2 = 0 ]"
check "both reports parsed, with measured>0" "jq_ $T/j1.json \"r['counts']['measured']>0\" | grep -q True && jq_ $T/j2.json \"r['counts']['measured']>0\" | grep -q True"
S1=$(jq_ "$T/j1.json" "r['counts']['sent']"); S2=$(jq_ "$T/j2.json" "r['counts']['sent']")
check "a scenario repeated in one process gives the same counters (sent $S1 / $S2, tolerance 1%)" \
  "python3 -c 'import sys; a,b=map(int,sys.argv[1:]); sys.exit(0 if abs(a-b)<=max(a,b)*0.01 else 1)' $S1 $S2"
check "the receiver is alive after two jobs" "kill -0 $SV 2>/dev/null"
check "job.py's output contains the service latency and the debt" "grep -q 'service p50=.*debt p99=' $T/j2.log"
# AN ARGUMENT WITH A SPACE must reach the receiver as one argument: in mysql and
# postgresql that is `--query 'SELECT 1'`, and without quotes on the wire the receiver saw
# two arguments and answered "the job did not parse" — the whole ladder was stubs.
python3 "$HERE/lib/job.py" 127.0.0.1 $JPORT "$T/j2q.json" $ARGS --name "self/quoted case" >"$T/j2q.log" 2>&1
check "an argument with a space reaches the receiver whole (--name 'self/quoted case')" \
  "jq_ $T/j2q.json \"r.get('name')=='self/quoted case' and r['counts']['measured']>0\" | grep -q True"
kill $FR 2>/dev/null; wait $FR 2>/dev/null

# THE INSERT KEY IS UNIQUE BETWEEN THE JOBS OF ONE RECEIVER. In mongo `_id` must
# be unique in the collection, and the collection is not cleaned between the suite's points.
# The nonce in the key was taken "once per process", and in --serve every next job
# (including the ceiling probes) inserted the same keys: 65-75 % duplicate key errors
# at EVERY insert point on both sides (ec2all-20260905-205134) — the round trip
# of a failure was measured, and at ok: 1.0. The toy mongo remembers the keys and answers
# writeErrors on a repeat; the second job must pass without app_errors.
MPORT=$((PORT + 4))
python3 "$HERE/lib/fakemongo.py" --port $MPORT >"$T/fm.log" 2>&1 & FM=$!
wait_port $MPORT || { fail "fakemongo did not come up"; cat "$T/fm.log"; }
MARGS="--proto mongo --host 127.0.0.1 --port $MPORT --op insert --conns 4 --rate 1000 --duration 1.0 --warmup 0.2 --epoll --stand talaria --tags workload=self,case=insert"
python3 "$HERE/lib/job.py" 127.0.0.1 $JPORT "$T/m1.json" $MARGS >"$T/m1.log" 2>&1
python3 "$HERE/lib/job.py" 127.0.0.1 $JPORT "$T/m2.json" $MARGS >"$T/m2.log" 2>&1
kill $FM 2>/dev/null; wait $FM 2>/dev/null
check "mongo insert: the first job without write failures (app_errors=0, measured>0)" \
  "jq_ $T/m1.json \"r['counts']['app_errors']==0 and r['counts']['measured']>0\" | grep -q True"
check "mongo insert: the SECOND job of the same receiver does not repeat the keys (app_errors=0; $(cat $T/fm.log 2>/dev/null | tr -d '\n'))" \
  "jq_ $T/m2.json \"r['counts']['app_errors']==0 and r['counts']['measured']>0\" | grep -q True"

echo "== 4. a connection drop marks the point rather than bringing the run down =="
python3 "$HERE/lib/job.py" 127.0.0.1 $CPORT "$T/j3.json" $ARGS >"$T/j3.log" 2>&1; RC3=$?
# CODE 3 MEANS "THERE IS NO GENERATOR", not "the point was not taken": by it the runner abandons
# the rest of this side's ladder. A dead agent otherwise costs the retry budget
# (600 s) on EVERY remaining point — one scoreboard lost mysql
# entirely that way (4.9 hours, 12 judgeable points of 40).
check "a closed receiver port: code 3, a stub with the reason" \
  "[ $RC3 = 3 ] && grep -q 'the point was not taken: the generator is unreachable' $T/j3.log && jq_ $T/j3.json \"r['counts']['measured']==0 and 'unreachable' in r['error']\" | grep -q True"
# the server dies in the middle of the measurement: the generator catches the drop, the report is still there
python3 "$HERE/lib/fakeresp.py" --port $DPORT --die-after 300 >"$T/fr2.log" 2>&1 & FR2=$!
wait_port $DPORT || fail "fakeresp --die-after did not come up"
python3 "$HERE/lib/job.py" 127.0.0.1 $JPORT "$T/j4.json" \
  --proto resp --host 127.0.0.1 --port $DPORT --conns 4 --rate 1000 --duration 1.5 --warmup 0.3 --epoll --stand talaria --tags workload=self,case=die \
  >"$T/j4.log" 2>&1; RC4=$?
check "the server died halfway: job.py returned 0, the report parsed" "[ $RC4 = 0 ] && jq_ $T/j4.json \"r['schema']==1\" | grep -q True"
check "the drop is visible in the counters (conn_errors/timeouts/lost) or in error" \
  "jq_ $T/j4.json \"(r['counts']['conn_errors']+r['counts']['timeouts']+r['counts']['lost'])>0 or bool(r.get('error'))\" | grep -q True"
check "the receiver survived a drop on the server's side" "kill -0 $SV 2>/dev/null"
wait $FR2 2>/dev/null
# garbage instead of a report: an impostor receiver answers with something that is not JSON
python3 - $CPORT <<'PY' >"$T/junk.log" 2>&1 & JK=$!
import socket, sys
ls = socket.socket(); ls.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
ls.bind(("127.0.0.1", int(sys.argv[1]))); ls.listen(1)
while True:                       # the first connect is from wait_port, it is empty
    c, _ = ls.accept(); d = c.recv(4096)
    if d:
        c.sendall(b"tlgen: segmentation fault\n"); c.close(); break
    c.close()
PY
wait_port $CPORT || fail "the impostor did not come up"
python3 "$HERE/lib/job.py" 127.0.0.1 $CPORT "$T/j5.json" $ARGS >"$T/j5.log" 2>&1; RC5=$?
check "garbage instead of a report: code 0, a stub saying \"answered with something other than a report\"" \
  "[ $RC5 = 0 ] && grep -q 'answered with something other than a report' $T/j5.log && jq_ $T/j5.json \"r['counts']['measured']==0\" | grep -q True"
wait $JK 2>/dev/null
kill $SV 2>/dev/null; wait $SV 2>/dev/null
# A silent server: it accepts the connection and says not a word. A protocol
# handshake where the server speaks first (mysql) used to hang in a blocking recv
# until the other side closed, and a blocking connect to a silent SYN takes up to 10 s on
# Talaria and up to two minutes on Linux: one reconnect in the middle of a run
# stopped the generator's schedule for seconds. The ceiling is --timeout.
MPORT=$((PORT + 4))
python3 - $MPORT <<'PY' >"$T/mute.log" 2>&1 & MU=$!
import socket, sys, time
ls = socket.socket(); ls.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
ls.bind(("127.0.0.1", int(sys.argv[1]))); ls.listen(16)
held = []
t0 = time.time()
while time.time() - t0 < 12:
    ls.settimeout(1)
    try: c, _ = ls.accept(); held.append(c)
    except socket.timeout: pass
PY
wait_port $MPORT || fail "the silent server did not come up"
T0=$(date +%s)
timeout 20 "$TLGEN" --proto mysql --host 127.0.0.1 --port $MPORT --conns 2 --rate 100 --duration 1 --warmup 0.2 --timeout 1 \
  >"$T/mute.json" 2>"$T/mute.err"; RCM=$?
DT=$(( $(date +%s) - T0 ))
check "a silent server: the protocol handshake hits --timeout rather than hanging (${DT} s, rc=$RCM)" "[ $RCM != 124 ] && [ $DT -le 6 ]"
kill $MU 2>/dev/null; wait $MU 2>/dev/null

# WHY THE CONNECTION DID NOT COME UP — three different reasons, and each must
# name ITSELF. This is a former defect of the instrument: all six failure paths reported
# strerror(errno), and some paths did not touch errno at all, so the report received
# the value from a SUCCESSFUL non-blocking connect. In the suite
# ec2all-20260907-180145 a FATAL from PostgreSQL looked because of that like
# "Operation in progress", and a mongod refusal like "No error information", while
# "after an overload the server stops accepting" counted for two sessions as a defect of
# OUR network stack.
EPORT=$((PORT + 6))
python3 - $EPORT <<'PGFAKE' >"$T/pgfake.log" 2>&1 & PGF=$!
import socket, struct, sys, threading, time
port = int(sys.argv[1])
def serve(p, mode):
    ls = socket.socket(); ls.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    ls.bind(("127.0.0.1", p)); ls.listen(8); ls.settimeout(1)
    t0 = time.time()
    while time.time() - t0 < 25:
        try: c, _ = ls.accept()
        except socket.timeout: continue
        try:
            c.recv(4096)
            if mode == "err":
                f = b"SFATAL\x00C53300\x00Msorry, too many clients already\x00\x00"
                c.sendall(b"E" + struct.pack(">I", len(f) + 4) + f)
        except OSError: pass
        c.close()
for i, m in ((0, "eof"), (1, "err")):
    threading.Thread(target=serve, args=(port + i, m), daemon=True).start()
time.sleep(26)
PGFAKE
wait_port $EPORT || fail "the fake pg did not come up"
wait_port $((EPORT + 1)) || fail "the fake pg (ErrorResponse) did not come up"
pg_try() {  # $1 port, $2 report file
    "$TLGEN" --proto pg --host 127.0.0.1 --port "$1" --user u --db d --query 'SELECT 1' \
        --conns 2 --rate 100 --duration 1 --warmup 0.2 --timeout 2 --out "$2" >/dev/null 2>&1
}
pg_try $((EPORT + 9)) "$T/d1.json"
pg_try $EPORT         "$T/d2.json"
pg_try $((EPORT + 1)) "$T/d3.json"
check "a connect refusal is named in words (connect(): Connection refused)" \
  "jq_ $T/d1.json \"'connect()' in r.get('error','') and 'refused' in r.get('error','').lower()\" | grep -q True"
check "the server closed the connection during the handshake — EXACTLY that is said, not the errno from connect" \
  "jq_ $T/d2.json \"'handshake' in r.get('error','') and 'EOF' in r.get('error','')\" | grep -q True"
check "a SERVER error reaches the report with its own text (FATAL 53300 too many clients)" \
  "jq_ $T/d3.json \"'53300' in r.get('error','') and 'too many clients' in r.get('error','')\" | grep -q True"
check "none of the three failures reports a foreign errno (Operation in progress)" \
  "! grep -al 'Operation in progress' $T/d1.json $T/d2.json $T/d3.json 2>/dev/null | grep -q ."
kill $PGF 2>/dev/null; wait $PGF 2>/dev/null
# The path to the generator is broken for a while (the runner's VPN reconnects for 2-4 minutes
# and answers with a refusal meanwhile): with a JOB_RETRY_S budget the point WAITS for
# the receiver, without a budget it is marked at once, as before. On EC2 without this
# two test cases were lost entirely, and the analysis went into the kernel.
RPORT=$((PORT + 5))
python3 "$HERE/lib/fakeresp.py" --port $PORT >"$T/fr3.log" 2>&1 & FR3=$!
wait_port $PORT || fail "fakeresp (the repeat) did not come up"
( sleep 3; exec "$TLGEN" --serve $RPORT ) >"$T/serve2.log" 2>&1 & SV2=$!
T0=$(date +%s)
JOB_RETRY_S=30 python3 "$HERE/lib/job.py" 127.0.0.1 $RPORT "$T/j6.json" $ARGS >"$T/j6.log" 2>&1; RC6=$?
DT=$(( $(date +%s) - T0 ))
check "the receiver came up after 3 s: with a retry budget the point was taken (${DT} s, retries in the log)" \
  "[ $RC6 = 0 ] && grep -q 'retrying in' $T/j6.log && jq_ $T/j6.json \"r['counts']['measured']>0\" | grep -q True"
python3 "$HERE/lib/job.py" 127.0.0.1 $CPORT "$T/j7.json" $ARGS >"$T/j7.log" 2>&1; RC7=$?
check "without a budget (the default) a refusal marks the point at once, without retries (code 3 — there is no generator)" \
  "[ $RC7 = 3 ] && ! grep -q 'retrying in' $T/j7.log && grep -q 'the point was not taken: the generator is unreachable' $T/j7.log"
pkill -P $SV2 2>/dev/null; kill $SV2 $FR3 2>/dev/null; wait $SV2 $FR3 2>/dev/null

echo "== 5. report.py: a broken file is a shout, a tail is tolerated, service json files are not points =="
D=$T/res; mkdir -p "$D"
python3 - "$T/j1.json" "$D" <<'PY'
import json, sys, copy
r = json.load(open(sys.argv[1])); d = sys.argv[2]
def put(name, obj, tail=""):
    open("%s/%s" % (d, name), "w").write(json.dumps(obj) + tail)
r["stand"] = "talaria"; r["tags"] = "workload=self,case=ping"
r["rate_achieved_pct"] = 100.0      # the summary is being checked, not the host: a short run undershoots the rate
put("talaria-ping-r1000-1.json", r)
put("talaria-ping-r1000-2.json", r, '\n{"error": 1}\n')          # the tail of an old tlgen
l = copy.deepcopy(r); l["stand"] = "linux"
put("linux-ping-r1000-1.json", l)
open("%s/linux-ping-r1000-2.json" % d, "w").write(json.dumps(l)[:200])   # cut off mid-word
put("report.json", {"schema": 1, "stand": "talaria", "tags": "workload=self,case=fake", "cfg": {"rate": 5}, "counts": {}})
put("floor.json", {"schema": 1, "stand": "linux", "tags": "workload=self,case=fake", "cfg": {"rate": 5}, "counts": {}})
PY
python3 "$HERE/lib/report.py" "$D" >"$T/rep.md" 2>&1; RCR=$?
check "report.py finished (code $RCR)" "[ $RCR = 0 ]"
check "the broken file is named loudly: \"NOT PARSED: 1\"" "grep -q 'NOT PARSED: 1' $T/rep.md && grep -q 'linux-ping-r1000-2.json' $T/rep.md"
check "a tail after the JSON did not lose the point (talaria has 2 repeats)" "grep -q 'spread' $T/rep.md && python3 -c \"
import sys; sys.path.insert(0,'$HERE/lib'); import report
t={report.key(r): 1 for r in report.load('$D')}
rs=report.load('$D'); n=sum(1 for r in rs if r['stand']=='talaria')
sys.exit(0 if n==2 else 1)\""
check "report.json/floor.json were not taken for points" "! grep -q 'case=fake\|self / fake' $T/rep.md"
check "the table contains both sides and the ratio at p50" "grep -q '^| talaria' $T/rep.md && grep -q '^| linux' $T/rep.md && grep -q '| self | ping | 1000 | p50 |' $T/rep.md"

echo "== 6. rejection and errors: the reason is printed, a single error does not reject =="
D2=$T/res2; mkdir -p "$D2"
python3 - "$T/j1.json" "$D2" <<'PY'
import json, sys, copy
r = json.load(open(sys.argv[1])); d = sys.argv[2]
r["tags"] = "workload=self,case=ping"; r["rate_achieved_pct"] = 100.0
def put(name, obj): open("%s/%s" % (d, name), "w").write(json.dumps(obj))
a = copy.deepcopy(r); a["stand"] = "talaria"; a["counts"]["timeouts"] = 1     # one hung connection
put("talaria-ping-r1000-1.json", a)
b = copy.deepcopy(r); b["stand"] = "linux"
put("linux-ping-r1000-1.json", b)
c = copy.deepcopy(r); c["stand"] = "talaria"; c["cfg"]["rate"] = 2000; c["counts"]["overload"] = True
put("talaria-ping-r2000-1.json", c)
e = copy.deepcopy(r); e["stand"] = "linux"; e["cfg"]["rate"] = 2000; e["counts"]["conn_errors"] = 500
put("linux-ping-r2000-1.json", e)
PY
python3 "$HERE/lib/report.py" "$D2" >"$T/rep2.md" 2>&1
check "a single error: the point is judged and marked \"(errors 1)\"" "grep -q '^| talaria (errors 1) | 1000 ' $T/rep2.md && grep -q '| self | ping | 1000 | p50 |' $T/rep2.md"
check "overload: an asterisk WITH A REASON, the point is not compared" "grep -q '^| talaria \\\\\* (overload) | 2000 ' $T/rep2.md && ! grep -q '| self | ping | 2000 | p50 |' $T/rep2.md"
check "mass errors (500 of $S1): rejected with a reason" "grep -q '^| linux \\\\\* (errors 500 of' $T/rep2.md"
python3 "$HERE/lib/summary.py" "$D" "$D2" >"$T/sum1.md" 2>&1; RS1=$?
python3 "$HERE/lib/summary.py" "$D2" "$D" >"$T/sum2.md" 2>&1
check "summary.py finished (code $RS1)" "[ $RS1 = 0 ]"
check "the summary does not depend on the order of the directories" "diff -q $T/sum1.md $T/sum2.md >/dev/null"
python3 "$HERE/lib/summary.py" "$D" >"$T/sum3.md" 2>&1; python3 "$HERE/lib/summary.py" "$D" >"$T/sum4.md" 2>&1
check "the summary is idempotent (two runs — one text)" "diff -q $T/sum3.md $T/sum4.md >/dev/null"

echo "== 6b. counting multi-millisecond answers and rejecting a thin tail =="
# The ">1/>4 ms" column re-judges AN ALREADY TAKEN scoreboard without touching the stand — and therefore
# must fail loudly on three things at once: it must count from hist_ns,
# it must not pass the generator's debt off as events (on the top steps of the ladder
# the open loop gives millions of "answers longer than 4 ms" with a service maximum of
# a quarter of a millisecond) and it must not allow judging a percentile with
# one request beyond it.
D3=$T/res3; mkdir -p "$D3"
python3 - "$T/j1.json" "$D3" <<'PY'
import json, sys, copy
r = json.load(open(sys.argv[1])); d = sys.argv[2]
r["tags"] = "workload=self,case=ping"; r["rate_achieved_pct"] = 100.0
r["counts"]["overload"] = False; r["counts"]["timeouts"] = 0
r["counts"]["conn_errors"] = 0; r["counts"]["app_errors"] = 0


def put(name, obj):
    open("%s/%s" % (d, name), "w").write(json.dumps(obj))


def mk(stand, rate, n, svc_max, hist, p9999=200000):
    x = copy.deepcopy(r)
    x["stand"] = stand; x["cfg"]["rate"] = rate
    x["counts"]["measured"] = n
    h = {"n": n, "min_ns": 10000, "mean_ns": 50000, "max_ns": svc_max,
         "p50": 50000, "p75": 60000, "p90": 70000, "p99": 90000,
         "p99_9": 120000, "p99_99": p9999, "p99_999": p9999}
    x["latency"]["service"] = h
    x["latency"]["open_loop"] = dict(h, max_ns=max(svc_max, 9000000))
    x["hist_ns"] = hist
    return x


# RATE 1000: Talaria has 700 answers longer than 1 ms, of them 300 longer than 4 ms; on Linux
# the histogram counts THE SAME, but the service maximum is 200 µs — that is debt.
N = 2000000
put("talaria-ping-r1000-1.json",
    mk("talaria", 1000, N, 9000000, [[50000, N - 700], [1000000, 400], [4000000, 300]]))
put("linux-ping-r1000-1.json",
    mk("linux", 1000, N, 200000, [[50000, N - 700], [1000000, 400], [4000000, 300]]))
# RATE 2000: two requests stand beyond p99.99 — there is nothing to judge.
put("talaria-ping-r2000-1.json", mk("talaria", 2000, 20000, 900000,
                                    [[50000, 20000]], p9999=300000))
put("linux-ping-r2000-1.json", mk("linux", 2000, 20000, 900000,
                                  [[50000, 20000]], p9999=150000))
PY
python3 "$HERE/lib/report.py" "$D3" >"$T/rep3.md" 2>&1
check "the column counts answers longer than 1 and 4 ms from hist_ns (700 / 300)" \
  "grep -q '^| talaria | 1000 .* 700[~]* / 300[~]* |' $T/rep3.md"
check "the same count with a service maximum of 200 µs is called debt, not events" \
  "grep -q '^| linux | 1000 .* debt / debt |' $T/rep3.md"
check "a percentile with only a few requests beyond it is NOT judged" \
  "grep -q 'Not judged (only a few requests beyond the percentile' $T/rep3.md && ! grep -q '| self | ping | 2000 | p99.99 |' $T/rep3.md"
check "and the p50 of the same point is judged as usual" \
  "grep -q '| self | ping | 2000 | p50 |' $T/rep3.md"
python3 "$HERE/lib/summary.py" "$D3" >"$T/sum5.md" 2>&1; RS5=$?
check "the summary finished (code $RS5) and prints the events column" \
  "[ $RS5 = 0 ] && grep -q '>1/>4 ms ours' $T/sum5.md"
check "the summary counts the thin-tail rejection as a separate line" \
  "grep -q 'a few requests beyond them' $T/sum5.md"

echo "== 6a. compare.py: repeats by median, not by the last file =="
# An A/B on EC2 is judged by compare.py; with three repeats the LAST file read was
# taken — one random of the three. Here the third AFTER repeat is an outlier of x10:
# the median does not see it (a ratio of 1.00), the "last file" would have given 10.
DA=$T/cmpA; DB=$T/cmpB; mkdir -p "$DA" "$DB"
python3 - "$T/j1.json" "$DA" "$DB" <<'PY'
import json, sys, copy
r = json.load(open(sys.argv[1])); a, b = sys.argv[2], sys.argv[3]
r["stand"] = "talaria"; r["tags"] = "workload=self,case=ping"; r["rate_achieved_pct"] = 100.0
for i in (1, 2, 3):
    open("%s/talaria-ping-r1000-%d.json" % (a, i), "w").write(json.dumps(r))
    x = copy.deepcopy(r)
    if i == 3:
        for k in ("p50", "p99", "p99_9", "p99_99"):
            x["latency"]["service"][k] *= 10; x["latency"]["open_loop"][k] *= 10
    open("%s/talaria-ping-r1000-%d.json" % (b, i), "w").write(json.dumps(x))
PY
python3 "$HERE/lib/compare.py" "$DA" "$DB" talaria >"$T/cmp.txt" 2>&1; RCC=$?
check "compare.py finished (code $RCC) and found the point" "[ $RCC = 0 ] && grep -q '^self *ping *talaria *1000 p50' $T/cmp.txt"
check "an outlier in one repeat of three is invisible: the p50 ratio is 1.00" "grep '^self *ping *talaria *1000 p50' $T/cmp.txt | grep -q ' 1.00$'"
check "run-remote-ab.sh: the syntax (sh -n)" "sh -n $HERE/run-remote-ab.sh && sh -n $HERE/infra/talaria-cmdline.sh"

# THE CLEANUP PICKS UP THE LEFTOVERS OF AN INTERRUPTED IMAGE BUILD — and does so NARROWLY.
# A suite killed from outside leaves an orphaned `mk-ami.sh`: it finishes without its
# parent and leaves a volume, a snapshot and an AMI that are in neither terraform's state
# nor under the stand's tag (once the cleanup said "zero" while the volume and the snapshot
# stayed). The selection must go BY THE NAME `tlbench-*`: the tag `Project=talaria`
# is shared with single probes, and cleaning up by it once killed someone else's ENA
# probe — that must not come back.
check "stand-down.sh: the syntax (sh -n)" "sh -n $HERE/infra/stand-down.sh"
check "stand-down.sh: picks up the image build's orphans (volume, snapshot, AMI, instance)" \
  "grep -q 'tag:Name,Values=tlbench-\*' $HERE/infra/stand-down.sh \
   && grep -q 'delete-snapshot' $HERE/infra/stand-down.sh \
   && grep -q 'deregister-image' $HERE/infra/stand-down.sh \
   && grep -q 'delete-volume' $HERE/infra/stand-down.sh"
check "stand-down.sh: does NOT clean up by the shared tag Project=talaria (those are other probes)" \
  "! grep -qE 'tag:Project,Values=talaria([^-]|$)' $HERE/infra/stand-down.sh"

echo "== 6b. the Linux stand: an unprivileged server in a chroot =="
# Three times in a row mysql on the Linux stand gave "ceiling 0 req/s": uid 27 does not exist on
# the host (User= -> status=217/USER), and the tree's root from mktemp -d was 0700.
# Here is a tree built THE SAME way as in stand-up.sh: the root must
# go into the archive traversable; the unit must change the user INSIDE the chroot.
Y=$HERE/infra/ansible/stand.yml
check "stand.yml: the server goes through chroot --userspec rather than User=/Group=" \
  "grep -q 'chroot --userspec={{ server_uid' $Y && ! grep -q '^ *User=' $Y && ! grep -q '^ *Group=' $Y"
check "stand.yml: the workload's root is made traversable AFTER unpacking, there is a LISTEN post-condition" \
  "grep -q 'loop: \[\"\", proc, sys, dev, tmp\]' $Y && grep -q 'ss -Hltn' $Y && grep -q 'server_port' $HERE/infra/stand-up.sh"
check "stand.yml: /dev and /sys into the workload's root use rbind (otherwise /dev/shm is empty and postgres does not start)" \
  "grep -q 'mount --rbind /dev' $Y && grep -q 'mount --rbind /sys' $Y && ! grep -q 'mount --bind /dev' $Y"
check "the Graviton stand gives both sides the whole instance for postgresql (SMP_ARM64=4 into mk-ami and terraform, LINUX_CORES_ARM64 into the Linux unit), the x86 one keeps SMP=2" \
  "grep -q 'CPUAffinity={{ server_cores | default(dp_cores) }}' $Y && (. $HERE/workloads/postgresql/meta.env; [ \"\$SMP_ARM64\" = 4 ] && [ \"\$LINUX_CORES_ARM64\" = 0-3 ] && [ \"\$SMP\" = 2 ]) && grep -q 'SMP=\$SMP_ARM64' $HERE/infra/mk-ami.sh && grep -q 'LINUX_CORES_ARM64' $HERE/infra/stand-up.sh"
check "stand.yml: isolcpus only for the generator (a server on isolated cores lives on ONE)" \
  "grep -q \"'isolcpus=' ~ dp_cores if role == 'loadgen'\" $Y && ! grep -q '^ *grubby.*args=\"isolcpus' $Y"
check "run-remote.sh: the Linux server's counters (statd :9999) are taken over the PUBLIC address (over the private one it was \"the snapshot is unavailable\" in every suite)" \
  "grep -q 'metrics-remote.sh\" \"\$stand\" \"\$srvpub\"' $HERE/run-remote.sh && ! grep -q 'metrics-remote.sh\" \"\$stand\" \"\$srv\"' $HERE/run-remote.sh"
# statd on AL2023 NEVER listened: there is no nc/ncat in the image, `snap | nc -l` died
# instantly, and the runner got "Connection refused" (every suite before 06.09). The gate is
# LIVE: statd comes up on its own port, the metrics-remote.sh client takes two
# snapshots in a row, and each must contain the thread placement `psr cpuN:`.
SD_FILE=$HERE/infra/ansible/files/statd.sh
check "statd.sh does not depend on nc/ncat (they are absent on AL2023)" "! grep -v '^ *#' $SD_FILE | grep -q '\\bnc -l\\|\\bncat\\b'"
sleep 30 & SD_PID=$!
sh "$SD_FILE" $SD_PID $DPORT >"$T/statd.log" 2>&1 & SD_SRV=$!
wait_port $DPORT || fail "statd did not come up on :$DPORT"
: >"$T/statd.txt"
STATD_PORT=$DPORT sh "$HERE/lib/metrics-remote.sh" linux 127.0.0.1 "$T/statd.txt" before
STATD_PORT=$DPORT sh "$HERE/lib/metrics-remote.sh" linux 127.0.0.1 "$T/statd.txt" after
kill $SD_SRV $SD_PID 2>/dev/null; wait $SD_SRV $SD_PID 2>/dev/null
check "statd: two snapshots in a row through the metrics-remote.sh client, each with the psr thread placement" \
  "[ \$(grep -c '^psr cpu[0-9]*:' $T/statd.txt) = 2 ] && ! grep -q 'the snapshot is unavailable' $T/statd.txt"
TD=$(mktemp -d); mkdir -p "$TD/opt"; chmod 755 "$TD"; tar czf "$T/tree.tgz" -C "$TD" .; rm -rf "$TD"
check "the root of a tree built as in stand-up.sh (mktemp -d + chmod 755 + tar) is 0755 in the archive" \
  "grep -q 'chmod 755 \"\$TREE\"' $HERE/infra/stand-up.sh && tar tvzf $T/tree.tgz | head -1 | grep -q '^drwxr-xr-x'"

echo "== 6c. the Talaria stand: the role of both instances is verified from the console =="
# A postgresql run ec2all-20260906-110825 spent half an hour in vain: the Talaria
# generator lost its DHCP DISCOVER, did not read its user-data and came up as a SECOND
# SERVER, while the harness waited for tlgen --serve for 600 s per point. The post-condition of
# stand-up.sh reads the manifest knob line from the serial of both instances;
# here it is run against a fake `aws` that serves a console by instance id.
RW=$T/role; mkdir -p "$RW/bin" "$RW/stand"
cat > "$RW/bin/aws" <<'A'
#!/bin/sh
# ec2 get-console-output --instance-id <id> ... -> the console file $RW_CON/<id>
while [ $# -gt 0 ]; do [ "$1" = --instance-id ] && { cat "$RW_CON/$2" 2>/dev/null; exit 0; }; shift; done
exit 1
A
chmod +x "$RW/bin/aws"
role_case() { # <name> <the server's console> <the generator's console>
  mkdir -p "$RW/$1"; printf '%b' "$2" > "$RW/$1/i-srv"; printf '%b' "$3" > "$RW/$1/i-gen"
  RW_CON=$RW/$1 PATH=$RW/bin:$PATH ROLE_WAIT_S=0 sh "$HERE/infra/talaria-role.sh" "$RW/stand" i-srv i-gen > "$RW/$1.log" 2>&1
}
SRV='netsys: net0 dhcp lease 10.0.1.5/24 gw=10.0.1.1 dns=10.0.0.2\ntalaria: the knob talaria.manifest = /cell.manifest — the manifest\n'
GEN='netsys: net0 dhcp lease 10.0.1.6/24 gw=10.0.1.1 dns=10.0.0.2\ntalaria: the knob talaria.manifest = /loadgen.manifest — the manifest\n'
BAD='netsys: dhcp TIMEOUT after 8 s — falling back to a static address (user-data/IMDS will be UNREACHABLE — the machine will come up without a role)\ntalaria: the knob talaria.manifest = /cell.manifest — the manifest\n'
check "the roles are right -> the stand is accepted, the lease is printed" \
  "role_case ok '$SRV' '$GEN' && grep -q 'loadgen (i-gen): role /loadgen.manifest, lease 10.0.1.6' $RW/ok.log"
check "the generator came up as a server (dhcp TIMEOUT) -> a REFUSAL with the reason from the console" \
  "! role_case bad '$SRV' '$BAD' && grep -q 'loadgen (i-gen): THE WRONG ROLE' $RW/bad.log && grep -q 'dhcp TIMEOUT' $RW/bad.log"
check "the console is empty -> a REFUSAL rather than a silent bring-up" \
  "! role_case empty '$SRV' '' && grep -q 'the console is empty' $RW/empty.log"

# A Talaria counter snapshot on EC2 (metrics-remote.sh) is read from the same console,
# and without `--latest` EC2 returns the snapshot taken at BOOT: ec2ab-mysql-20260906-131019
# kept in every "before/after" the tail of the boot stamped 10:22 — there were no counters
# for the run at any point. The fake `aws`: without `--latest` the boot, with it the tail.
CW=$T/con; mkdir -p "$CW/bin"
cat > "$CW/bin/aws" <<'A'
#!/bin/sh
latest=0; for a in "$@"; do [ "$a" = --latest ] && latest=1; done
[ $latest = 1 ] && echo "stat: a FRESH console tail" || echo "talaria: boot (the snapshot taken at boot)"
A
chmod +x "$CW/bin/aws"
check "metrics-remote.sh on EC2 takes a FRESH console tail (get-console-output --latest) rather than the boot" \
  "PATH=$CW/bin:\$PATH TALARIA_ID=i-x sh $HERE/lib/metrics-remote.sh talaria 1.2.3.4 $CW/m.txt before && grep -q 'a FRESH console tail' $CW/m.txt && ! grep -q 'the snapshot taken at boot' $CW/m.txt"
check "stand-up.sh calls talaria-role.sh before announcing \"the stand is up\"" \
  "grep -q 'talaria-role.sh.*|| exit 1' $HERE/infra/stand-up.sh"

echo "== 6d. the phase's console: the snapshots of the 64 KB window are spliced without duplicates and without silent loss =="
# Once the diagnostics were captured with ONE console snapshot at the end, i.e. with
# the last 64 KB window (`console-PA.txt` exactly 65582 B) — the first analysis pass
# was lost entirely. Now the phase polls the window itself
# (`infra/console-watch.sh`), and the splice (`lib/console-splice.py`) must: not
# append the overlap a second time (the console is full of IDENTICAL `stat:` blocks —
# idle snapshots with identical counters) and not swallow a gap silently
# if the window moved entirely between snapshots.
CS=$T/console; mkdir -p "$CS/bin"
# The stream is like a real console: the `stat:` blocks resemble one another (inside a block
# the lines repeat), but one block is distinguishable from another — that is how the kernel prints them.
python3 -c 'import sys; sys.stdout.write("".join("stat: snapshot %d line %d counters identical\n" % (i, j) for i in range(12) for j in range(40)))' > "$CS/stream"
head -c 12000 "$CS/stream" > "$CS/w1"
tail -c +6001 "$CS/stream" | head -c 12000 > "$CS/w2"
tail -c +12001 "$CS/stream" > "$CS/w3"
for w in w1 w2 w3; do python3 "$HERE/lib/console-splice.py" "$CS/acc.txt" < "$CS/$w" 2>>"$CS/splice.log"; done
check "three overlapping windows spliced into the ORIGINAL stream (not a byte extra)" \
  "cmp -s $CS/acc.txt $CS/stream"
python3 "$HERE/lib/console-splice.py" "$CS/acc.txt" < "$CS/w3" 2>>"$CS/splice.log"
check "the same snapshot once more — the file did not grow (a window is not appended twice)" \
  "cmp -s $CS/acc.txt $CS/stream"
# THE PATHOLOGY FOR WHICH THE SPLICE ORDER WAS CHOSEN: byte-identical blocks
# (idle snapshots with identical counters — a former instrument defect).
# Here it is impossible in principle to tell how many there were, and the splice may eat
# an extra period; what it may NOT do is glue a piece into the wrong place, and
# that is what is checked: the accumulator must end with the last window.
python3 -c 'import sys; sys.stdout.write(("stat: snapshot idle counters identical\n" * 40) * 12)' > "$CS/same"
head -c 12000 "$CS/same" > "$CS/s1"; tail -c +6001 "$CS/same" | head -c 12000 > "$CS/s2"
python3 "$HERE/lib/console-splice.py" "$CS/same.txt" < "$CS/s1" 2>>"$CS/splice.log"
python3 "$HERE/lib/console-splice.py" "$CS/same.txt" < "$CS/s2" 2>>"$CS/splice.log"
check "identical blocks: the accumulator ends with the last window and is no shorter than it" \
  "tail -c 12000 $CS/same.txt | cmp -s - $CS/s2 && [ \"\$(wc -c < $CS/same.txt)\" -ge 12000 ]"
printf 'AAAA' > "$CS/g1"; printf 'BBBB' > "$CS/g2"
python3 "$HERE/lib/console-splice.py" "$CS/gap.txt" < "$CS/g1" 2>/dev/null
GAPMSG=$(python3 "$HERE/lib/console-splice.py" "$CS/gap.txt" < "$CS/g2" 2>&1)
check "the window moved entirely -> the GAP is named and the new part is not lost (it used to be a silent splice)" \
  "echo '$GAPMSG' | grep -q GAP && grep -q GAP $CS/gap.txt && grep -q BBBB $CS/gap.txt"
cat > "$CS/bin/aws" <<'AWSFAKE'
#!/bin/sh
# ec2 get-console-output --latest -> a piece of the console from $CON_PART
cat "$CON_PART" 2>/dev/null
AWSFAKE
chmod +x "$CS/bin/aws"
printf 'stat: part 1\nstat: part 2\n' > "$CS/p1"
printf 'stat: part 2\nstat: part 3\n' > "$CS/p2"
CON_PART=$CS/p1 PATH=$CS/bin:$PATH sh "$HERE/infra/console-watch.sh" once i-x "$CS/w.txt" >/dev/null 2>&1
CON_PART=$CS/p2 PATH=$CS/bin:$PATH sh "$HERE/infra/console-watch.sh" once i-x "$CS/w.txt" >/dev/null 2>&1
check "console-watch.sh once appends the window to the accumulator (parts 1,2,3 — once each)" \
  "[ \"\$(grep -c 'stat: part' $CS/w.txt)\" = 3 ]"
check "ab-phase.sh captures the console itself while the node is alive and takes the LAST snapshot" \
  "grep -q 'console-watch.sh\" loop' $HERE/ab-phase.sh && grep -q 'console-watch.sh\" once' $HERE/ab-phase.sh"

echo "== 7. the actor matrix's accumulator != the runner's file =="
A=$REPO/services/actors
ACC=$(grep -a -o 'out="[^"]*"' "$A/matrix.sh" | head -1 | sed 's/out=//; s/"//g; s|.*/||')
RUN=$(grep -a -o 'tee "[^"]*"' "$A/run.sh" | head -1 | sed 's/tee //; s/"//g; s|.*/||')
# without eval: the names contain "$st" from the script
if [ -n "$ACC" ] && [ -n "$RUN" ] && [ "$ACC" != "$RUN" ]; then ok "matrix.sh accumulates in '$ACC', run.sh writes to '$RUN' — different names"
else fail "matrix.sh accumulates in '$ACC', run.sh writes to '$RUN' — the accumulator coincided with the runner's file"; fi

echo "== 8. the ClickHouse initramfs stamp changes with one byte in q.sql =="
CH=$REPO/services/clickhouse
if command -v mke2fs >/dev/null && command -v debugfs >/dev/null; then
  R=$T/root; mkdir -p "$R"; printf '<x/>' > "$R/config.xml"; printf '<u/>' > "$R/users.xml"; printf 'SELECT 1\n' > "$R/q.sql"
  mke2fs -q -F -t ext2 -d "$R" "$T/a.img" 1M >/dev/null 2>&1
  printf 'SELECT 2\n' > "$R/q.sql"
  mke2fs -q -F -t ext2 -d "$R" "$T/b.img" 1M >/dev/null 2>&1
  SA=$("$CH/ir-stamp.sh" "$T/a.img" "$CH/ir-stamp.sh"); SB=$("$CH/ir-stamp.sh" "$T/b.img" "$CH/ir-stamp.sh"); SA2=$("$CH/ir-stamp.sh" "$T/a.img" "$CH/ir-stamp.sh")
  check "the stamp is deterministic ($SA)" "[ \"$SA\" = \"$SA2\" ] && [ ${#SA} = 16 ]"
  check "a change of q.sql changes the stamp ($SA -> $SB)" "[ \"$SA\" != \"$SB\" ]"
  check "run-linux-vm.sh verifies the stamp before reusing the initramfs" \
    "grep -q 'initramfs-ch.stamp' $CH/run-linux-vm.sh && grep -q 'ir-stamp.sh' $CH/run-linux-vm.sh"
else
  echo "  SKIP  no mke2fs/debugfs"
fi

echo
echo "selfcheck: PASS=$PASS FAIL=$FAIL"
[ "$FAIL" = 0 ]
