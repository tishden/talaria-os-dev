#!/bin/sh
# ONE MORE PHASE ON AN ALREADY RUNNING STAND — over the ladder of PHASE A.
#
#   ab-phase.sh <run directory> <tag> "<cmdline tail>" [workload] [cases...]
#   env: DUR= WARM= REPEAT= (as in run-remote.sh)
#        ROLES="server loadgen" — whom to give the tail to (diagnostics go ONLY to the server:
#                                 `talaria.stat=`/`sysprof` on the generator spoil the instrument)
#        REVERSE=1 — walk the ladder TOP DOWN (three losses stand exactly
#                    on the bottom point, which goes first and pays for the warm-up)
#        GEN=linux — measure the Talaria server WITH A LINUX GENERATOR (the answer
#                    depends on whose client it is; that is how the instrument is checked)
#        ONLY_RATES="7566 3026" — only these points of ladder A (a diagnostic
#                    phase over one point instead of five)
#        CONSOLE="server" — whose serial console to capture DURING the phase
#                    (server | loadgen | "server loadgen" | none), period CONSOLE_S=20.
#                    The file is <run directory>/console-<tag>.txt
#   A tail of "=" means "do not touch the cmdline" (a repeat phase, a reverse ladder,
#   a foreign generator — all of that is taken WITHOUT rebooting the nodes).
#
# WHY SEPARATE FROM run-remote-ab.sh. That one can do exactly two phases and brings up its own
# stand per call (AMI + terraform + ansible = 10-11 minutes). Experience
# showed that is not what is needed: the ONLY reliable conclusion came from FOUR
# phases on ONE bring-up (A — the defaults, B — the previous behaviour, C — parking,
# D — a short slice), because between stand bring-ups not even the MEDIAN
# reproduces: an A/B within one hour gave 10/0/0 against Linux, while a scoreboard three hours later
# on a fresh stand gave 2/3/5 with the numbers moved by 20 %.
#
# So the rule is: the stand is brought up ONCE (`run-remote-ab.sh` with
# `AB_KEEP=1` or `infra/stand-up.sh`), phase A is taken with both sides,
# and after that each hypothesis is one phase run by this script. The ladder is taken from the
# FILE NAMES of phase A: otherwise the phase would build its own from its own ceiling, and not a single
# point would match.
#
# THE PHASE CAPTURES THE CONSOLE ITSELF WHILE THE NODE IS ALIVE. A diagnostic phase without a console is
# pointless: the Talaria kernel's counters on EC2 live ONLY in serial. Once
# the console was captured by hand and as a single snapshot — i.e. the last 64 KB window
# (`console-PA.txt` exactly 65582 B), and the first diagnostic pass was lost entirely, while
# what survived was saved only by the terminated instance still returning its console.
# Now `console-watch.sh` polls the window every CONSOLE_S seconds and splices the
# snapshots (`console-splice.py`), and the last snapshot is taken BEFORE the phase
# ends and the stand is torn down.
#
# CHECKING THE SERVER IS MANDATORY AND COSTS A MINUTE. The tail goes to BOTH nodes and may
# break either of them: once `talaria.netscan=0` fell under the `talaria.net*` prefix
# in `tail_touches_nic`, the kernel reopened the NIC after the lease was obtained,
# ENA went into `DEV_STS=0x21`, and ten points honestly spent their retry
# budget of 5 s per connect, giving zero. Forty minutes of stand time for a minute of checking.
set -u
ALL=${1:?the run directory (it must contain an A subdirectory)}
TAG=${2:?the phase tag, e.g. C}
EXTRA=${3:?the cmdline tail}
WL=${4:-mysql}
shift 4 2>/dev/null || shift 3
CASES=${*:-}
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HERE/../.."
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
export STAND_DIR=${STAND_DIR:-$HOME/.talaria-lab/stand}
[ -d "$ALL/A" ] || { echo "no $ALL/A — phase A must be taken first"; exit 1; }
[ -f "$STAND_DIR/stand.env" ] || { echo "the stand is not up"; exit 1; }
"$HERE/infra/sg-refresh.sh" || true   # the runner's address may have changed (VPN)
LOG="$ALL/log-$TAG.txt"
say() { echo "$*" | tee -a "$LOG"; }

ROLES=${ROLES:-server loadgen}
say "== phase $TAG: tail \"$EXTRA\" to nodes [$ROLES]${REVERSE:+, ladder top down}${GEN:+, generator $GEN} ($(date +%H:%M:%S))"
if [ "$EXTRA" != "=" ]; then
    for role in $ROLES; do
        ROLE=$role "$HERE/infra/talaria-cmdline.sh" "$EXTRA" >> "$LOG" 2>&1 \
            || { say "   REFUSAL: $role — the tail did not take"; exit 1; }
    done
fi
# shellcheck disable=SC1090
. "$STAND_DIR/stand.env"
# The workload's meta.env declares its OWN `CASES` (mysql: "select1:...|pk:...") and
# used to overwrite the case list from the arguments — the phase then walked the words of the SQL query.
WANT_CASES=$CASES
. "$HERE/workloads/$WL/meta.env"
CASES=$WANT_CASES

# AFTER THE TAIL THE SERVER MUST ANSWER (see the header). The probe's arguments come from
# meta.env (`PROBE_ARGS`): a bare `--op ping` is rejected by the mysql server
# ("Unknown database 'talaria'"), and two phases were cancelled on a healthy
# stand under the guise of "the server does not answer".
# THE PROBE'S BUDGET COMES FROM THE WORKLOAD'S BOOT_WAIT, NOT FROM A CONSTANT. It used to be 90 s, while
# changing the tail is a node reboot: mongodb has `BOOT_WAIT=180`, and the FIRST
# phase after phase A was cancelled with "the server does not answer" on a healthy stand.
# The second phase passed the same way — i.e. the refusal was not about the server
# but about the probe not waiting for the database.
PROBE_RETRY_S=${PROBE_RETRY_S:-$(( ${BOOT_WAIT:-90} + 60 ))}
if ! JOB_RETRY_S=$PROBE_RETRY_S python3 "$HERE/lib/job.py" "$talaria_loadgen_pub" 9100 "$ALL/.probe-$TAG.json" \
        --proto "$PROTO" --host "$talaria_server_ip" --port "$PORT" ${PROBE_ARGS:---op ping} --conns 1 \
        --rate 0 --count 5 --duration 5 --warmup 0 2>&1 | /usr/bin/grep -qE "req/s"; then
    say "   REFUSAL: after tail $TAG THE SERVER does not answer within ${PROBE_RETRY_S} s — the phase is cancelled"
    say "          look in serial for: \"user-data touches the network (token ...)\" and \"ena: ... sts=\""
    say "          and for the probe's error: $(python3 -c "import json;print(json.load(open('$ALL/.probe-$TAG.json')).get('error',''))" 2>/dev/null)"
    # The serial of the server that did not answer: the phase's watcher starts only after the
    # answer, so without this one snapshot a refusal leaves nothing to read (kp31-32 lost three).
    rid=${talaria_server_id:-}
    if [ -n "$rid" ]; then
        "$HERE/infra/console-watch.sh" once "$rid" "$ALL/console-$TAG-refusal.txt" >> "$LOG" 2>&1 || true
        say "          the server's serial: $ALL/console-$TAG-refusal.txt"
    fi
    exit 2
fi
say "   the server answers"

# The console: a watcher per node needed, the file next to the phase.
CONSOLE=${CONSOLE:-server}
CON_PIDS=""
CON_FILE="$ALL/console-$TAG.txt"
if [ "$CONSOLE" != none ]; then
    for side in $CONSOLE; do
        eval "cid=\${talaria_${side}_id:-}"
        [ -n "$cid" ] || { say "   console $side: stand.env has no talaria_${side}_id — skipping"; continue; }
        f="$CON_FILE"; [ "$side" = server ] || f="$ALL/console-$TAG-$side.txt"
        "$HERE/infra/console-watch.sh" loop "$cid" "$f" "${CONSOLE_S:-20}" >> "$LOG" 2>&1 &
        CON_PIDS="$CON_PIDS $!:$cid:$f"
        say "   console $side ($cid) is captured every ${CONSOLE_S:-20} s -> $f"
    done
fi
# A phase may be interrupted (Ctrl-C, a RUN ERROR) — the watchers must not survive
# it and silently burn API calls until the end of the session.
con_stop() {
    for rec in $CON_PIDS; do
        pid=${rec%%:*}; rest=${rec#*:}; cid=${rest%%:*}; f=${rest#*:}
        kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
        # THE LAST SNAPSHOT IS TAKEN WHILE THE NODE IS ALIVE, otherwise the end of the run (which is the whole
        # point: the counters are cumulative and are read as a difference) stays outside the window.
        "$HERE/infra/console-watch.sh" once "$cid" "$f" >> "$LOG" 2>&1 || true
        say "   console $cid captured: $(wc -c < "$f" 2>/dev/null || echo 0) B -> $f"
    done
    CON_PIDS=""
}
trap 'con_stop' EXIT INT TERM

# Talaria only: a copy of stand.env without the Linux addresses — run-remote.sh skips
# a side without addresses itself (the Linux anchor was taken in phase A of the same hour).
BD="$ALL/.stand-$TAG"; mkdir -p "$BD"
/usr/bin/grep -v '^linux_' "$STAND_DIR/stand.env" > "$BD/stand.env"
if [ "${GEN:-}" = linux ]; then
    # A Linux generator probes the Talaria server: ONLY the generator's addresses are
    # substituted, the server and its instance id stay ours.
    sed -i "s/^talaria_loadgen_ip=.*/talaria_loadgen_ip=$linux_loadgen_ip/; s/^talaria_loadgen_pub=.*/talaria_loadgen_pub=$linux_loadgen_pub/" "$BD/stand.env"
fi
[ -n "$CASES" ] || CASES=$(ls "$ALL/A" | sed -n 's/^talaria-\(.*\)-r[0-9]*-[0-9]*\.json$/\1/p' | sort -u)
for case in $CASES; do
    rates=$(ls "$ALL/A" | sed -n "s/^talaria-$case-r\([0-9]*\)-[0-9]*\.json$/\1/p" | sort -un ${REVERSE:+-r} | tr '\n' ' ')
    [ -n "$rates" ] || { say "   $case: phase A has no points — skipping"; continue; }
    if [ -n "${ONLY_RATES:-}" ]; then
        rates=$(for r in $rates; do for o in $ONLY_RATES; do [ "$r" = "$o" ] && echo "$r"; done; done | tr '\n' ' ')
        [ -n "$rates" ] || { say "   $case: ONLY_RATES does not intersect ladder A — skipping"; continue; }
    fi
    say "   $case: rates $rates"
    STAND_DIR="$BD" OUTDIR="$ALL/$TAG" RATES="$rates" \
        DUR=${DUR:-40} WARM=${WARM:-10} REPEAT=${REPEAT:-2} \
        "$HERE/run-remote.sh" "$WL" "$case" >> "$LOG" 2>&1 || say "   ERROR $case"
done
con_stop
say "== phase $TAG is done ($(date +%H:%M:%S))"
say "   compare with:  python3 bench/suite/lib/compare.py $ALL/$TAG $ALL/A"
