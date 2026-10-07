#!/bin/sh
# Change the cmdline tail of a LIVE Talaria server on the stand — without rebuilding
# the stand: an A/B of a kernel knob on EC2 then costs a reboot of one machine (2-3 min)
# rather than 40 minutes of bring-up (AMI + ansible).
#
#   infra/talaria-cmdline.sh "<extra tokens>"       # e.g. "talaria.netirq=1"
#   infra/talaria-cmdline.sh ""                      # restore the default
#   env: STAND_DIR= (by default ~/.talaria-lab/stand), ROLE=server|loadgen,
#        APPEND=1 — keep every current token the new tail does not set (an A/B's B)
#
# How it works: the instance's user-data is the cmdline tail (see terraform,
# `talaria.userdata=1`). The tail is read by the kernel on every boot over IMDS, so
# changing user-data + stop/start = a new cmdline on the same AMI and the same volume.
# The base tokens (`talaria.smp=`, `talaria.manifest=`, `talaria.stat=`)
# are taken from the CURRENT user-data and preserved; everything else is replaced by the
# argument. What was written is READ BACK and verified — user-data that was "almost"
# enough would cost a run on the defaults disguised as variant B.
#
# The private address survives a stop/start (the server is addressed by it), the public
# one does not, and stand.env is updated. Readiness: the kernel's "final cmdline" line
# with the new tail in serial (get-console-output) and then the manifest's answer.
set -eu
EXTRA=${1-}
HERE=$(cd "$(dirname "$0")" && pwd)
STAND_DIR=${STAND_DIR:-$HOME/.talaria-lab/stand}
[ -f "$STAND_DIR/stand.env" ] || { echo "the stand is not up"; exit 1; }
. "$STAND_DIR/stand.env"
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-$STAND_REGION}
ROLE=${ROLE:-server}
case $ROLE in
    server)  ID=$talaria_server_id;  PUBKEY=talaria_server_pub ;;
    loadgen) ID=$talaria_loadgen_id; PUBKEY=talaria_loadgen_pub ;;
    *) echo "ROLE=server|loadgen"; exit 1 ;;
esac
[ -n "$ID" ] || { echo "no Talaria instance ($ROLE) in stand.env"; exit 1; }

cur=$(aws ec2 describe-instance-attribute --instance-id "$ID" --attribute userData \
        --query 'UserData.Value' --output text | base64 -d)
# A BASE TOKEN THAT OVERRIDES THE TAIL IS DROPPED:
# `talaria.stat=8000` in the base plus `talaria.stat=2000` in the tail gave the kernel
# one knob twice, and it refused to boot ("given TWICE") — the node did not
# come up while the script waited 15 minutes for serial.
# APPEND=1 (an A/B's tail B): EVERY current token stays unless the new tail sets the same key — B
# must be A plus the knob. Without it a B of `talaria.dmap=1` dropped the generator's
# `talaria.netidle=1` (ARM64-PLAN §11.1): two knobs changed, one of them unasked.
KEEP='^talaria\.\(smp\|manifest\|stat\)='; [ "${APPEND:-0}" = 1 ] && KEEP='^talaria\.'
base=$(printf '%s\n' "$cur" | tr ' ' '\n' | /usr/bin/grep -a "$KEEP" | while read -r tok; do
    key=${tok%%=*}
    printf '%s ' "$EXTRA" | tr ' ' '\n' | /usr/bin/grep -aq "^$key=" || printf '%s ' "$tok"
done)
new="${base}${EXTRA}"
new=$(printf '%s' "$new" | sed 's/ *$//')
echo "== $ROLE $ID: user-data was: \"$cur\""
echo "   will become: \"$new\""
[ "$cur" = "$new" ] && { echo "   no changes — doing nothing"; exit 0; }

echo "== stop"
aws ec2 stop-instances --instance-ids "$ID" >/dev/null
aws ec2 wait instance-stopped --instance-ids "$ID"
# `--user-data Value=...` fails in this version of the aws cli ("'in <string>' requires
# string..."), both inline and with fileb://; the general attribute form works.
aws ec2 modify-instance-attribute --instance-id "$ID" --attribute userData --value "$(printf '%s' "$new" | base64 -w0)"
back=$(aws ec2 describe-instance-attribute --instance-id "$ID" --attribute userData \
        --query 'UserData.Value' --output text | base64 -d)
[ "$back" = "$new" ] || { echo "REFUSAL: user-data read back differently: \"$back\""; exit 1; }
echo "== start"
T0=$(date +%s)
aws ec2 start-instances --instance-ids "$ID" >/dev/null
aws ec2 wait instance-running --instance-ids "$ID"
pub=$(aws ec2 describe-instances --instance-ids "$ID" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
sed -i "s/^$PUBKEY=.*/$PUBKEY=$pub/" "$STAND_DIR/stand.env"
echo "   started, public address $pub (stand.env updated)"

# Readiness comes from serial: the kernel prints the final cmdline AFTER merging the tail
# (so IMDS answered and the tail is ours), and the manifest line when the workload has started.
# The generator's manifest has no knobs — there is no "manifest:" line, so a second
# sign is the "process by manifest" line (printed for any manifest).
# The EC2 console lags by tens of seconds, so we wait with a margin. And ONLY with
# `--latest`: without it `get-console-output` returns the console snapshot taken
# shortly after the start and updated slowly — for a restarted instance it
# keeps showing the PREVIOUS boot for a long time. That is how tail B of the mongodb A/B was lost
# (`ec2ab-mongodb-20260906-052411`): the server "waited" for 468 s, the generator did not
# make it in 600 — on a live node that already had the tail in place.
i=0
while [ $i -lt 90 ]; do
    out=$(aws ec2 get-console-output --instance-id "$ID" --latest --query Output --output text 2>/dev/null || true)
    if printf '%s' "$out" | /usr/bin/grep -aq "the final cmdline = .*$(printf '%s' "$new" | sed 's/[][\.*^$]/\\&/g')"; then
        if printf '%s' "$out" | /usr/bin/grep -aq "^manifest:\|a process by the manifest"; then
            echo "   ready in $(( $(date +%s) - T0 )) s; from serial:"
            printf '%s\n' "$out" | /usr/bin/grep -a "the final cmdline\|netsys: the card.s line\|netsys: a waiter\|^manifest:" | tail -6 | sed 's/^/     /'
            exit 0
        fi
    fi
    i=$((i + 1)); sleep 10
done
echo "REFUSAL: in 15 min serial did not show the new cmdline and the manifest; what is visible:"
printf '%s\n' "$out" | /usr/bin/grep -a "the final cmdline\|a process by the manifest\|^manifest:" | tail -3 | sed 's/^/     /'
echo "   the console's tail:"
printf '%s\n' "$out" | tail -15
exit 1
