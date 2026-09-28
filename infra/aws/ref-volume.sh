#!/bin/bash
# ref-volume.sh: take a SLICE of a reference volume (AL2023 by default) for a
# byte-by-byte comparison with our image.
#
# Why: on the same c6i.metal, in the same legacy-bios mode, the Amazon volume
# is executed and ours is not (a control showed the console channel works, so our
# marker's silence is real). The difference is in the volume itself. Hypotheses
# about it cost three sessions; comparing BYTES with another implementation had
# once already proven cheaper than any hypothesis (that is how ENA accel_mode was found).
#
# What it does: brings up a cheap helper, creates a volume from the reference AMI's
# snapshot, attaches it, takes the layout and raw areas (sector 0, GPT, the core.img
# gap, the tail with the backup GPT) and fetches them to the host. The instance and volume are destroyed.
#
# Usage:
#   infra/aws/ref-volume.sh [ami-id] [output-dir]
#   env: AWS_INI (credentials), KEEP=1 — keep the helper.
set -euo pipefail
REF_AMI=${1:-}
OUT=${2:-$HOME/.talaria-lab/ref}
AWS_INI=${AWS_INI:-}   # optional: a file "Access key=…/Secret=…"; otherwise the AWS CLI chain (~/.aws, AWS_PROFILE, env)
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
if [ -z "${AWS_ACCESS_KEY_ID:-}" ] && [ -n "$AWS_INI" ] && [ -f "$AWS_INI" ]; then
  AWS_ACCESS_KEY_ID=$(sed -n 's/^ *Access key *= *//Ip' "$AWS_INI" | tr -d '\r')
  AWS_SECRET_ACCESS_KEY=$(sed -n 's/^ *Secret[ a-z]* *= *//Ip' "$AWS_INI" | tr -d '\r')
  export AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
fi
W=$HOME/.talaria-lab
mkdir -p "$W" "$OUT"

[ -n "$REF_AMI" ] || REF_AMI=$(aws ssm get-parameters \
  --names /aws/service/ami-amazon-latest/al2023-ami-kernel-default-x86_64 \
  --query 'Parameters[0].Value' --output text 2>/dev/null || true)
[ -n "$REF_AMI" ] && [ "$REF_AMI" != None ] || REF_AMI=$(aws ssm get-parameters \
  --names /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64 \
  --query 'Parameters[0].Value' --output text)
echo "== reference AMI: $REF_AMI"
SNAP=$(aws ec2 describe-images --image-ids "$REF_AMI" \
  --query 'Images[0].BlockDeviceMappings[0].Ebs.SnapshotId' --output text)
SNAPSZ=$(aws ec2 describe-images --image-ids "$REF_AMI" \
  --query 'Images[0].BlockDeviceMappings[0].Ebs.VolumeSize' --output text)
echo "== root volume snapshot: $SNAP ($SNAPSZ GiB)"

# ── key and security group (the stand may have been torn down by lab-down.sh) ─
HAVE_KEY=$(aws ec2 describe-key-pairs --key-names talaria-v11 --query 'KeyPairs[0].KeyName' --output text 2>/dev/null || echo None)
if [ ! -f "$W/talaria-v11.pem" ] || [ "$HAVE_KEY" != talaria-v11 ]; then
  aws ec2 delete-key-pair --key-name talaria-v11 >/dev/null 2>&1 || true
  rm -f "$W/talaria-v11.pem"
  aws ec2 create-key-pair --key-name talaria-v11 --query KeyMaterial --output text > "$W/talaria-v11.pem"
  chmod 600 "$W/talaria-v11.pem"
fi
VPC=$(aws ec2 describe-vpcs --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text)
SUBNET=$(aws ec2 describe-subnets --filters Name=vpc-id,Values="$VPC" \
  --query 'Subnets[?MapPublicIpOnLaunch==`true`]|[0].SubnetId' --output text)
AZ=$(aws ec2 describe-subnets --subnet-ids "$SUBNET" --query 'Subnets[0].AvailabilityZone' --output text)
SG=$(aws ec2 describe-security-groups --filters Name=group-name,Values=talaria-v11 Name=vpc-id,Values="$VPC" \
  --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null || true)
if [ "$SG" = None ] || [ -z "$SG" ]; then
  SG=$(aws ec2 create-security-group --group-name talaria-v11 --description "Talaria v1.1 lab" \
    --vpc-id "$VPC" --query GroupId --output text)
  MYIP=$(curl -s https://checkip.amazonaws.com || echo 0.0.0.0)
  aws ec2 authorize-security-group-ingress --group-id "$SG" --protocol tcp --port 22 --cidr "$MYIP/32" >/dev/null
  aws ec2 authorize-security-group-ingress --group-id "$SG" --protocol -1 --source-group "$SG" >/dev/null
fi

HELPER_AMI=$(aws ssm get-parameters --names /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64 \
  --query 'Parameters[0].Value' --output text)
IID=$(aws ec2 run-instances --image-id "$HELPER_AMI" --instance-type t3.small --key-name talaria-v11 \
  --security-group-ids "$SG" --subnet-id "$SUBNET" --associate-public-ip-address \
  --placement "AvailabilityZone=$AZ" \
  --tag-specifications 'ResourceType=instance,Tags=[{Key=Project,Value=talaria-v11},{Key=Name,Value=talaria-refdump}]' \
  --query 'Instances[0].InstanceId' --output text)
echo "instance $IID" >> "$W/resources.txt"
VOL=""
cleanup() {
  [ "${KEEP:-0}" = 1 ] && return 0
  [ -n "$VOL" ] && { aws ec2 detach-volume --volume-id "$VOL" >/dev/null 2>&1 || true; sleep 10;
                     aws ec2 delete-volume --volume-id "$VOL" >/dev/null 2>&1 || true; }
  aws ec2 terminate-instances --instance-ids "$IID" >/dev/null 2>&1 || true
}
trap cleanup EXIT
aws ec2 wait instance-running --instance-ids "$IID"
IP=$(aws ec2 describe-instances --instance-ids "$IID" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
echo "== helper $IID ($IP)"

# A volume FROM THE reference's SNAPSHOT — exactly what the metal firmware sees on Amazon.
VOL=$(aws ec2 create-volume --snapshot-id "$SNAP" --availability-zone "$AZ" --volume-type gp3 \
  --tag-specifications 'ResourceType=volume,Tags=[{Key=Project,Value=talaria-v11}]' \
  --query VolumeId --output text)
echo "volume $VOL" >> "$W/resources.txt"
aws ec2 wait volume-available --volume-ids "$VOL"
aws ec2 attach-volume --volume-id "$VOL" --instance-id "$IID" --device /dev/sdg >/dev/null
aws ec2 wait volume-in-use --volume-ids "$VOL"
echo "== volume $VOL attached as /dev/sdg"

SSH="ssh -i $W/talaria-v11.pem -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10"
for i in $(seq 1 40); do $SSH ec2-user@"$IP" true 2>/dev/null && break; sleep 5; done

# ── taking the slice on the helper ───────────────────────────────────────────
# The reference disk is the second NVMe (the first is the helper's own root). It is found
# by the volume serial number, not by device number: the nvme order is not guaranteed.
$SSH ec2-user@"$IP" "sudo dnf install -q -y gdisk >/dev/null 2>&1 || true; true"
$SSH ec2-user@"$IP" "VID=\$(echo $VOL | tr -d '-'); D=\$(for d in /dev/nvme*n1; do s=\$(sudo nvme id-ctrl \$d 2>/dev/null | sed -n 's/^sn *: *//p' | tr -d ' '); case \"\$s\" in \$VID*) echo \$d;; esac; done | head -1); [ -n \"\$D\" ] || D=/dev/nvme1n1; echo \"DISK=\$D\";
  sudo sgdisk -p \$D > /tmp/ref-sgdisk.txt 2>&1;
  sudo sfdisk -d \$D > /tmp/ref-sfdisk.txt 2>&1;
  sudo sgdisk -v \$D > /tmp/ref-verify.txt 2>&1;
  for n in 1 2 3 4 127 128; do echo \"--- partition \$n\"; sudo sgdisk -i \$n \$D; done > /tmp/ref-parts.txt 2>&1;
  sudo blkid \$D* > /tmp/ref-blkid.txt 2>&1 || true;
  sudo dd if=\$D of=/tmp/ref-head.bin bs=512 count=8192 status=none;
  SZ=\$(sudo blockdev --getsz \$D); echo \"SECTORS=\$SZ\" >> /tmp/ref-sgdisk.txt;
  sudo dd if=\$D of=/tmp/ref-tail.bin bs=512 skip=\$((SZ-34)) count=34 status=none;
  # The whole BIOS boot partition (type ef02) — the loader's core.img lies there, and
  # the MBR pointer points exactly at it. It has no fixed offset:
  # on Amazon it is sector 22528, ours is 2048, so it is taken from the table.
  BB=\$(sudo sfdisk -d \$D | sed -n 's/.*start= *\\([0-9]*\\).*21686148-6449-6E6F-744E-656564454649.*/\\1/p' | head -1);
  if [ -n \"\$BB\" ]; then echo \"BIOSBOOT_LBA=\$BB\" >> /tmp/ref-sgdisk.txt;
    sudo dd if=\$D of=/tmp/ref-biosboot.bin bs=512 skip=\$BB count=2048 status=none; fi
  # The ESP tree: what the firmware would find on the UEFI path.
  EP=\$(sudo sfdisk -d \$D | sed -n 's/^\\([^ ]*\\) *: .*C12A7328-F81F-11D2-BA4B-00A0C93EC93B.*/\\1/p' | head -1);
  if [ -n \"\$EP\" ]; then sudo mkdir -p /mnt/refesp && sudo mount \$EP /mnt/refesp 2>/dev/null &&
    { sudo find /mnt/refesp -printf '%10s  %p\\n'; sudo umount /mnt/refesp; } > /tmp/ref-esp.txt 2>&1; fi
  ls -l /tmp/ref-*.bin"
scp -i "$W/talaria-v11.pem" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -q \
  ec2-user@"$IP":/tmp/ref-'*' "$OUT/"
echo "== reference slice in $OUT:"
ls -l "$OUT"
