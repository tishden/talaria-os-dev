#!/bin/bash
# ena-rtt.sh: measure the "Linux neighbour -> Talaria" RTT with one command.
#
# Launches an instance from a ready AMI with the given cmdline tail (via user-data),
# waits for the kernel echo to be ready, runs udp_ping from the NEIGHBOUR (it is in the
# same cluster placement group — otherwise the distance between racks is measured) and
# terminates the instance. So one hypothesis costs ~2 minutes instead of a cycle with an AMI build.
#
# Usage:
#   ENVF=... PEM=... infra/aws/ena-rtt.sh "talaria.ena=llq" [type] [N packets]
#   env: KEEP=1 — keep the instance; AMI=... — an explicit image.
set -euo pipefail
TAIL=${1:-}
TYPE=${2:-c6i.large}
N=${3:-2000}
ENVF=${ENVF:?no ENVF}; PEM=${PEM:?no PEM}; . "$ENVF"
W=$(dirname "$ENVF")
AMI=${AMI:-$(sed -n 's/^AMI=//p' "$W/last-node.sh" | tail -1)}
[ -n "$AMI" ] || { echo "no AMI found"; exit 1; }
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
SSH="ssh -i $PEM -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=15"
TS=$(date +%H%M%S)
UD="talaria.mode=userver talaria.kecho=7777 $TAIL"

console_of() {
  raw=$(aws ec2 get-console-output --instance-id "$1" --latest --output text --query Output 2>/dev/null || true)
  [ -z "$raw" ] && return 0
  dec=$(printf '%s' "$raw" | base64 -d 2>/dev/null || true)
  case "$dec" in *talaria*|*uefi-stub*) printf '%s' "$dec"; return 0;; esac
  printf '%s' "$raw"
}

echo "== AMI $AMI, type $TYPE, cmdline tail: $UD"
IID=$(aws ec2 run-instances --image-id "$AMI" --instance-type "$TYPE" --key-name talaria-v11 \
  --security-group-ids "$SG" --subnet-id "$SUBNET" --associate-public-ip-address \
  --placement "GroupName=${CPG},AvailabilityZone=${AZ}" --user-data "$UD" \
  --tag-specifications "ResourceType=instance,Tags=[{Key=Project,Value=talaria-v11},{Key=Name,Value=talaria-rtt-$TS}]" \
  --query 'Instances[0].InstanceId' --output text)
echo "instance $IID" >> "$W/resources.txt"
trap '[ "${KEEP:-0}" = 1 ] || aws ec2 terminate-instances --instance-ids "$IID" >/dev/null 2>&1 || true' EXIT
PRIV=$(aws ec2 describe-instances --instance-ids "$IID" --query 'Reservations[0].Instances[0].PrivateIpAddress' --output text)
echo "instance $IID, private address $PRIV"

# Readiness is NOT detected by a console line: its buffer is limited, and the
# periodic output (ENA statistics every 30 s) overwrites the echo startup line — a
# known pitfall. We wait for running + a fixed boot pause, and the actual
# readiness is confirmed by udp_ping itself.
aws ec2 wait instance-running --instance-ids "$IID"
sleep "${BOOTWAIT:-100}"
console_of "$IID" > "$W/rtt-$TS.log" || true
grep -aE "ena: the LLQ (is active|is configured)|ena: RSS|ena: the PHC|ena: the IO queues|ena: the device|the kernel echo|txbench:" "$W/rtt-$TS.log" | tail -8
# talaria.ena=txbench=N prints its result to serial and udp_ping is not needed
grep -aq "txbench:" "$W/rtt-$TS.log" && { echo "== the transmit run was taken from the console (udp_ping not needed)"; exit 0; }

echo "== udp_ping from the neighbour ($PEER_PRIV -> $PRIV:7777), $N packets"
$SSH ec2-user@"$PEER_IP" "/tmp/udp_ping $PRIV 7777 $N 500 1 64 2>&1 | tail -6"
