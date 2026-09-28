#!/bin/bash
# ena-phc.sh: does ENA have a device clock (PHC) on THIS instance type.
#
# Why a separate script and not ena-rtt.sh: the type for this probe is different (the
# PHC is enabled on m7i neighbours, while our whole stand is c6i), and there is no
# point putting a foreign type into the cluster placement group with the neighbour —
# RTT is not measured here, one console line is needed. A group of mixed types may
# well refuse for capacity, and the probe would fail for a reason not its own.
#
# What we look at: bit 29 of supported_features (ENA_ADMIN_PHC_CONFIG). The kernel
# reads it at initialisation and prints "ena: PHC ...". With
# talaria.ena=phc the driver also tries to enable it.
#
# Usage:
#   ENVF=... PEM=... infra/aws/ena-phc.sh [type] ["cmdline tail"]
set -euo pipefail
TYPE=${1:-m7i.xlarge}
TAIL=${2:-talaria.ena=phc}
ENVF=${ENVF:?no ENVF}; PEM=${PEM:?no PEM}; . "$ENVF"
W=$(dirname "$ENVF")
AMI=${AMI:-$(sed -n 's/^AMI=//p' "$W/last-node.sh" | tail -1)}
[ -n "$AMI" ] || { echo "no AMI found"; exit 1; }
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
TS=$(date +%H%M%S)
UD="talaria.mode=userver talaria.kecho=7777 $TAIL"

echo "== AMI $AMI, type $TYPE, tail: $UD"
IID=$(aws ec2 run-instances --image-id "$AMI" --instance-type "$TYPE" --key-name talaria-v11 \
  --security-group-ids "$SG" --subnet-id "$SUBNET" --associate-public-ip-address \
  --user-data "$UD" \
  --tag-specifications "ResourceType=instance,Tags=[{Key=Project,Value=talaria-v11},{Key=Name,Value=talaria-phc-$TS}]" \
  --query 'Instances[0].InstanceId' --output text)
echo "instance $IID" >> "$W/resources.txt"
trap '[ "${KEEP:-0}" = 1 ] || aws ec2 terminate-instances --instance-ids "$IID" >/dev/null 2>&1 || true' EXIT
aws ec2 wait instance-running --instance-ids "$IID"
sleep "${BOOTWAIT:-140}"

raw=$(aws ec2 get-console-output --instance-id "$IID" --latest --output text --query Output 2>/dev/null || true)
dec=$(printf '%s' "$raw" | base64 -d 2>/dev/null || true)
case "$dec" in *talaria*) :;; *) dec=$raw;; esac
printf '%s' "$dec" > "$W/phc-$TYPE-$TS.log"
echo "== what the kernel saw on $TYPE =="
grep -aE "ena: mac=|ena: the PHC|the final cmdline" "$W/phc-$TYPE-$TS.log" | tail -6 || true
echo "== full console: $W/phc-$TYPE-$TS.log"
