#!/bin/sh
# AN A/B OF ONE KERNEL KNOB ON EC2: bring the stand up, run the workload as is (A),
# append a candidate tail to Talaria's cmdline, run the same ladder again (B),
# tear the stand down and put the comparison next to the results.
#
#   run-remote-ab.sh <workload> "<cmdline tail for B>"
#   env: KERN=      — the candidate kernel for the AMI (by default kernel/target/...,
#                     keep the candidate build in ANOTHER target: see tools/mk-tboot.sh)
#        AB_KEEP=1  — do not tear the stand down after the run (to look at serial, to finish by hand)
#        AB_ROLES=  — which Talaria nodes get tail B (default "server loadgen")
#        and everything run-remote.sh (DUR/WARM/CONNS/REPEAT) and stand-up.sh understand
#
# Why: a change to the network path (the NIC's line, parking, the quantum) cannot be
# judged on EC2 otherwise — there is no ENA locally, and "it got better" without two runs on ONE stand
# within ONE hour is a feeling (the stand's floor wanders between boots).
#
# How to be honest: A runs on BOTH sides (Linux is the anchor of the same hour), B only on
# Talaria, OVER LADDER A: each case's rates are taken from the file names of A,
# otherwise B would build its own ladder from its own ceiling and the points would not match.
# Tail B is set through infra/talaria-cmdline.sh (stop -> user-data -> start,
# with a check in serial that the kernel read the new tail). The variable's key is
# terraform's `TF_VAR_talaria_extra` for A, if A too needs a tail.
set -u
WL=${1:?workload}
EXTRA=${2:?the cmdline tail for B, e.g. "talaria.netirq=1"}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
export STAND_DIR=${STAND_DIR:-$HOME/.talaria-lab/stand}
. "$HERE/lib/standlock.sh"; stand_lock "$@"
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
KERN=${KERN:-$REPO/kernel/target/x86_64-talaria/release/talaria}
export KERN
[ -f "$KERN" ] || { echo "no kernel $KERN"; exit 1; }
STAMP=$(date +%Y%m%d-%H%M%S)
ALL="$HERE/results/ec2ab-$WL-$STAMP"
mkdir -p "$ALL"
LOG="$ALL/log.txt"; : > "$LOG"
say() { echo "$*" | tee -a "$LOG"; }

say "== an A/B on EC2 -> $ALL"
say "   workload $WL, B = \"$EXTRA\""
say "   kernel: $KERN ($(sha256sum "$KERN" | cut -c1-16))"
{ echo "workload=$WL"; echo "extra_b=$EXTRA"; echo "kernel=$KERN"; sha256sum "$KERN"; } > "$ALL/ab.env"

if [ -f "$STAND_DIR/stand.env" ]; then
    say "== a live stand was found — tearing it down before the start"
    "$HERE/infra/stand-down.sh" >> "$LOG" 2>&1 || true
fi

RED=0
t0=$(date +%s)
if ! "$HERE/infra/stand-up.sh" "$WL" >> "$LOG" 2>&1; then
    say "== STAND ERROR (see $STAND_DIR/mk-ami-$WL.log)"; RED=1
else
    say "   the stand came up in $(( $(date +%s) - t0 )) s"
    say "== A ($(date +%H:%M:%S)): as is, both sides"
    OUTDIR="$ALL/A" "$HERE/run-remote.sh" "$WL" >> "$LOG" 2>&1 || { say "   RUN ERROR A"; RED=1; }

    if [ $RED = 0 ]; then
        # The tail goes to BOTH Talaria nodes (AB_ROLES="server loadgen"): the generator
        # is Talaria too, and its path for receiving the answer enters the measured latency
        # just as a Linux generator's interrupts enter it. The system is judged
        # as a whole, the way it will go into the suite with the new default.
        say "== tail B: $EXTRA -> ${AB_ROLES:-server loadgen} ($(date +%H:%M:%S))"
        okb=1
        for role in ${AB_ROLES:-server loadgen}; do
            APPEND=1 ROLE=$role "$HERE/infra/talaria-cmdline.sh" "$EXTRA" >> "$LOG" 2>&1 || { say "   $role: the tail did not take"; okb=0; }
        done
        # AFTER TAIL B THE GENERATOR MUST ANSWER — AND THAT IS CHECKED WITH ONE
        # SHORT JOB, NOT WITH A LADDER.
        #
        # Tail B goes to BOTH Talaria nodes, and a knob may break exactly the
        # generator: `talaria.netyield=0` (a network waiter does not yield the core) left
        # tlgen --serve without accepting connections. Every point after that honestly
        # spent its retry budget (600 s) — ten points, an hour and a half of
        # stand time and zero numbers. The check costs a minute.
        if [ $okb = 1 ]; then
            eval "gen=\${talaria_loadgen_pub:-}"
            if [ -n "${gen:-}" ]; then
                if ! JOB_RETRY_S=60 python3 "$HERE/lib/job.py" "$gen" 9100 "$ALL/.probe-b.json" \
                        --proto resp --host 127.0.0.1 --port 1 --op ping --conns 1 --rate 0 \
                        --duration 1 --warmup 0 >/dev/null 2>&1; then
                    say "   REFUSAL: after tail B the Talaria generator does not answer — phase B is cancelled (the knob broke the instrument itself)"
                    okb=0; RED=1
                fi
            fi
        fi
        # AFTER TAIL B THE SERVER MUST ANSWER — THAT IS A SEPARATE CHECK.
        #
        # The generator check (above) catches a knob that
        # broke the instrument. There was no symmetric SERVER check, and once
        # that cost half a run: the tail `talaria.netscan=0` fell under the
        # `talaria.net*` prefix in `tail_touches_nic`, the kernel reopened the NIC
        # after the lease had already been obtained, ENA went into `DEV_STS=0x21` — and ten
        # points of phase B spent their retry budget at 5 s per connect, giving zero.
        # A minute of checking against forty minutes of stand time.
        if [ $okb = 1 ]; then
            # shellcheck disable=SC1090
            . "$HERE/workloads/$WL/meta.env" 2>/dev/null || true
            eval "srv=\${talaria_server_ip:-}"; eval "gen=\${talaria_loadgen_pub:-}"
            if [ -n "${srv:-}" ] && [ -n "${gen:-}" ] && [ -n "${PROTO:-}" ] && [ -n "${PORT:-}" ]; then
                if ! JOB_RETRY_S=90 python3 "$HERE/lib/job.py" "$gen" 9100 "$ALL/.probe-b-srv.json" \
                        --proto "$PROTO" --host "$srv" --port "$PORT" --op ping --conns 1 --rate 0 \
                        --count 5 --duration 5 --warmup 0 2>&1 | /usr/bin/grep -qE "req/s"; then
                    say "   REFUSAL: after tail B the Talaria SERVER does not answer — phase B is cancelled"
                    say "          (look in serial for the line \"user-data touches the network (token ...)\" and ena sts=)"
                    okb=0; RED=1
                fi
            fi
        fi
        if [ $okb = 1 ]; then
            # Talaria only: a copy of stand.env without the Linux addresses — run-remote.sh
            # skips a side without addresses itself ("no addresses, skipping").
            BD="$ALL/.stand-b"; mkdir -p "$BD"
            /usr/bin/grep -v '^linux_' "$STAND_DIR/stand.env" > "$BD/stand.env"
            say "== B ($(date +%H:%M:%S)): Talaria only, over ladder A"
            for case in $(ls "$ALL/A" | sed -n 's/^talaria-\(.*\)-r[0-9]*-[0-9]*\.json$/\1/p' | sort -u); do
                rates=$(ls "$ALL/A" | sed -n "s/^talaria-$case-r\([0-9]*\)-[0-9]*\.json$/\1/p" | sort -un | tr '\n' ' ')
                say "   $case: rates $rates"
                STAND_DIR="$BD" OUTDIR="$ALL/B" RATES="$rates" "$HERE/run-remote.sh" "$WL" "$case" >> "$LOG" 2>&1 \
                    || { say "   RUN ERROR B/$case"; RED=1; }
            done
        else
            say "   tail B did not take (see $LOG)"; RED=1
        fi
    fi
fi

# THE CONSOLES OF PHASE B COME HOME before the teardown: a B whose server stopped answering
# (MariaDB under `talaria.dmap=1`, 2026-10-07) could not be explained — nobody had kept them.
if [ -f "$STAND_DIR/stand.env" ]; then
    eval "$(/usr/bin/grep -a '^talaria_\(server\|loadgen\)_id=' "$STAND_DIR/stand.env")"
    for role in server loadgen; do
        eval "id=\${talaria_${role}_id:-}"
        [ -n "$id" ] && aws ec2 get-console-output --instance-id "$id" --latest --query Output --output text \
            > "$ALL/console-$role-end.log" 2>/dev/null || true
    done
fi

if [ "${AB_KEEP:-0}" = 1 ]; then
    say "== AB_KEEP=1: the stand is left up (tear it down with: infra/stand-down.sh)"
else
    say "== tearing the stand down"
    "$HERE/infra/stand-down.sh" >> "$LOG" 2>&1 || { say "   TEARDOWN ERROR — CHECK FOR LEFTOVERS"; RED=1; }
    left=$(aws ec2 describe-instances --filters Name=tag:Project,Values=talaria-bench \
        Name=instance-state-name,Values=running,pending,stopping,stopped \
        --query 'Reservations[].Instances[].InstanceId' --output text 2>/dev/null | wc -w)
    [ "$left" = 0 ] || { say "   LEFTOVER: $left instances tagged talaria-bench"; RED=1; }
fi

if [ -d "$ALL/A" ] && [ -d "$ALL/B" ]; then
    say "== A against the Linux of the same hour"
    python3 "$HERE/lib/report.py" "$ALL/A" 2>&1 | tee -a "$LOG" | tee "$ALL/report-A.txt" >/dev/null
    say "== B against A (Talaria): a ratio < 1 means B is faster"
    python3 "$HERE/lib/compare.py" "$ALL/A" "$ALL/B" talaria 2>&1 | tee -a "$LOG" | tee "$ALL/compare-AB.txt"
fi
say "== result: $([ $RED = 0 ] && echo OK || echo 'RED PRESENT') ($(date +%H:%M:%S)), everything in $ALL"
exit $RED
