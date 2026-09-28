#!/bin/sh
# A CHECK BEFORE A SCOREBOARD RUN: twenty-five minutes against five hours spent blind.
#
# One session lost three stand runs in a row, and none of them was about the
# systems being compared: diagnostics on the generator node killed the job receiver,
# IMDS failed to answer twice after a stop/start (the node came up without its role), and one
# instance came up with ena sts=0x21 (no network at all). So before a long run
# it is NOT "the stand came up" that is checked but three conditions, each of which has already
# let us down, and each with its own refusal code.
#
#   preflight.sh <workload> [expected p99, µs]
# Codes: 0 — the scoreboard run may go ahead; 1 — the stand/role; 2 — no numbers taken; 3 — the kernel in
# the AMI is the wrong one (ring lies in the thousands); 4 — the latency is not what it was in debugging.
set -u
WL=${1:-postgresql}
# THE THRESHOLD IS WIDE, AND THAT IS MEASURED. On this stand the p99 of one and the same build at
# one and the same point wanders between BOOTS by up to 1.6x (86 / 133 / 134 in three
# consecutive boots; 88 in a scoreboard and 166 in a check a day later). So the check
# cuts off A BREAKAGE (numbers worse by multiples) rather than telling good from better —
# for that there are alternating boots.
WANT_P99=${2:-200}
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HERE/../.."
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
export STAND_DIR=${STAND_DIR:-$HOME/.talaria-lab/stand}
export TF_VAR_talaria_stat_ms=20000
OUT=$HERE/results/preflight-$(date +%Y%m%d-%H%M%S)
mkdir -p "$OUT"
say() { echo "$*" | tee -a "$OUT/log.txt"; }

say "== the check before a scoreboard run: $WL (expecting a p99 no worse than $WANT_P99 µs)"
if ! sh "$HERE/infra/stand-up.sh" "$WL" >> "$OUT/log.txt" 2>&1; then
    say "REFUSAL 1: the stand did not come up or the role did not start — do not run the scoreboard"
    sh "$HERE/infra/stand-down.sh" >> "$OUT/log.txt" 2>&1; exit 1
fi
. "$STAND_DIR/stand.env"
case $WL in
    postgresql) ARGS="--proto pg --port 5432 --op query --user postgres --db postgres --query 'SELECT 1'" ;;
    mysql)      ARGS="--proto mysql --port 3306 --op query --user root --db mysql --query 'SELECT 1'" ;;
    redis)      ARGS="--proto resp --port 6379 --op ping" ;;
    mongodb)    ARGS="--proto mongo --port 27017 --op ping" ;;
    *)          say "REFUSAL 2: I do not know how to probe $WL"; sh "$HERE/infra/stand-down.sh" >> "$OUT/log.txt" 2>&1; exit 2 ;;
esac
eval "python3 $HERE/lib/job.py '$talaria_loadgen_pub' 9100 '$OUT/probe.json' \
     $ARGS --host $talaria_server_ip --conns 4 --rate 10000 --duration 20 --warmup 5 --spin \
     --name preflight/$WL --stand talaria" >> "$OUT/log.txt" 2>&1
aws ec2 get-console-output --instance-id "$talaria_server_id" --latest --output text > "$OUT/console.txt" 2>&1
sh "$HERE/infra/stand-down.sh" >> "$OUT/log.txt" 2>&1

P99=$(python3 -c "
import json
r=json.load(open('$OUT/probe.json'))
print('ERR' if r.get('error') else int(r['latency']['service']['p99']/1000))" 2>/dev/null)
LIE=$(/usr/bin/grep -ao 'the ring lied [0-9]*' "$OUT/console.txt" | tail -1 | awk '{print $NF}')
SNAP=$(/usr/bin/grep -ac 'stat: --- snapshot' "$OUT/console.txt")
say "   p99=$P99 µs, ring lies=${LIE:-none}, counter snapshots=$SNAP"

[ "$P99" = ERR ] || [ -z "$P99" ] && { say "REFUSAL 2: no numbers taken (the generator/server did not answer)"; exit 2; }
if [ -n "${LIE:-}" ] && [ "$LIE" -gt 1000 ]; then
    say "REFUSAL 3: ring lies $LIE — an OLD kernel went into the AMI (after the fix this is single digits)"; exit 3
fi
if [ "$P99" -gt "$WANT_P99" ]; then
    say "REFUSAL 4: p99 $P99 µs is worse than the expected $WANT_P99 — sort that out first, then the scoreboard"; exit 4
fi
say "== THE CHECK IS CLEAN: the scoreboard run may be taken"
exit 0
