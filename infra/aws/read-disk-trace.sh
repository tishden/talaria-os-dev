#!/bin/bash
# read-disk-trace.sh: read the kernel/loader trace from an EBS volume.
#
# Why. On bare metal the instance console shows neither COM1 nor the screen, and
# it turned out that on a VIRTUAL instance booting via BIOS it is empty
# too. The only channel is what the kernel wrote to disk itself
# (`talaria.trace=<LBA>`) and what the loader wrote (sector 2047). This script
# brings the volume to a "readable" state and extracts both traces.
#
# Usage:
#   infra/aws/read-disk-trace.sh <instance-id|volume-id> [log_LBA] [loader_LBA]
#     * instance-id — the instance is terminated (its root volume must have
#       DeleteOnTermination=false), then its root volume is read;
#     * volume-id   — the volume is read as is (it must be available).
#   env: KEEPHELPER=<id> — use a ready helper instead of a new one,
#        LSPART=<n> — show the contents of partition n (traces of the kernel's work),
#        DUMP=<sectors> — how many log sectors to print (default 128).
set -euo pipefail
TARGET=${1:?usage: read-disk-trace.sh <instance-id|volume-id> [log_LBA] [loader_LBA]}
TLBA=${2:-1024}
BLBA=${3:-2047}
DUMP=${DUMP:-128}
W=${WORKDIR:-$HOME/.talaria-lab}
AWS_INI=${AWS_INI:-}   # optional: a file "Access key=…/Secret=…"; otherwise the AWS CLI chain (~/.aws, AWS_PROFILE, env)
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
if [ -z "${AWS_ACCESS_KEY_ID:-}" ] && [ -f "$AWS_INI" ]; then
  AWS_ACCESS_KEY_ID=$(sed -n 's/^ *Access key *= *//Ip' "$AWS_INI" | tr -d '\r')
  AWS_SECRET_ACCESS_KEY=$(sed -n 's/^ *Secret[ a-z]* *= *//Ip' "$AWS_INI" | tr -d '\r')
  export AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
fi
mkdir -p "$W"
SSH="ssh -i $W/talaria-v11.pem -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10"

case "$TARGET" in
  i-*) VOL=$(aws ec2 describe-instances --instance-ids "$TARGET" \
         --query 'Reservations[0].Instances[0].BlockDeviceMappings[0].Ebs.VolumeId' --output text)
       echo "== root volume of instance $TARGET: $VOL; terminating the instance"
       aws ec2 terminate-instances --instance-ids "$TARGET" >/dev/null
       # `aws ec2 wait volume-available` does not wait long enough after metal — our own loop
       for _ in $(seq 1 80); do
         st=$(aws ec2 describe-volumes --volume-ids "$VOL" --query 'Volumes[0].State' --output text 2>/dev/null || echo unknown)
         [ "$st" = available ] && break
         sleep 15
       done
       echo "   volume $VOL: $st";;
  vol-*) VOL=$TARGET;;
  *) echo "unknown target: $TARGET"; exit 1;;
esac

AZ=$(aws ec2 describe-volumes --volume-ids "$VOL" --query 'Volumes[0].AvailabilityZone' --output text)
VPC=$(aws ec2 describe-vpcs --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text)
SUBNET=$(aws ec2 describe-subnets --filters Name=vpc-id,Values="$VPC" Name=availability-zone,Values="$AZ" \
  --query 'Subnets[?MapPublicIpOnLaunch==`true`]|[0].SubnetId' --output text)
SG=$(aws ec2 describe-security-groups --filters Name=group-name,Values=talaria-v11 Name=vpc-id,Values="$VPC" \
  --query 'SecurityGroups[0].GroupId' --output text)

HELPER=${KEEPHELPER:-}
OWN=0
if [ -z "$HELPER" ]; then
  OWN=1
  HELPER=$(aws ec2 run-instances --image-id "$(aws ssm get-parameters \
      --names /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64 \
      --query 'Parameters[0].Value' --output text)" \
    --instance-type t3.small --key-name talaria-v11 --security-group-ids "$SG" --subnet-id "$SUBNET" \
    --associate-public-ip-address --placement "AvailabilityZone=$AZ" \
    --tag-specifications 'ResourceType=instance,Tags=[{Key=Project,Value=talaria-v11},{Key=Name,Value=talaria-traceread}]' \
    --query 'Instances[0].InstanceId' --output text)
  echo "instance $HELPER" >> "$W/resources.txt"
  aws ec2 wait instance-running --instance-ids "$HELPER"
fi
HIP=$(aws ec2 describe-instances --instance-ids "$HELPER" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
cleanup() {
  aws ec2 detach-volume --volume-id "$VOL" >/dev/null 2>&1 || true
  [ "$OWN" = 1 ] && [ "${KEEP:-0}" != 1 ] && aws ec2 terminate-instances --instance-ids "$HELPER" >/dev/null 2>&1 || true
}
trap cleanup EXIT
aws ec2 attach-volume --volume-id "$VOL" --instance-id "$HELPER" --device /dev/sdg >/dev/null
aws ec2 wait volume-in-use --volume-ids "$VOL"
for _ in $(seq 1 40); do $SSH ec2-user@"$HIP" true 2>/dev/null && break; sleep 5; done

$SSH ec2-user@"$HIP" "set -e; VID=\$(echo $VOL | tr -d '-');
  D=\$(lsblk -dno NAME,SERIAL | awk -v v=\"\$VID\" '\$2 ~ \"^\"v {print \"/dev/\"\$1}' | head -1);
  [ -n \"\$D\" ] || { echo 'volume not found'; lsblk; exit 1; };
  echo \"== disk \$D\";
  echo '--- LOADER trace (sector $BLBA): phase / int 13h status / remainder';
  sudo dd if=\$D bs=512 skip=$BLBA count=1 status=none > /tmp/b.bin;
  hexdump -C /tmp/b.bin | head -2;
  echo '--- KERNEL trace header (sector $TLBA):';
  sudo dd if=\$D bs=512 skip=$TLBA count=1 status=none > /tmp/h.bin;
  hexdump -C /tmp/h.bin | head -3;
  if [ -n \"${SLOTS:-}\" ]; then
    echo '--- suite state (sector 1004): magic, done, total';
    sudo dd if=\$D bs=512 skip=1004 count=1 status=none > /tmp/st.bin;
    head -c 13 /tmp/st.bin; echo; od -An -tu4 -j16 -N8 /tmp/st.bin;
    for i in \$(seq 0 $(( ${SLOTS:-1} ))); do
      OFF=\$(( $TLBA + i * 129 ));
      echo \"=== slot \$i (LBA \$OFF)\";
      sudo dd if=\$D bs=512 skip=\$OFF count=1 status=none | tr -d '\\000' | head -1;
      sudo dd if=\$D bs=512 skip=\$(( OFF + 1 )) count=128 status=none | tr -d '\\000' > /tmp/s.txt;
      grep -aE 'suite: run|ALL OK|FAILURE|FAILED|p50=|speedup|IPC|Mops|ns|us|pkt/s|blocks' /tmp/s.txt | head -30;
    done;
  fi;
  echo '--- kernel log ($DUMP sectors from $((TLBA + 1))):';
  sudo dd if=\$D bs=512 skip=$((TLBA + 1)) count=$DUMP status=none > /tmp/k.bin;
  if head -c 64 /tmp/k.bin | grep -aq .; then :; fi;
  strings -n 4 /tmp/k.bin | head -400;
  echo '--- (tail entropy: how many zero bytes out of 65536)';
  od -An -tu1 -v /tmp/k.bin | tr ' ' '\n' | grep -c '^0$' || true;
  if [ -n \"${LSPART:-}\" ]; then
    echo '--- partition ${LSPART:-} (traces of the KERNEL ITSELF at work):';
    sudo mount -o ro \${D}p${LSPART:-2} /mnt 2>&1 | head -2;
    sudo find /mnt -maxdepth 3 -printf '%10s  %p\n' 2>/dev/null | head -20;
    sudo umount /mnt 2>/dev/null || true;
  fi"
echo "== volume $VOL read (the volume itself is NOT deleted: $VOL)"
