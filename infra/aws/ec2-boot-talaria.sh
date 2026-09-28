#!/bin/bash
# ec2-boot-talaria.sh: build a Talaria image, write it to the EBS volume of a
# builder instance, take a snapshot, register an AMI (boot-mode uefi,
# ena-support) and launch an instance; print the boot log from the serial console.
#
# A fast loop without S3/import-snapshot: dd onto the attached volume -> create-snapshot
# (minutes) -> register-image. One iteration takes ~4-6 minutes and costs pennies.
#
# Usage: ec2-boot-talaria.sh "<cmdline>" [instance-type] [data.img]
#   env: ENVF (a file with BUILDER/BUILDER_IP/SG/SUBNET), PEM, KEEP=1 (do not terminate)
set -euo pipefail
CMDLINE=${1:?usage: ec2-boot-talaria.sh "<cmdline>" [type] [data.img]}
TYPE=${2:-c6i.large}
DATA=${3:-kernel/disk.img}
REPO=$(cd "$(dirname "$0")/../.." && pwd)
ENVF=${ENVF:?no ENVF}
PEM=${PEM:?no PEM}
. "$ENVF"
# Credentials: a ~/.aws profile (AWS_PROFILE, default "talaria") OR keys in the
# environment (lab-up.sh can load them from an AWS_INI file).
if [ -z "${AWS_ACCESS_KEY_ID:-}" ]; then export AWS_PROFILE=${AWS_PROFILE:-talaria}; fi
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
SSH="ssh -i $PEM -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10"
W=$(dirname "$ENVF")
TS=$(date +%H%M%S)

echo "== building the image: $CMDLINE"
# DATA may be relative (to the repository root) or an absolute path.
case "$DATA" in /*) DATA_PATH="$DATA";; *) DATA_PATH="$REPO/$DATA";; esac
[ -f "$DATA_PATH" ] || { echo "no data image: $DATA_PATH"; exit 1; }
# NO `|| true`: a silent build failure used to ship a STALE image from the previous
# iteration into the AMI (and a whole ~15-minute cycle went into debugging the wrong kernel).
rm -f "$W/talaria-ec2.img"
# HYBRID=1 — a hybrid image (BIOS+UEFI) instead of a pure UEFI ESP.
# Needed for *.metal: it does not accept an AMI with boot-mode uefi.
if [ "${HYBRID:-0}" = 1 ]; then
  HYBRID=1 "$REPO/tools/mk-bios.sh" "$W/talaria-ec2.img" "$DATA_PATH" "$CMDLINE" | grep -a 'mk-bios: /'
elif [ "${HYBRID:-0}" = bios ]; then
  # a plain MBR image (no GPT and no ESP) — for BOOTMODE=legacy-bios
  HYBRID=0 "$REPO/tools/mk-bios.sh" "$W/talaria-ec2.img" "$DATA_PATH" "$CMDLINE" | grep -a 'mk-bios: /'
else
  "$REPO/tools/mk-ami.sh" "$W/talaria-ec2.img" "$DATA_PATH" "$CMDLINE" | grep -a 'mk-ami: /'
fi
[ -f "$W/talaria-ec2.img" ] || { echo "the image was not built"; exit 1; }
# FULLDISK=<size> — stretch the image to EXACTLY the size of the volume it will
# land on, and move the GPT backup header to the end of the disk (`sgdisk -e`).
#
# Why: our image is ~107 MB, while the builder's volume (and hence the AMI root
# volume) is 2 GiB. dd leaves a disk whose primary GPT says "the disk ends at
# 107 MB", with NO backup header in the last sector at all. UEFI survives that
# (the primary one is enough for it), but a CSM that checks the whole GPT is
# entitled to reject such a disk — and then sector 0 code is NOT EXECUTED AT ALL,
# which is what we see on *.metal (the marker is silent for 8 minutes).
# The image stays sparse, and gzip squeezes the tail of zeros to almost nothing.
if [ -n "${FULLDISK:-}" ]; then
  truncate -s "$FULLDISK" "$W/talaria-ec2.img"
  if sgdisk -e "$W/talaria-ec2.img" >/dev/null 2>&1; then
    echo "== image stretched to $FULLDISK, backup GPT moved to the end of the disk"
  else
    echo "== WARNING: sgdisk -e failed (GPT not rewritten)"
  fi
fi
# MARKER=1 — replace the MBR code with a marker that prints a line to COM1 with
# its first instruction. *.metal diagnostics: does the firmware reach the MBR at all.
# Strictly AFTER sgdisk: it rewrites the protective MBR too, erasing the marker.
[ "${MARKER:-0}" = 1 ] && "$REPO/tools/mbr-marker.sh" "$W/talaria-ec2.img"
gzip -cf "$W/talaria-ec2.img" > "$W/talaria-ec2.img.gz"

echo "== uploading to the builder ($BUILDER_IP) -> /dev/nvme1n1"
scp -i "$PEM" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -q "$W/talaria-ec2.img.gz" ec2-user@"$BUILDER_IP":/tmp/
$SSH ec2-user@"$BUILDER_IP" 'sudo sh -c "gzip -dc /tmp/talaria-ec2.img.gz | dd of=/dev/nvme1n1 bs=1M status=none && sync"'

VOL=$(aws ec2 describe-instances --instance-ids "$BUILDER" \
  --query 'Reservations[0].Instances[0].BlockDeviceMappings[?DeviceName==`/dev/sdf`].Ebs.VolumeId' --output text)
echo "== snapshot of volume $VOL"
SNAP=$(aws ec2 create-snapshot --volume-id "$VOL" --description "talaria-v11 $TS" \
  --tag-specifications 'ResourceType=snapshot,Tags=[{Key=Project,Value=talaria-v11}]' --query SnapshotId --output text)
echo "snapshot $SNAP" >> "$W/resources.txt"
aws ec2 wait snapshot-completed --snapshot-ids "$SNAP"

# BOOTMODE: uefi (default), uefi-preferred (hybrid: ordinary instances via
# UEFI, *.metal via BIOS), legacy-bios (BIOS only).
BOOTMODE=${BOOTMODE:-uefi}
echo "== register-image (boot-mode $BOOTMODE + ena-support)"
AMI=$(aws ec2 register-image --name "talaria-v11-$TS" --architecture x86_64 \
  --root-device-name /dev/xvda --boot-mode "$BOOTMODE" --ena-support --virtualization-type hvm \
  --block-device-mappings "[{\"DeviceName\":\"/dev/xvda\",\"Ebs\":{\"SnapshotId\":\"$SNAP\",\"DeleteOnTermination\":true,\"VolumeType\":\"gp3\"}}]" \
  --query ImageId --output text)
echo "ami $AMI" >> "$W/resources.txt"
echo "AMI: $AMI"

echo "== run-instances ($TYPE)"
# CPG=<placement group> — put the instance under test into the same group as the Linux
# neighbour: otherwise RTT measures the distance between racks, not the stack (see lab-up.sh).
PLACE=""
[ -n "${CPG:-}" ] && PLACE="--placement GroupName=$CPG${AZ:+,AvailabilityZone=$AZ}"
# UD=<cmdline tail> — the instance's user-data. One AMI is built with the
# CANONICAL cmdline, and the experiment mode arrives as a tail: ena-rtt.sh and
# ena-load.sh work the same way. Here it lets one launch (especially an expensive
# metal one) both boot and come up in the mode needed for the measurements.
IID=$(aws ec2 run-instances --image-id "$AMI" --instance-type "$TYPE" --key-name talaria-v11 \
  --security-group-ids "$SG" --subnet-id "$SUBNET" --associate-public-ip-address $PLACE \
  ${UD:+--user-data "$UD"} \
  --tag-specifications 'ResourceType=instance,Tags=[{Key=Project,Value=talaria-v11},{Key=Name,Value=talaria-node}]' \
  --query 'Instances[0].InstanceId' --output text)
echo "instance $IID" >> "$W/resources.txt"
PRIV=$(aws ec2 describe-instances --instance-ids "$IID" --query 'Reservations[0].Instances[0].PrivateIpAddress' --output text)
echo "TALARIA_IID=$IID"; echo "TALARIA_PRIV=$PRIV"
echo "TALARIA_IID=$IID" > "$W/last-node.sh"; echo "TALARIA_PRIV=$PRIV" >> "$W/last-node.sh"
echo "AMI=$AMI" >> "$W/last-node.sh"; echo "SNAP=$SNAP" >> "$W/last-node.sh"

# AWS CLI v2 returns Output ALREADY decoded, v1 returns base64. Try both:
# a blind `| base64 -d` on v2 gave nothing, and the log looked like "the instance is silent".
console_of() {
  raw=$(aws ec2 get-console-output --instance-id "$1" --latest --output text --query Output 2>/dev/null || true)
  [ -z "$raw" ] && return 0
  dec=$(printf '%s' "$raw" | base64 -d 2>/dev/null || true)
  case "$dec" in *talaria*|*uefi-stub*) printf '%s' "$dec"; return 0;; esac
  printf '%s' "$raw"
}
# TRIES/WAITFOR: how many times to poll the console (every 10 s) and what to wait for.
# Without WAITFOR the loop exits on the FIRST non-empty output — on *.metal that is the
# firmware banner, with minutes to go before the useful log. For metal set
# WAITFOR (e.g. TALARIA-MBR) and a larger TRIES.
TRIES=${TRIES:-24}
echo "== serial console (get-console-output, up to $((TRIES * 10 / 60)) min${WAITFOR:+, waiting for /$WAITFOR/})"
for i in $(seq 1 "$TRIES"); do
  sleep 10
  OUT=$(console_of "$IID")
  [ -z "$OUT" ] && continue
  echo "$OUT" > "$W/console-$TS.log"
  if [ -n "${WAITFOR:-}" ]; then
    printf '%s' "$OUT" | grep -aqE "$WAITFOR" || { [ "$i" -lt "$TRIES" ] && continue; }
  fi
  echo "--- serial (attempt $i) ---"; echo "$OUT" | tail -40; break
done
[ "${KEEP:-0}" = 1 ] || { echo "== terminate $IID"; aws ec2 terminate-instances --instance-ids "$IID" >/dev/null; }
