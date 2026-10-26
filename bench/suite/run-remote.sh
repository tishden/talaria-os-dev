#!/bin/sh
# A suite run on the EC2 stand: Talaria against Talaria and Linux against Linux.
#
#   run-remote.sh <workload> [case label]
#   env: STAND_DIR= (where the stand's state lives, by default ~/.talaria-lab/stand)
#        DUR= WARM= CONNS= REPEAT= RATES= CAP=0 (do not measure the ceiling when RATES= is set)
#
# The stand is brought up by stand-up.sh (terraform + ansible) and torn down by stand-down.sh.
# Here there is only the run: it creates and deletes no resources, so it can be
# started as many times in a row as needed while working through the numbers.
#
# Both sides are measured with ONE instrument: the load generator is the very same tlgen
# binary, brought up as a job receiver (`--serve 9100`). On Talaria it runs
# from an image by the /loadgen.manifest manifest, on Linux from a systemd unit. The job
# goes over TCP, the report comes back as JSON.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
WL=${1:?workload}
ONLY=${2:-}
WD="$HERE/workloads/$WL"
[ -d "$WD" ] || { echo "no workload $WL"; exit 1; }
. "$WD/meta.env"

STAND_DIR=${STAND_DIR:-$HOME/.talaria-lab/stand}
[ -f "$STAND_DIR/stand.env" ] || { echo "the stand is not up — run bench/suite/infra/stand-up.sh"; exit 1; }
. "$STAND_DIR/stand.env"
# The runner's address may have changed since the stand came up (VPN) — then the stand is
# unreachable entirely, and the run loses everything it has not taken yet
# (a lost Linux side in one run). It costs a second.
"$HERE/infra/sg-refresh.sh" || true

DUR=${DUR:-60}; WARM=${WARM:-10}; CONNS=${CONNS:-${CONNS:-8}}; REPEAT=${REPEAT:-3}
STAMP=$(date +%Y%m%d-%H%M%S)
OUT=${OUTDIR:-$HERE/results/ec2-$WL-$STAMP}
mkdir -p "$OUT"
echo "== stand $STAND_TYPE in $STAND_AZ, workload $WL, results $OUT"

# A job for the generator: an argument line over TCP to :9100, the answer is JSON.
# A connection drop and an unparseable answer MARK the point with a stub rather than bringing the
# run down (lib/job.py; the self-check is selfcheck.sh). The runner -> generator path
# goes over the internet (and through the runner's VPN, which reconnects
# several times a day for 2-4 minutes, answering with a refusal meanwhile): a refusal and
# a reset on that path are RETRIED within the JOB_RETRY_S budget rather than costing a point.
job() {   # $1 the generator's address, $2 the report file, then the arguments
    gen=$1; out=$2; shift 2
    JOB_RETRY_S=${JOB_RETRY_S:-600} python3 "$HERE/lib/job.py" "$gen" 9100 "$out" "$@"
}

# THE CEILING IS A MEDIAN OF SEVERAL PROBES, not one probe.
#
# The same fix as in `run-local.sh`, and for the same reason: a single
# probe right after bring-up measures the STATE (the cache, the checkpointer, EBS
# warm-up), not the ceiling; locally it wandered by 56 %. THE WHOLE ladder of rates is built
# from the ceiling, and the SMALLER of the two stands is taken — one unlucky
# probe shifts every point of the run. On EC2 the price of a mistake is higher: a re-run costs
# money and stand time.
# THE CASE'S ARGUMENTS GO THROUGH `eval`, AS THE LADDER'S DO. In `CASES` the quotes are
# part of the syntax (`--query 'SELECT 1'`), and the ladder removes them with `eval`;
# here `$3` used to be substituted bare, and the receiver got `'SELECT` and `1'` with the
# quotes INSIDE the tokens. That only worked by accident: job.py glued the
# tokens back into a string with the same quotes. Now job.py quotes arguments
# with spaces itself, and both paths must produce identical tokens.
capacity() {   # $1 generator, $2 server, $3 the case's arguments
    probes=
    n=1
    while [ "$n" -le "${PROBE_N:-3}" ]; do
        # The probe stays on disk (`probe-<stand>-<label>-<n>.json`): a ceiling of
        # "0 req/s" without a report is a riddle, with a report it is a diagnosis (on mysql the
        # Linux side had a ceiling of 0 three times in a row, and there was nothing to find out
        # why from: the file was overwritten and erased).
        pf="$OUT/probe-${stand:-x}-${label:-x}-$n.json"
        eval "job '$1' '$pf' --proto $PROTO --host $2 --port $PORT $3 \
            --conns $CONNS --rate 0 --duration 6 --warmup 2 --spin" >/dev/null 2>&1 || true
        v=$(python3 -c "
import json, sys
try:
    r = json.load(open('$pf'))
    v = int(r.get('throughput_rps', 0))
    if v == 0:
        c = r.get('counts', {})
        sys.stderr.write('     probe $n: 0 req/s — error=%r conn_errors=%s timeouts=%s\n'
                         % (r.get('error'), c.get('conn_errors'), c.get('timeouts')))
    print(v)
except Exception as e:
    sys.stderr.write('     probe $n: the report did not parse (%s)\n' % e); print(0)")
        probes="$probes $v"
        n=$((n + 1))
    done
    python3 -c "
v = sorted(int(x) for x in '$probes'.split())
import sys
sys.stderr.write('     ceiling probes:%s, spread %d %%\n' % ('$probes', 0 if not v or v[-1] == 0 else (v[-1]-v[0])*100//max(v[-1],1)))
print(v[len(v)//2] if v else 0)"
}

# ── pass 0: the warm-up — the data without which the cases have nothing to measure ──
#
# For the databases the `pk` case reads a table created by `WARMUP_CMD` from
# meta.env; locally run-local.sh executes it, while here it was not executed
# AT ALL — and the `pk` ceiling was 0 req/s on both sides (the
# ec2all-20260905-133810 suite), the ladder "empty". The command is written for the local
# tlgen (`"$TLGEN" ... --out /dev/null`); here `$TLGEN` is a function that
# sends the same job to the receiver on the stand, dropping `--out`, and `$GUEST_IP`
# is the server's private address.
warm_remote() {   # $1 generator, $2 server
    [ -n "${WARMUP_CMD:-}" ] || return 0
    GUEST_IP=$2 PORT=$PORT TLGEN=warm_job WARM_GEN=$1 WARM_OUT="$OUT/.warm.json" sh -c "
warm_job() {
    args=; skip=0
    for a in \"\$@\"; do
        [ \$skip = 1 ] && { skip=0; continue; }
        [ \"\$a\" = --out ] && { skip=1; continue; }
        args=\"\$args '\$a'\"
    done
    eval \"JOB_RETRY_S=\${JOB_RETRY_S:-600} python3 '$HERE/lib/job.py' '\$WARM_GEN' 9100 '\$WARM_OUT' \$args\"
}
$WARMUP_CMD" || echo "   the warm-up on $2 did not work"
}
for stand in talaria linux; do
    # The `:-` is mandatory: under `set -u` a stand.env WITHOUT one side's addresses (phase B of an
    # A/B — Talaria only) killed the run at the eval itself rather than at the check below;
    # that is how phase B of mysql (ec2ab-mysql-20260906-131019) failed at every point.
    eval "gen=\${${stand}_loadgen_pub:-}"; eval "srv=\${${stand}_server_ip:-}"
    [ -n "${gen:-}" ] && [ -n "${srv:-}" ] || continue
    echo "-- warm-up $stand"
    warm_remote "$gen" "$srv"
done
rm -f "$OUT/.warm.json"

# ── pass 1: the ceilings of both sides ───────────────────────────────────────
# CAP=0 with RATES= given means do not measure the ceiling: its only consumer
# here is the ladder, and three probes at the ceiling (3x10 s at 53 thousand req/s
# on postgresql) put twice as many round trips into the cumulative counters of the stat block
# as the measurement at a low rate itself, and the serial analysis then judges the wrong thing.
# ab-phase.sh runs one case per call (ONLY=) into the same OUT: truncating the whole file kept
# only the last case's ceiling, and genboard's table printed "—" for pk under the Linux generator
CAPS="$OUT/.caps"
if [ -n "${ONLY:-}" ] && [ -f "$CAPS" ]; then
    grep -v "^$ONLY " "$CAPS" > "$CAPS.tmp" || true; mv "$CAPS.tmp" "$CAPS"
else
    : > "$CAPS"
fi
old_ifs=$IFS
[ "${CAP:-1}" = 0 ] && [ -n "${RATES:-}" ] && echo "-- the ceiling is not measured (CAP=0, the ladder is given by RATES=)"
for stand in talaria linux; do
    [ "${CAP:-1}" = 0 ] && [ -n "${RATES:-}" ] && break
    # THE CONTROL CHANNEL IS PUBLIC, THE LOAD IS PRIVATE.
    #
    # The job is sent to the generator by THIS machine, which is outside the VPC, and a private address
    # is not routable from it: an attempt to give a job silently ended up as
    # "ceiling 0 req/s". The security group opens ports 9000-9999 for the operator's
    # address anyway — that is exactly the design.
    # The load itself still goes INSIDE the VPC: the generator reaches the server
    # by its private address (`--host $srv`), and not a single packet of the public network
    # enters the measurement.
    eval "gen=\${${stand}_loadgen_pub:-}"; eval "srv=\${${stand}_server_ip:-}"
    [ -n "${gen:-}" ] && [ -n "${srv:-}" ] || { echo "-- $stand: no addresses, skipping"; continue; }
    IFS='|'
    for c in $CASES; do
        IFS=$old_ifs
        label=${c%%:*}; args=${c#*:}
        if [ -z "$ONLY" ] || [ "$ONLY" = "$label" ]; then
            cap=$(eval "capacity $gen $srv \"$args\"")
            echo "$label $stand $cap" >> "$CAPS"
            echo "   ceiling $stand/$label: $cap req/s"
        fi
        IFS='|'
    done
    IFS=$old_ifs
done

ladder_for() {
    [ -z "${RATES:-}" ] || { echo "$RATES"; return; }
    python3 - "$CAPS" "$1" <<'PY'
import sys
caps = [int(p[2]) for p in (l.split() for l in open(sys.argv[1]))
        if len(p) == 3 and p[0] == sys.argv[2] and int(p[2]) > 0]
print(" ".join(str(int(min(caps) * f)) for f in (0.1, 0.25, 0.5, 0.75, 0.9)) if caps else "")
PY
}

# ── pass 2: one ladder for both sides ────────────────────────────────────────
for stand in talaria linux; do
    eval "gen=\${${stand}_loadgen_pub:-}"; eval "srv=\${${stand}_server_ip:-}"   # see above: control is public, the load is private
    # The Linux server's counters (statd, :9999) use the same control channel, i.e. the
    # PUBLIC address: with the private one this machine got "the snapshot is unavailable" in
    # EVERY metrics file of every suite on EC2 — the Linux counters were never taken,
    # and the placement of threads on cores (`psr`) was blind.
    [ -n "${gen:-}" ] && [ -n "${srv:-}" ] || continue
    # The addresses are checked FIRST and the public one used AFTERWARDS: in phase B of an
    # A/B run only the Talaria side is brought up, and `$linux_server_pub` under `set -u`
    # killed the script — "linux_server_pub: unbound variable". The numbers by
    # then were already taken, while the run counted as an ERROR, and half of the
    # A/B (B/find, B/insert, B/ping, B/pk, B/select1) went red.
    eval "srvpub=\${${stand}_server_pub:-}"; [ -n "${srvpub:-}" ] || srvpub=$srv
    echo "-- stand $stand (generator $gen -> server $srv)"
    gendown=0
    IFS='|'
    for c in $CASES; do
        IFS=$old_ifs
        label=${c%%:*}; args=${c#*:}
        if [ -z "$ONLY" ] || [ "$ONLY" = "$label" ]; then
            ladder=$(ladder_for "$label")
            [ -n "$ladder" ] || { echo "   $label: the ladder is empty"; IFS='|'; continue; }
            echo "   $label: ladder $ladder"
            for rate in $ladder; do
                r=1
                while [ "$r" -le "$REPEAT" ]; do
                    f="$OUT/$stand-$label-r$rate-$r.json"
                    TALARIA_ID="${talaria_server_id:-}" \
                        "$HERE/lib/metrics-remote.sh" "$stand" "$srvpub" "$OUT/metrics-$stand-$label-r$rate-$r.txt" before || true
                    echo "    $rate (repeat $r):"
                    if eval "job $gen '$f' --proto $PROTO --host $srv --port $PORT $args \
                        --conns $CONNS --rate $rate --duration $DUR --warmup $WARM --spin \
                        --name '$WL/$label' --stand $stand \
                        --tags 'workload=$WL,case=$label,stand_type=$STAND_TYPE,repeat=$r'"
                    then gendown=0
                    else [ $? = 3 ] && gendown=$((gendown + 1)) || gendown=0
                    fi
                    TALARIA_ID="${talaria_server_id:-}" \
                        "$HERE/lib/metrics-remote.sh" "$stand" "$srvpub" "$OUT/metrics-$stand-$label-r$rate-$r.txt" after || true
                    r=$((r + 1))
                    # THE RECEIVER IS DEAD — ABANDON THE SIDE RATHER THAN SPEND THE BUDGET.
                    # Two points in a row with "the generator is unreachable" is not the network
                    # between us (the retry budget cures that) but a dead agent;
                    # the rest of the ladder will give exactly the same stubs at 600 s
                    # each (one scoreboard: mysql, 4.9 hours, 12 points of
                    # 40). We stop LOUDLY so that it is visible in the
                    # run's log rather than in the summary's empty cells.
                    if [ "$gendown" -ge 2 ]; then
                        echo "!! the $stand generator ($gen) has not answered for two points in a row — the rest of this side's ladder IS NOT TAKEN"
                        break
                    fi
                done
                [ "$gendown" -ge 2 ] && break
            done
        fi
        [ "$gendown" -ge 2 ] && { IFS=$old_ifs; break; }
        IFS='|'
    done
    IFS=$old_ifs
    [ "$gendown" -ge 2 ] && echo "!! the $stand side was taken partially: the generator's agent did not answer"
done

cp "$STAND_DIR/stand.env" "$OUT/stand.env" 2>/dev/null || true
python3 "$HERE/lib/report.py" "$OUT" || true
# A failure to take the counters is reported LOUDLY. Seven suites passed with a polite "the snapshot
# is unavailable" in every file (first the private address, then statd without nc), and
# nobody noticed, because the file is in place and the run is green.
bad=$(grep -l -a 'the snapshot is unavailable\|the instance console is unavailable' "$OUT"/metrics-*.txt 2>/dev/null | wc -l)
all=$(ls "$OUT"/metrics-*.txt 2>/dev/null | wc -l)
[ "$bad" = 0 ] || echo "!! the counters were NOT TAKEN in $bad of $all metrics-*.txt files — analysis without them is blind"
echo "== done: $OUT"
