#!/bin/sh
# CAPTURE THE CONSOLE WHILE THE NODE IS ALIVE — not after the stand has been torn down.
#
#   console-watch.sh once <instance id> <file>            # one snapshot and exit
#   console-watch.sh loop <instance id> <file> [period_s]  # until killed
#
# WHY. All Talaria diagnostics on EC2 live in serial: the kernel's counters are printed
# there as a `stat:` block, and there is no other channel. serial can be fetched only with
# `aws ec2 get-console-output --latest`, and that returns a 64 KB WINDOW. Hence two
# rules, bought at the price of one lost diagnostic pass:
#
#   * the window must be captured PERIODICALLY and spliced (console-splice.py), otherwise
#     the start of the run moves out — while the counters must be read as the DIFFERENCE of the first and
#     the last snapshot, being cumulative;
#   * it MUST be captured WHILE THE NODE IS ALIVE. Once the console was saved only by the fact that
#     the terminated instance still returned it; that is luck, not a plan.
#
# The default period is 20 s, twice as often as `talaria.stat=20000` prints: with
# `talaria.stat=5000` the `stat:` block comes faster than the serial line carries it away,
# snapshots are missed, and the window keeps neighbouring IDLE blocks with
# identical counters (they look real — and that is exactly the instrument's defect).
set -u
MODE=${1:?mode: once | loop}
ID=${2:?instance id}
OUT=${3:?accumulator file}
PERIOD=${4:-20}
HERE=$(cd "$(dirname "$0")" && pwd)
SPLICE="$HERE/../lib/console-splice.py"
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}

snap() {
    # --latest IS MANDATORY: without it EC2 returns the snapshot taken at BOOT
    # (a known pitfall, with a gate in selfcheck.sh).
    aws ec2 get-console-output --instance-id "$ID" --latest --output text 2>/dev/null \
        | python3 "$SPLICE" "$OUT" 2>&1 | sed "s/^/console-watch $ID: /"
}

case "$MODE" in
    once) snap ;;
    loop)
        # We are killed with TERM from ab-phase.sh; the last snapshot is taken by it
        # (`once`) — there is nothing to catch the signal with here: `sleep` would finish anyway.
        while :; do
            snap
            sleep "$PERIOD"
        done ;;
    *) echo "console-watch: unknown mode $MODE"; exit 2 ;;
esac
