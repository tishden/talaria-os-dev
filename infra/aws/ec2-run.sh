#!/bin/bash
# ec2-run.sh: run the NEXT experiment on an ALREADY built AMI, passing the cmdline
# tail through EC2 user-data.
#
# Why: ec2-boot-talaria.sh runs the "image -> dd onto a volume -> snapshot ->
# register-image -> instance" cycle for every hypothesis — 10-15 minutes. A kernel with
# talaria.userdata=1 reads the cmdline tail from IMDS (/latest/user-data), so an
# experiment costs exactly one instance launch (~1-2 min), and one AMI serves the whole series.
#
# AMI requirement: its talaria.cfg must contain talaria.userdata=1 and the network
# parameters (talaria.ip=dhcp talaria.pci=1) — user-data cannot be read without a network.
# LIMITATION: mode=fast cannot be selected this way (the cell on an AP takes the NIC
# for itself before the stack comes up) — only from talaria.cfg.
#
# Usage:
#   ENVF=~/.talaria-lab/lab.env PEM=~/.talaria-lab/talaria-v11.pem \
#     infra/aws/ec2-run.sh "talaria.test=fsbench" [instance-type] [ami]
#   env: KEEP=1 — do not terminate the instance; WAIT=<s> — how long to wait for the log (600).
set -euo pipefail
UD=${1:?usage: ec2-run.sh "<cmdline tail>" [type] [ami]}
TYPE=${2:-c6i.large}
ENVF=${ENVF:?no ENVF}
PEM=${PEM:?no PEM}
. "$ENVF"
W=$(dirname "$ENVF")
AMI=${3:-}
if [ -z "$AMI" ]; then
  [ -f "$W/last-node.sh" ] || { echo "no $W/last-node.sh — run ec2-boot-talaria.sh first"; exit 1; }
  AMI=$(sed -n 's/^AMI=//p' "$W/last-node.sh" | tail -1)
fi
[ -n "$AMI" ] || { echo "no AMI found"; exit 1; }
if [ -z "${AWS_ACCESS_KEY_ID:-}" ]; then export AWS_PROFILE=${AWS_PROFILE:-talaria}; fi
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
TS=$(date +%H%M%S)

echo "== AMI $AMI, type $TYPE, user-data: $UD"
PLACE=""
[ -n "${CPG:-}" ] && PLACE="--placement GroupName=$CPG${AZ:+,AvailabilityZone=$AZ}"
IID=$(aws ec2 run-instances --image-id "$AMI" --instance-type "$TYPE" --key-name talaria-v11 \
  --security-group-ids "$SG" --subnet-id "$SUBNET" --associate-public-ip-address $PLACE \
  --user-data "$UD" \
  --tag-specifications "ResourceType=instance,Tags=[{Key=Project,Value=talaria-v11},{Key=Name,Value=talaria-run-$TS}]" \
  --query 'Instances[0].InstanceId' --output text)
echo "instance $IID" >> "$W/resources.txt"
PRIV=$(aws ec2 describe-instances --instance-ids "$IID" --query 'Reservations[0].Instances[0].PrivateIpAddress' --output text)
echo "TALARIA_IID=$IID"; echo "TALARIA_PRIV=$PRIV"
echo "TALARIA_IID=$IID" > "$W/last-run.sh"; echo "TALARIA_PRIV=$PRIV" >> "$W/last-run.sh"

cleanup() { [ "${KEEP:-0}" = 1 ] && { echo "KEEP=1: instance $IID left running"; return 0; }; aws ec2 terminate-instances --instance-ids "$IID" >/dev/null 2>&1 || true; echo "== terminate $IID"; }
trap cleanup EXIT

# AWS CLI v2 returns Output ALREADY decoded, v1 returns base64. Try both:
# a blind `| base64 -d` on v2 gave nothing, and the log looked like "the instance is silent".
console_of() {
  raw=$(aws ec2 get-console-output --instance-id "$1" --latest --output text --query Output 2>/dev/null || true)
  [ -z "$raw" ] && return 0
  dec=$(printf '%s' "$raw" | base64 -d 2>/dev/null || true)
  case "$dec" in *talaria*|*uefi-stub*) printf '%s' "$dec"; return 0;; esac
  printf '%s' "$raw"
}
# get-console-output ONLY with --latest: without it Nitro returns nothing.
LOG="$W/run-$TS.log"
DEADLINE=$(( $(date +%s) + ${WAIT:-600} ))
LAST=0
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
  sleep 15
  OUT=$(console_of "$IID")
  [ -z "$OUT" ] && continue
  echo "$OUT" > "$LOG"
  N=$(wc -l < "$LOG")
  [ "$N" -gt "$LAST" ] && { echo "--- serial: $N lines ---"; LAST=$N; }
  # signs of a finished run
  echo "$OUT" | grep -qaE "ALL OK|FAILED|FAILURE|RECOVERED OK|COMMITTED" && break
done
echo "== log: $LOG"
tail -40 "$LOG" 2>/dev/null || echo "(empty)"
