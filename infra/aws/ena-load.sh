#!/bin/bash
# ena-load.sh: what our datapath does UNDER LOAD — two quantities in one
# instance launch.
#
#   phase 1/2 — p50 stability: udp_ping in silence, then the same under a background
#              stream. Originally a diagnostic ("does the device hold completions
#              on a timer?"); that question is settled, and one point remains —
#              whether latency drifts under traffic.
#   phase 3   — the RECEIVE ceiling: background load without a rate limit to a sink port
#              (talaria.ksink), and the kernel prints `rxbench:` — how many frames
#              actually reached the driver. Receive had never been measured: there was
#              only a transmit run (talaria.ena=txbench=N).
#
# Both phases run on ONE instance, so the placement spread (~10 µs between
# launches) does not interfere — two measurements of one machine are compared.
#
# Usage:
#   ENVF=... PEM=... infra/aws/ena-load.sh "talaria.ena=llq" [type] [packets] [background pps]
set -euo pipefail
TAIL=${1:-}
TYPE=${2:-c6i.large}
N=${3:-20000}
FPPS=${4:-100000}
ENVF=${ENVF:?no ENVF}; PEM=${PEM:?no PEM}; . "$ENVF"
W=$(dirname "$ENVF")
AMI=${AMI:-$(sed -n 's/^AMI=//p' "$W/last-node.sh" | tail -1)}
[ -n "$AMI" ] || { echo "no AMI found"; exit 1; }
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
SSH="ssh -n -i $PEM -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=15"
TS=$(date +%H%M%S)
# ksink=7778 — receive and discard (without it the stack builds an ICMP "port
# unreachable" for every background packet, and the rate of those replies would be
# measured instead of receive); rxbench=1 — print the receive rate once a second.
UD="talaria.mode=userver talaria.kecho=7777 talaria.ksink=7778 talaria.ena=rxbench=1 $TAIL"

echo "== AMI $AMI, type $TYPE, tail: $UD"
IID=$(aws ec2 run-instances --image-id "$AMI" --instance-type "$TYPE" --key-name talaria-v11 \
  --security-group-ids "$SG" --subnet-id "$SUBNET" --associate-public-ip-address \
  --placement "GroupName=${CPG},AvailabilityZone=${AZ}" --user-data "$UD" \
  --tag-specifications "ResourceType=instance,Tags=[{Key=Project,Value=talaria-v11},{Key=Name,Value=talaria-load-$TS}]" \
  --query 'Instances[0].InstanceId' --output text)
echo "instance $IID" >> "$W/resources.txt"
trap '[ "${KEEP:-0}" = 1 ] || aws ec2 terminate-instances --instance-ids "$IID" >/dev/null 2>&1 || true' EXIT
PRIV=$(aws ec2 describe-instances --instance-ids "$IID" --query 'Reservations[0].Instances[0].PrivateIpAddress' --output text)
aws ec2 wait instance-running --instance-ids "$IID"
sleep "${BOOTWAIT:-100}"

if [ "${RXONLY:-0}" != 1 ]; then
  echo "== phase 1: udp_ping in silence"
  $SSH ec2-user@"$PEER_IP" "/tmp/udp_ping $PRIV 7777 $N 500 1 64 2>&1 | tail -2"

  echo "== phase 2: udp_ping under background load ($FPPS packets/s to sink port 7778 —"
  echo "   the background loads ONLY the device's receive path and the stack, without doubling replies)"
  $SSH ec2-user@"$PEER_IP" "nohup /tmp/udp_flood $PRIV 7778 60 $FPPS 64 > /tmp/flood.out 2>&1 &"
  sleep 3
  $SSH ec2-user@"$PEER_IP" "/tmp/udp_ping $PRIV 7777 $N 500 1 64 2>&1 | tail -2"
  sleep 2
  $SSH ec2-user@"$PEER_IP" "cat /tmp/flood.out 2>/dev/null | tail -1"
  # Wait until phase 2's background finishes (60 s from its start), otherwise two
  # streams share the neighbour's transmitter and the ceiling comes out low.
  sleep "${RXGAP:-45}"
fi

RXJOBS=${RXJOBS:-2}
# FLOODER_IP — which machine floods. By default the same neighbour, but it is its own
# ceiling: c6i.large emits ~560 k packets/s IN TOTAL, however many processes run
# on it (verified: 1 process — 557k, 4 processes — 140k each). Exactly that much
# was received, i.e. the generator was measured, not receive. An honest ceiling
# needs a bigger neighbour — its address is passed here.
FLOOD_IP=${FLOODER_IP:-$PEER_IP}
echo "== phase 3: receive ceiling — $RXJOBS generator(s) from $FLOOD_IP, ${RXSECS:-30} s"
$SSH ec2-user@"$FLOOD_IP" "rm -f /tmp/rxflood*.out; for j in \$(seq 1 $RXJOBS); do nohup /tmp/udp_flood $PRIV 7778 ${RXSECS:-30} 0 64 > /tmp/rxflood\$j.out 2>&1 & done; wait; cat /tmp/rxflood*.out | awk -F'-> ' '{print} /packets\/s/{split(\$2,a,\" \"); s+=a[1]} END{print \"TOTAL sent \" s \" packets/s\"}'"
sleep 3

echo "== console of the node under test (datapath counters and rxbench) =="
raw=$(aws ec2 get-console-output --instance-id "$IID" --latest --output text --query Output 2>/dev/null || true)
dec=$(printf '%s' "$raw" | base64 -d 2>/dev/null || true)
case "$dec" in *talaria*) :;; *) dec=$raw;; esac
printf '%s' "$dec" > "$W/load-$TS.log"
grep -aE "rxbench:|ena: rx=|ena: LLQ|ena: the device|the sink UDP" "$W/load-$TS.log" | tail -12 || true
echo "== full console: $W/load-$TS.log"
