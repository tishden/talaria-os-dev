#!/bin/sh
# THE WHOLE tlgen SUITE ON EC2 IN ONE COMMAND: for each workload bring the stand up,
# run, TEAR IT DOWN, check that nothing is left; at the end, one summary.
#
#   run-remote-all.sh [workloads...]     (by default all that have a meta.env)
#   env: the same as run-remote.sh (DUR/WARM/CONNS/REPEAT/RATES), SKIP="..."
#        and stand-up.sh's (AWS_DEFAULT_REGION, TF_VAR_instance_type)
#
# Why: the criterion is "a run on EC2 starts with one command,
# reaches the end and gives judgeable points on both sides, and the cleanup is checked
# by a gate". For seven sessions the stand was brought up by hand one workload at a time, and every time
# something in the seam (the AMI, user-data, the generator's address, the cleanup) was done wrong.
#
# The stand costs money: one workload is four instances for about an hour. So
# an error in ONE workload does not bring the run down, but the stand after it is ALWAYS torn down, and
# after every teardown the live instances tagged Project=talaria-bench are counted:
# a non-zero remainder is a red line in the summary, not a quiet bill at the end of the month.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
LIST=${*:-}
[ -n "$LIST" ] || LIST=$(cd "$HERE/workloads" && ls -d */ 2>/dev/null | tr -d /)
SKIP=${SKIP:-}
STAMP=$(date +%Y%m%d-%H%M%S)
ALL="$HERE/results/ec2all-$STAMP"
export STAND_DIR=${STAND_DIR:-$HOME/.talaria-lab/stand}
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
mkdir -p "$ALL"
LOG="$ALL/log.txt"
: > "$LOG"
say() { echo "$*" | tee -a "$LOG"; }

# Hygiene: a live stand from a previous run must be torn down first, otherwise terraform
# overwrites its state and the old instances are left orphaned.
if [ -f "$STAND_DIR/stand.env" ]; then
    say "== a live stand found ($STAND_DIR/stand.env) — tearing it down before the start"
    "$HERE/infra/stand-down.sh" >> "$LOG" 2>&1 || true
fi

# THE REMAINDER IS COUNTED HONESTLY, OTHERWISE IT IS NOT COUNTED AT ALL. The list of states did not
# include `shutting-down`, and a machine whose teardown DID NOT FINISH fell into the gap:
# the check printed "remainder 0" while the instances stayed alive. That is how
# four hours of four machines burned — after "CLEANUP ERROR" the summary still
# said zero. Plus one re-check after a pause: termination is not instantaneous,
# and AWS's first answer often still finds the machine alive.
alive() { aws ec2 describe-instances --filters Name=tag:Project,Values=talaria-bench \
        Name=instance-state-name,Values=running,pending,stopping,shutting-down,stopped \
        --query 'Reservations[].Instances[].InstanceId' --output text 2>/dev/null | wc -w; }
alive_settled() { n=$(alive); [ "$n" = 0 ] && { echo 0; return; }; sleep 30; alive; }

say "== the suite on EC2 -> $ALL (region $AWS_DEFAULT_REGION)"
RED=0
for wl in $LIST; do
    case " $SKIP " in *" $wl "*) say "-- $wl: skipped"; continue ;; esac
    [ -f "$HERE/workloads/$wl/meta.env" ] || { say "-- $wl: no meta.env"; continue; }
    # Harness workloads (actors/clickhouse/jvm/task) are not run through tlgen —
    # they have no PROTO=; by default they are skipped rather than failing.
    grep -q '^PROTO=' "$HERE/workloads/$wl/meta.env" || { say "-- $wl: not a tlgen workload (no PROTO=), skipping"; continue; }
    say "== $wl: the stand ($(date +%H:%M:%S))"
    t0=$(date +%s)
    st=OK
    if "$HERE/infra/stand-up.sh" "$wl" >> "$LOG" 2>&1; then
        say "   the stand came up in $(( $(date +%s) - t0 )) s, running"
        if ! OUTDIR="$ALL/$wl" "$HERE/run-remote.sh" "$wl" >> "$LOG" 2>&1; then
            st="RUN ERROR"
        fi
    else
        st="STAND ERROR (see $STAND_DIR/mk-ami-$wl.log)"
    fi
    "$HERE/infra/stand-down.sh" >> "$LOG" 2>&1 || st="$st, CLEANUP ERROR"
    n=$(alive_settled)
    if [ "$n" != 0 ]; then
        # The teardown did not finish — we try ONCE MORE, and only then colour the summary.
        # Otherwise the "CLEANUP ERROR" line stays the only trace while the machines
        # keep running (four hours of four machines once).
        say "   $wl: $n left after the cleanup — tearing down again"
        "$HERE/infra/stand-down.sh" >> "$LOG" 2>&1 || true
        n=$(alive_settled)
    fi
    [ "$n" = 0 ] || { st="$st, INSTANCES LEFT: $n"; }
    case "$st" in OK) ;; *) RED=$((RED + 1)) ;; esac
    say "   $wl: $st in $(( $(date +%s) - t0 )) s"
done

DIRS=$(ls -d "$ALL"/*/ 2>/dev/null | sed 's|/$||' | tr '\n' ' ')
if [ -n "$DIRS" ]; then
    python3 "$HERE/lib/summary.py" $DIRS --md "$ALL/summary.md" > "$ALL/summary.txt" 2>&1 || true
    tail -30 "$ALL/summary.txt" | tee -a "$LOG"
fi
say "== done: $ALL, workloads with errors: $RED"
[ "$RED" = 0 ]
