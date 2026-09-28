#!/bin/bash
# ec2-crash-test.sh: crash consistency of the ext2 journal on LIVE EBS. The local
# harness simulates a crash by killing QEMU; here the "pulled power cord" is
# stop-instances --force (a hard power-off), after which the EBS root volume
# persists and the instance starts again with different user-data.
#
# Usage:
#   ENVF=... PEM=... infra/aws/ec2-crash-test.sh "<phase1>" "<phase2>" [type] [ami]
#   e.g.: "talaria.test=journal1" "talaria.test=journal2"
set -euo pipefail
UD1=${1:?no phase 1 user-data}
UD2=${2:?no phase 2 user-data}
TYPE=${3:-c6i.large}
ENVF=${ENVF:?no ENVF}; . "$ENVF"
W=$(dirname "$ENVF")
AMI=${4:-$(sed -n 's/^AMI=//p' "$W/last-node.sh" | tail -1)}
[ -n "$AMI" ] || { echo "no AMI found"; exit 1; }
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
TS=$(date +%H%M%S)

console_of() {
  raw=$(aws ec2 get-console-output --instance-id "$1" --latest --output text --query Output 2>/dev/null || true)
  [ -z "$raw" ] && return 0
  dec=$(printf '%s' "$raw" | base64 -d 2>/dev/null || true)
  case "$dec" in *talaria*|*uefi-stub*) printf '%s' "$dec"; return 0;; esac
  printf '%s' "$raw"
}
wait_for() { # wait_for IID PATTERN SECONDS
  local d=$(( $(date +%s) + $3 ))
  while [ "$(date +%s)" -lt "$d" ]; do
    sleep 15
    OUT=$(console_of "$1"); [ -z "$OUT" ] && continue
    echo "$OUT" > "$W/crash-$TS.log"
    echo "$OUT" | grep -qaE "$2" && return 0
  done
  return 1
}

IID=$(aws ec2 run-instances --image-id "$AMI" --instance-type "$TYPE" --key-name talaria-v11 \
  --security-group-ids "$SG" --subnet-id "$SUBNET" --associate-public-ip-address \
  --user-data "$UD1" \
  --tag-specifications "ResourceType=instance,Tags=[{Key=Project,Value=talaria-v11},{Key=Name,Value=talaria-crash-$TS}]" \
  --query 'Instances[0].InstanceId' --output text)
echo "instance $IID" >> "$W/resources.txt"
trap 'aws ec2 terminate-instances --instance-ids "$IID" >/dev/null 2>&1 || true; echo "== terminate $IID"' EXIT
echo "== phase 1 ($UD1) on $IID"
wait_for "$IID" "COMMITTED|ALL OK|FAILED|FAILURE" 420 || { echo "phase 1 did not report"; tail -20 "$W/crash-$TS.log"; exit 1; }
grep -aE "journal1:|ext2: a JBD2 journal" "$W/crash-$TS.log" | tail -4

echo "== CRASH: stop --force (hard power-off, the EBS volume persists)"
aws ec2 stop-instances --instance-ids "$IID" --force >/dev/null
aws ec2 wait instance-stopped --instance-ids "$IID"

echo "== phase 2 ($UD2): the same volume, new user-data"
# PITFALL: modify-instance-attribute wants user-data ALREADY base64-encoded (unlike
# run-instances, which encodes it itself). Without that phase 2 starts with an empty
# cmdline tail and silently checks nothing.
aws ec2 modify-instance-attribute --instance-id "$IID" --user-data "Value=$(printf '%s' "$UD2" | base64 -w0)"
aws ec2 start-instances --instance-ids "$IID" >/dev/null
aws ec2 wait instance-running --instance-ids "$IID"
wait_for "$IID" "RECOVERED OK|FAILED|ALL OK|FAILURE" 420 || { echo "phase 2 did not report"; tail -20 "$W/crash-$TS.log"; exit 1; }
grep -aE "THE JOURNAL HAS BEEN REPLAYED|journal2:|ext2: a JBD2 journal" "$W/crash-$TS.log" | tail -5
