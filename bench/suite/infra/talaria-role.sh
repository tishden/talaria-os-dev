#!/bin/sh
# A post-condition of the stand: both Talaria instances have THE role set by user-data.
#   talaria-role.sh <stand directory> <server id> <generator id>
# The role is visible in the first lines of the serial console: the kernel prints the
# manifest knob — `/cell.manifest` on the server and `/loadgen.manifest` on the generator.
# The console on Nitro is published with a delay (up to a couple of minutes) — we wait up to 3 minutes
# (ROLE_WAIT_S is the waiting step, 0 for the self-check). A refusal is a non-zero code: a stand
# without a generator is torn down at once rather than after an hour of empty retries.
# Moved out of stand-up.sh so that selfcheck.sh can run it against a fake `aws`.
set -u
STAND_DIR=$1; SERVER_ID=$2; LOADGEN_ID=$3
STEP=${ROLE_WAIT_S:-5}
echo "== Talaria: the role according to the console"
role_ok=1
for side in server loadgen; do
    if [ "$side" = server ]; then id=$SERVER_ID; want=/cell.manifest; else id=$LOADGEN_ID; want=/loadgen.manifest; fi
    con="$STAND_DIR/console-$side-boot.log"; i=0
    while [ $i -lt 36 ]; do
        aws ec2 get-console-output --instance-id "$id" --latest --output text > "$con" 2>/dev/null || true
        /usr/bin/grep -aq "knob talaria.manifest = \|stat: --- snapshot .* role \|dhcp TIMEOUT\|BOOT REFUSED\|PANIC" "$con" && break
        i=$((i + 1)); sleep "$STEP"
    done
    # THE ROLE IS ALSO LOOKED FOR IN THE FRESH PART OF THE LOG. The bring-up line lives at the START of the console, while
    # the `get-console-output` window is 64 KB: with the counter block enabled
    # (`talaria.stat=`) it is pushed out within a couple of minutes, and the check honestly failed
    # a stand with a healthy kernel (a run of 226 KB of snapshots).
    # So the role is printed in the header of EVERY snapshot — we accept both traces.
    if /usr/bin/grep -aq "knob talaria.manifest = $want" "$con" \
       || /usr/bin/grep -aq "stat: --- snapshot .* role $want" "$con"; then
        echo "   $side ($id): role $want, lease $(/usr/bin/grep -a -m1 'dhcp lease' "$con" | sed 's/.*lease //')"
    else
        echo "   $side ($id): THE WRONG ROLE or the boot failed (console: $con):"
        /usr/bin/grep -a -m3 "dhcp\|user-data\|manifest\|REFUSED\|PANIC" "$con" | sed 's/^/      /'
        [ -s "$con" ] || echo "      the console is empty after $((36 * STEP)) s"
        role_ok=0
    fi
done
if [ "$role_ok" != 1 ]; then
    echo "== Talaria: REFUSAL — a stand without the needed role, the run makes no sense (tear it down: bench/suite/infra/stand-down.sh)"
    exit 1
fi
exit 0
