#!/bin/bash
# talaria-deploy.sh — deploy a workload on Talaria in AWS with one command:
# binary -> disk image -> EBS volume -> snapshot -> AMI -> a running instance.
#
# Why a separate tool. Until now the path to EC2 existed only as diagnostic
# scripts (`metal-ref-probe.sh` — "a probe against the reference",
# `ec2-boot-talaria.sh` — a development cycle on top of a pre-existing builder).
# A workload can be deployed with either, but both require knowing which knobs are
# for diagnostics and which are for the job. Here there is only the job.
#
# Usage:
#   infra/aws/talaria-deploy.sh --app ./myapp [options]
#
#   --app PATH        the workload: a static musl binary (check it with
#                     kernel/tools/talaria-check ./myapp)
#   --args "..."      workload arguments (argv[1..])
#   --env "K=V;K=V"   workload environment variables
#   --restart N       restarts on failure (the supervisor), 0 = do not restart
#   --type TYPE       instance type (default c6i.large; *.metal — bare metal; a Graviton
#                     type — c7g.large, m7g.*, c8g.* — selects arm64)
#   --arch ARCH       x86_64 or arm64 (default: from the binary's e_machine, else from --type);
#                     arm64 boots UEFI through the aarch64 stub (EC2's arm64 is UEFI-only)
#   --smp N           how many cores to give the workload (BSP + N-1 APs); 1 = BSP only
#   --net dhcp|none   network (ENA + DHCP) or no network
#   --mode MODE       kernel mode (default cell; proc — the process model with
#                     fork/signals, used by the bench/suite servers)
#   --userdata        read the cmdline tail from the instance's user-data
#                     (talaria.userdata=1): ONE AMI for several roles, the role is
#                     chosen at launch instead of rebuilding the image
#   --stop-after SEC  power the machine off after SEC seconds (ACPI S5 -> the instance
#                     goes to stopped: handy for one-off runs)
#   --extra "..."     an arbitrary cmdline tail (talaria.*)
#   --disk-gb N       root volume size (default: from the image size)
#   --data PATH       a ready data image instead of building one from --app
#   --name NAME       instance/AMI name (default talaria-<time>)
#   --dry-run         only build the image and print the plan
#   --ami-only        stop after the AMI (someone else launches the instance —
#                     e.g. the bench/suite terraform stand; prints ami-...)
#   KEEP_IMG=1        (environment) do not delete the intermediate images from
#                     ~/.talaria-lab after the AMI is registered (by default they
#                     are deleted: the snapshot replaces them entirely)
#   --keep-disk       the root volume SURVIVES the instance (DeleteOnTermination=false):
#                     needed on metal, where the kernel trace is read from the volume
#                     (read-disk-trace.sh). By default the volume dies with the
#                     instance — otherwise every run left a billable volume behind
#                     (a "volume leak"), and the stand did not notice.
#
# The boot chain is chosen by instance type: `*.metal` can only do BIOS, so there
# it uses our own loader (`tools/mk-tboot.sh`); on virtual instances it works
# exactly the same way, so it is used everywhere — one path instead of two. arm64 is the
# exception by necessity: a Graviton boots UEFI only, so there the image is an ESP with the
# aarch64 stub (`tools/mk-ami.sh`, `kernel/uefi-stub/build.sh aarch64` after the kernel build),
# and the console works (the 16550 behind PCI) — no disk trace is needed or laid.
#
# What is needed: the aws cli with EC2 permissions, the talaria-v11 key (created
# automatically), a default VPC with a public subnet. A t3.small helper comes up for
# a minute to write the image onto the volume, and is terminated.
set -euo pipefail

REPO=$(cd "$(dirname "$0")/../.." && pwd)
W=${WORKDIR:-$HOME/.talaria-lab}
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}

APP=""; ARGS=""; ENVS=""; RESTART=3; TYPE=c6i.large; SMP=1; NET=dhcp; MODE=cell; USERDATA=0
STOP_AFTER=""; EXTRA=""; DISK_GB=""; DATA=""; NAME=""; DRY=0; AMI_ONLY=0; DOT=true; ARCH=""
while [ $# -gt 0 ]; do
  case "$1" in
    --app) APP=$2; shift 2;;
    --args) ARGS=$2; shift 2;;
    --env) ENVS=$2; shift 2;;
    --restart) RESTART=$2; shift 2;;
    --type) TYPE=$2; shift 2;;
    --arch) ARCH=$2; shift 2;;
    --smp) SMP=$2; shift 2;;
    --net) NET=$2; shift 2;;
    --mode) MODE=$2; shift 2;;
    --userdata) USERDATA=1; shift;;
    --stop-after) STOP_AFTER=$2; shift 2;;
    --extra) EXTRA=$2; shift 2;;
    --disk-gb) DISK_GB=$2; shift 2;;
    --data) DATA=$2; shift 2;;
    --name) NAME=$2; shift 2;;
    --dry-run) DRY=1; shift;;
    --ami-only) AMI_ONLY=1; shift;;
    --keep-disk) DOT=false; shift;;
    -h|--help) sed -n '2,54p' "$0"; exit 0;;
    *) echo "unknown argument: $1 (see --help)"; exit 1;;
  esac
done
[ -n "$APP" ] || [ -n "$DATA" ] || { echo "need --app <binary> or --data <data image>"; exit 1; }
NAME=${NAME:-talaria-$(date +%Y%m%d-%H%M%S)}
mkdir -p "$W"
# The architecture: said, else the binary's own e_machine (0xB7 AArch64, 0x3E x86-64), else
# the instance type's family (a "g" after the generation: c7g, m6g, c8g, r7gd...). A binary of
# one machine on an instance of the other cannot run, so a mismatch stops here.
if [ -z "$ARCH" ] && [ -n "$APP" ] && [ -f "$APP" ]; then
  case "$(od -An -tx1 -j18 -N2 "$APP" | tr -d ' ')" in b700) ARCH=arm64;; 3e00) ARCH=x86_64;; esac
fi
if [ -z "$ARCH" ]; then case "$TYPE" in [a-z]*[0-9]g*.*) ARCH=arm64;; *) ARCH=x86_64;; esac; fi
case "$ARCH" in aarch64) ARCH=arm64;; arm64|x86_64) ;; *) echo "--arch: x86_64 or arm64, not $ARCH"; exit 1;; esac
[ "$ARCH" = arm64 ] && [ "$TYPE" = c6i.large ] && TYPE=c7g.large
case "$TYPE" in [a-z]*[0-9]g*.*) TARCH=arm64;; *) TARCH=x86_64;; esac
[ "$TARCH" = "$ARCH" ] || { echo "the workload is $ARCH, the instance type $TYPE is $TARCH"; exit 1; }

# ── 1. data image: the workload + the manifest ──────────────────────────────
IMGDATA="$W/$NAME-data.img"
if [ -n "$DATA" ]; then
  cp "$DATA" "$IMGDATA"
else
  [ -f "$APP" ] || { echo "no binary: $APP"; exit 1; }
  PACK=("$REPO/kernel/tools/talaria-pack" -o "$IMGDATA" --fs ext2 --app "$APP" --restart "$RESTART")
  [ -n "$ARGS" ] && PACK+=(--args "$ARGS")
  [ -n "$ENVS" ] && PACK+=(--env "$ENVS")
  echo "== data image: $(basename "$APP") -> $IMGDATA"
  python3 "${PACK[@]}" | sed 's/^/   /'
fi

# ── 2. boot image: our own loader (BIOS) — one path for metal and for virtual
# instances ─────────────────────────────────────────────────────────────────
# talaria.trace=2048 — the whole kernel log as raw sectors on disk. On *.metal this is
# the ONLY channel: the instance console shows neither COM1 nor the screen there, and
# without the trace debugging looks like "the instance is silent". The area for it is
# reserved by mk-tboot itself and costs nothing.
if [ "$ARCH" = arm64 ]; then
  CMDLINE="talaria.pci=1 talaria.mode=$MODE"   # the console is the SPCR's 16550; no trace area on an ESP image
else
  CMDLINE="console=ttyS0 talaria.pci=1 talaria.trace=2048 talaria.mode=$MODE"
fi
[ "$NET" = dhcp ] && CMDLINE="$CMDLINE talaria.ip=dhcp"
[ "$USERDATA" = 1 ] && CMDLINE="$CMDLINE talaria.userdata=1"
[ "$SMP" -gt 1 ] && CMDLINE="$CMDLINE talaria.smp=$SMP"
[ -n "$STOP_AFTER" ] && CMDLINE="$CMDLINE talaria.stop=$STOP_AFTER talaria.poweroff=1"
[ -n "$EXTRA" ] && CMDLINE="$CMDLINE $EXTRA"
IMG="$W/$NAME.img"
echo "== boot image: $IMG"
echo "   cmdline: $CMDLINE"
if [ "$ARCH" = arm64 ]; then
  ARCH=arm64 "$REPO/tools/mk-ami.sh" "$IMG" "$IMGDATA" "$CMDLINE" | sed 's/^/   /'
else
  "$REPO/tools/mk-tboot.sh" "$IMG" "$IMGDATA" "$CMDLINE" | sed 's/^/   /'
fi
IMG_B=$(stat -c%s "$IMG")
DISK_GB=${DISK_GB:-$(( (IMG_B + 1073741823) / 1073741824 ))}
[ "$DISK_GB" -lt 1 ] && DISK_GB=1
if [ "$DRY" = 1 ]; then
  echo "== dry-run: the image is built ($ARCH), the volume would be ${DISK_GB} GiB, type $TYPE"
  exit 0
fi

# ── 3. AWS environment (default VPC, the project's group and key) ───────────
VPC=$(aws ec2 describe-vpcs --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text)
SUBNET=$(aws ec2 describe-subnets --filters Name=vpc-id,Values="$VPC" \
  --query 'Subnets[?MapPublicIpOnLaunch==`true`]|[0].SubnetId' --output text)
AZ=$(aws ec2 describe-subnets --subnet-ids "$SUBNET" --query 'Subnets[0].AvailabilityZone' --output text)
SG=$(aws ec2 describe-security-groups --filters Name=group-name,Values=talaria-v11 Name=vpc-id,Values="$VPC" \
  --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null || true)
if [ "$SG" = None ] || [ -z "$SG" ]; then
  SG=$(aws ec2 create-security-group --group-name talaria-v11 --description "Talaria lab" \
    --vpc-id "$VPC" --query GroupId --output text)
  MYIP=$(curl -s https://checkip.amazonaws.com || echo 0.0.0.0)
  aws ec2 authorize-security-group-ingress --group-id "$SG" --protocol tcp --port 22 --cidr "$MYIP/32" >/dev/null
  aws ec2 authorize-security-group-ingress --group-id "$SG" --protocol -1 --source-group "$SG" >/dev/null
fi
# ── An SSH rule for the CURRENT address ─────────────────────────────────────
# The group is created once and lives for months, while the developer's public
# address changes (VPN, an ISP reconnect). A metal run failed exactly like that:
# the instance worked, but the helper refused ssh because the group held the
# PREVIOUS address. We check and add it on every run — it costs one API call.
MYIP=$(curl -s --max-time 10 https://checkip.amazonaws.com || true)
if [ -n "$MYIP" ]; then
  HAVE=$(aws ec2 describe-security-groups --group-ids "$SG" \
    --query "SecurityGroups[0].IpPermissions[?ToPort==\`22\`].IpRanges[].CidrIp" --output text 2>/dev/null || true)
  case " $HAVE " in
    *" $MYIP/32 "*) : ;;
    *) echo "== group $SG has no ssh access from $MYIP — adding it"
       aws ec2 authorize-security-group-ingress --group-id "$SG" --protocol tcp --port 22 \
         --cidr "$MYIP/32" >/dev/null 2>&1 || true;;
  esac
fi

# THE KEY FILE IS PER REGION. A key pair lives in one region, and the file used to be one for
# all: a bring-up in a second region recreated "its" pair and overwrote the file — and every
# later upload in the first region failed with "Connection closed" ten times in a row
# (2026-10-22, two stands in two regions at once). The default region keeps the old name.
PEM=$W/talaria-v11.pem
case "$AWS_DEFAULT_REGION" in ap-northeast-1) ;; *) PEM=$W/talaria-v11-$AWS_DEFAULT_REGION.pem;; esac
HAVE_KEY=$(aws ec2 describe-key-pairs --key-names talaria-v11 --query 'KeyPairs[0].KeyName' --output text 2>/dev/null || echo None)
if [ ! -f "$PEM" ] || [ "$HAVE_KEY" != talaria-v11 ]; then
  aws ec2 delete-key-pair --key-name talaria-v11 >/dev/null 2>&1 || true
  rm -f "$PEM"
  aws ec2 create-key-pair --key-name talaria-v11 --query KeyMaterial --output text > "$PEM"
  chmod 600 "$PEM"
fi
SSH="ssh -i $PEM -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10"
TAGS="ResourceType=instance,Tags=[{Key=Project,Value=talaria},{Key=Name,Value=$NAME}]"

HELPER=""; VOL=""
cleanup_partial() {
  [ -n "$VOL" ] && { aws ec2 detach-volume --volume-id "$VOL" >/dev/null 2>&1 || true
                     # WAIT FOR THE DETACH, DO NOT SLEEP EIGHT SECONDS. An attached
                     # volume is not deleted, the error was swallowed by `|| true`,
                     # and the volume stayed FOREVER — four of them piled up in one session.
                     aws ec2 wait volume-available --volume-ids "$VOL" >/dev/null 2>&1 || true
                     aws ec2 delete-volume --volume-id "$VOL" >/dev/null 2>&1 \
                       || echo "WARNING: volume $VOL not deleted — remove it by hand"; }
  [ -n "$HELPER" ] && aws ec2 terminate-instances --instance-ids "$HELPER" >/dev/null 2>&1 || true
}
trap cleanup_partial EXIT

# ── 4. volume + image upload through a disposable helper ────────────────────
echo "== volume ${DISK_GB} GiB in $AZ"
VOL=$(aws ec2 create-volume --size "$DISK_GB" --availability-zone "$AZ" --volume-type gp3 \
  --tag-specifications "ResourceType=volume,Tags=[{Key=Project,Value=talaria},{Key=Name,Value=$NAME}]" \
  --query VolumeId --output text)
aws ec2 wait volume-available --volume-ids "$VOL"
HELPER_AMI=$(aws ssm get-parameters --names /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64 \
  --query 'Parameters[0].Value' --output text)
HELPER=$(aws ec2 run-instances --image-id "$HELPER_AMI" --instance-type t3.small --key-name talaria-v11 \
  --security-group-ids "$SG" --subnet-id "$SUBNET" --associate-public-ip-address \
  --placement "AvailabilityZone=$AZ" \
  --tag-specifications "ResourceType=instance,Tags=[{Key=Project,Value=talaria},{Key=Name,Value=$NAME-uploader}]" \
  --query 'Instances[0].InstanceId' --output text)
aws ec2 wait instance-running --instance-ids "$HELPER"
HIP=$(aws ec2 describe-instances --instance-ids "$HELPER" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
aws ec2 attach-volume --volume-id "$VOL" --instance-id "$HELPER" --device /dev/sdg >/dev/null
aws ec2 wait volume-in-use --volume-ids "$VOL"
for _ in $(seq 1 40); do $SSH ec2-user@"$HIP" true 2>/dev/null && break; sleep 5; done
gzip -cf "$IMG" > "$IMG.gz"
# THE UPLOAD SURVIVES A CONNECTION DROP. The control machine goes through a VPN that
# reconnects every hour or two and breaks all its TCP sessions ("Connection
# closed" in the middle of scp or dd — and the AMI is not built, and the load stand is lost).
# Both steps are idempotent (the file is overwritten, the disk is written from scratch), so
# a drop is a reason to retry, not to give up: up to 10 attempts with a 15 s pause (four
# were not enough for a helper whose sshd came up late: "Connection closed" four times in a
# row cost a whole stand bring-up on 2026-10-22).
retry() { local n; for n in 1 2 3 4 5 6 7 8 9 10; do "$@" && return 0; echo "   attempt $n failed ($1) — retrying in 15 s"; sleep 15; done; return 1; }
retry scp -i "$PEM" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -q "$IMG.gz" ec2-user@"$HIP":/tmp/img.gz
# The disk is found by the volume's SERIAL NUMBER (for EBS NVMe that is the vol-id
# without dashes): the nvme order is not guaranteed, and a mistake here would mean
# writing over the helper's root. The write is done WITHOUT conv=sparse: on an account
# with EBS encryption by default an unwritten block reads as garbage, not as zero.
flash() { $SSH ec2-user@"$HIP" "set -e; VID=\$(echo $VOL | tr -d '-');
  D=\$(lsblk -dno NAME,SERIAL | awk -v v=\"\$VID\" '\$2 ~ \"^\"v {print \"/dev/\"\$1}' | head -1);
  [ -n \"\$D\" ] || { echo 'volume not found by serial number'; lsblk; exit 1; };
  sudo sh -c \"gzip -dc /tmp/img.gz | dd of=\$D bs=1M status=none\"; sync; sudo blockdev --flushbufs \$D;
  echo \"image written to \$D\"" | sed 's/^/   /'; }
retry flash
aws ec2 detach-volume --volume-id "$VOL" >/dev/null
aws ec2 wait volume-available --volume-ids "$VOL"
aws ec2 terminate-instances --instance-ids "$HELPER" >/dev/null; HELPER=""

# ── 5. snapshot + AMI ───────────────────────────────────────────────────────
SNAP=$(aws ec2 create-snapshot --volume-id "$VOL" --description "$NAME" \
  --tag-specifications "ResourceType=snapshot,Tags=[{Key=Project,Value=talaria},{Key=Name,Value=$NAME}]" \
  --query SnapshotId --output text)
echo "== snapshot $SNAP"
aws ec2 wait snapshot-completed --snapshot-ids "$SNAP"
# The same discipline as in cleanup_partial: wait for the detach and do NOT swallow a refusal.
aws ec2 wait volume-available --volume-ids "$VOL" >/dev/null 2>&1 || true
aws ec2 delete-volume --volume-id "$VOL" >/dev/null 2>&1 \
  || echo "WARNING: volume $VOL not deleted — remove it by hand"
VOL=""
# arm64: UEFI only; the Intel VF flag (sriov simple) is x86's — a Graviton has only the ENA.
BOOTMODE=legacy-bios; SRIOV="--sriov-net-support simple"; [ "$ARCH" = arm64 ] && { BOOTMODE=uefi; SRIOV=""; }
AMI=$(aws ec2 register-image --name "$NAME" --architecture "$ARCH" \
  --root-device-name /dev/xvda --boot-mode "$BOOTMODE" --ena-support --virtualization-type hvm \
  $SRIOV --imds-support v2.0 \
  --block-device-mappings "[{\"DeviceName\":\"/dev/xvda\",\"Ebs\":{\"SnapshotId\":\"$SNAP\",\"DeleteOnTermination\":$DOT,\"VolumeType\":\"gp3\"}}]" \
  --query ImageId --output text)
echo "== AMI $AMI (the volume with the instance: DeleteOnTermination=$DOT)"
# THE INTERMEDIATE IMAGES ARE NOT NEEDED AFTER THE AMI IS REGISTERED. Their names carry
# a timestamp and nobody removed them: over one month 220 tlbench-*.img files piled up
# in ~/.talaria-lab/, 150 GB, and the /home partition reached 96 %.
# Everything they held is now in snapshot $SNAP; KEEP_IMG=1 keeps them for local
# debugging (--dry-run does not reach this point and does not touch the image).
[ "${KEEP_IMG:-0}" = 1 ] || rm -f "$IMGDATA" "$IMG" "$IMG.gz"
if [ "$AMI_ONLY" = 1 ]; then
  trap - EXIT
  echo "== --ami-only: not launching an instance"
  echo "AMI_ID=$AMI"
  exit 0
fi

# ── 6. launch (metal holds the vCPU quota for some minutes after terminate) ─
trap - EXIT
IID=""
for a in $(seq 1 10); do
  IID=$(aws ec2 run-instances --image-id "$AMI" --instance-type "$TYPE" --key-name talaria-v11 \
    --security-group-ids "$SG" --subnet-id "$SUBNET" --associate-public-ip-address \
    --placement "AvailabilityZone=$AZ" --tag-specifications "$TAGS" \
    --query 'Instances[0].InstanceId' --output text 2>"$W/runerr.txt") && break
  grep -q VcpuLimitExceeded "$W/runerr.txt" || { cat "$W/runerr.txt"; exit 1; }
  echo "   VcpuLimitExceeded — waiting 60 s (attempt $a)"; sleep 60; IID=""
done
[ -n "$IID" ] || { echo "the instance was not launched"; exit 1; }
aws ec2 wait instance-running --instance-ids "$IID"
IP=$(aws ec2 describe-instances --instance-ids "$IID" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
cat <<EOF

== deployed
   instance : $IID ($TYPE)
   address  : $IP
   AMI      : $AMI (snapshot $SNAP)
   cmdline  : $CMDLINE

Next:
   kernel log (also on disk)    infra/aws/read-disk-trace.sh $IID 2048 2047   (x86, only with --keep-disk)
   console (empty on x86 metal) aws ec2 get-console-output --instance-id $IID --latest --output text
   terminate                    aws ec2 terminate-instances --instance-ids $IID
EOF
