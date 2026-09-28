#!/bin/bash
# metal-ref-probe.sh: a probe "FROM THE REFERENCE" — run a volume KNOWN TO BOOT
# (AL2023) through OUR pipeline (volume from a snapshot -> our own snapshot ->
# our own register-image -> launch) and see what the *.metal firmware says.
#
# Why this way. For three sessions the work went "from us towards the reference":
# our volume was patched until it matched Amazon's byte for byte — and on metal it
# was still silent. The reverse direction is cheaper and halves the search space:
# take the reference BYTES and change exactly one thing — whose pipeline delivered them.
#
# MARKER=chain (default) puts a "talking" marker into sector 0: it prints a
# line (to COM1 AND via int 10h), with HOLD=N keeps it on the screen for N seconds and
# hands control to the ORIGINAL sector 0 (saved at SAVE_LBA=2047).
# MARKER=halt — a marker that prints and HALTS: it DESTROYS the BIOS path, and on
# the reference that is the decisive experiment "which way does metal boot at all".
# MARKER=none — a run without patching sector 0 (a clean control of the pipeline).
#
# WHAT THESE PROBES SHOWED:
#   * the reference bytes boot on metal through OUR pipeline -> the pipeline is sound;
#   * the reference with a broken sector 0 does NOT boot -> metal goes the BIOS way;
#   * our volume is not taken as bootable either with GPT+ESP or without them, and its
#     sector 0 is not executed (a marker with HOLD=8 left nothing in the console);
#   * the bare-metal console is a SCREEN SNAPSHOT of the firmware: raw COM1 is not visible
#     at all, and int 10h is visible only if the line stays on screen for seconds.
#
# IMG=<local image> — the second mode: not the reference but OUR image. An empty
# volume of the needed size is created, the image is dd'ed onto it (the marker, if
# needed, is applied LOCALLY to the file before upload), then the same pipeline
# snapshot -> AMI -> launch. No separate builder instance (lab-up.sh) is needed.
#
# Usage:
#   infra/aws/metal-ref-probe.sh [instance-type]
#   IMG=out.img [DISK_GB=8] infra/aws/metal-ref-probe.sh c6i.metal
#   GRAFT="ours.img:446:64" infra/aws/metal-ref-probe.sh c6i.metal   (bisection)
#   env: AWS_INI, MARKER=chain|halt|none, BOOTMODE=legacy-bios, TRIES=<polls of 10 s>,
#        AMI=<ready AMI> (skip the build and only launch), KEEP=1.
set -euo pipefail
TYPE=${1:-c6i.large}
MARKER=${MARKER:-chain}
# HOLD=<seconds> — the marker keeps the line on screen for N seconds (a dot per BIOS
# tick) before handing control on: on metal the console is filled by the firmware,
# and a line erased by the next step milliseconds later never reaches it.
export HOLD=${HOLD:-0}
# PROOF=<LBA> — the marker leaves an EXECUTION TRACE right on the disk (int 13h AH=43)
# and the root volume is NOT deleted after the run: a helper reads that sector and says
# unambiguously whether sector 0 was executed. It is needed because on metal output from
# sector 0 is visible over NO channel — verified with the reference at HOLD=8.
export PROOF=${PROOF:-}
# READ=<LBA> — the marker READS that sector (the first sector of the gap with diskboot.img)
# the same way boot.img itself reads it (int 13h AH=42), and puts into the trace
# BOTH the read status AND the bytes read: trace 1 = PROOF (our code + status),
# trace 2 = PROOF-1 (what exactly was read). That is how the link
# boot.img -> core.img is checked, which the console on metal does not survive to.
export READ=${READ:-}
# TRACELOG=<LBA> — read the EARLY KERNEL LOG from the volume (talaria.trace=<LBA>): the
# kernel accumulates all its output in memory and flushes it as raw sectors at this LBA
# at every boot phase. On metal this is the only channel showing how far the kernel
# got: the console there shows neither COM1 nor the screen. The root volume, as with
# PROOF, survives the instance (DeleteOnTermination=false).
export TRACELOG=${TRACELOG:-}
# LSPART=<n> — after the run, mount partition n of the volume and show its
# contents: that shows whether the KERNEL ITSELF worked (it writes to the data
# partition), not only the loader.
export LSPART=${LSPART:-}
BOOTMODE=${BOOTMODE:-legacy-bios}
REPO=$(cd "$(dirname "$0")/../.." && pwd)
W=${WORKDIR:-$HOME/.talaria-lab}
AWS_INI=${AWS_INI:-}   # optional: a file "Access key=…/Secret=…"; otherwise the AWS CLI chain (~/.aws, AWS_PROFILE, env)
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
if [ -z "${AWS_ACCESS_KEY_ID:-}" ] && [ -f "$AWS_INI" ]; then
  AWS_ACCESS_KEY_ID=$(sed -n 's/^ *Access key *= *//Ip' "$AWS_INI" | tr -d '\r')
  AWS_SECRET_ACCESS_KEY=$(sed -n 's/^ *Secret[ a-z]* *= *//Ip' "$AWS_INI" | tr -d '\r')
  export AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
fi
mkdir -p "$W"
TS=$(date +%H%M%S)
LOG="$W/metalref-$TS.log"

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

HAVE_KEY=$(aws ec2 describe-key-pairs --key-names talaria-v11 --query 'KeyPairs[0].KeyName' --output text 2>/dev/null || echo None)
if [ ! -f "$W/talaria-v11.pem" ] || [ "$HAVE_KEY" != talaria-v11 ]; then
  aws ec2 delete-key-pair --key-name talaria-v11 >/dev/null 2>&1 || true
  rm -f "$W/talaria-v11.pem"
  aws ec2 create-key-pair --key-name talaria-v11 --query KeyMaterial --output text > "$W/talaria-v11.pem"
  chmod 600 "$W/talaria-v11.pem"
fi
SSH="ssh -i $W/talaria-v11.pem -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10"

HELPER=""; VOL=""
# The volume WITH THE RUN LOG may be deleted only after the log has been read.
# A metal run costing 40 minutes and ~$4 was lost exactly like that: the read failed over
# ssh (the helper refused), and the exit handler removed the volume together with the only
# copy of the result. The flag is raised BEFORE the read and cleared only once the log is ours.
READ_PENDING=0
cleanup() {
  if [ "$READ_PENDING" = 1 ] && [ -n "$VOL" ]; then
    echo "!! the run log was NOT read — volume $VOL LEFT IN PLACE"
    echo "   read it again:  infra/aws/read-disk-trace.sh $VOL ${TRACELOG:-2048} ${PROOF:-2047}"
    echo "   delete later:   aws ec2 delete-volume --volume-id $VOL"
    [ -n "$HELPER" ] && [ "${KEEP:-0}" != 1 ] && aws ec2 terminate-instances --instance-ids "$HELPER" >/dev/null 2>&1
    return 0
  fi
  [ "${KEEP:-0}" = 1 ] && return 0
  [ -n "$VOL" ] && { aws ec2 detach-volume --volume-id "$VOL" >/dev/null 2>&1 || true; sleep 8;
                     aws ec2 delete-volume --volume-id "$VOL" >/dev/null 2>&1 || true; }
  [ -n "$HELPER" ] && aws ec2 terminate-instances --instance-ids "$HELPER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# ── 1. an AMI from the reference bytes or from OUR image (or a ready one from AMI=) ─
if [ -z "${AMI:-}" ]; then
  if [ -n "${IMG:-}" ]; then
    [ -f "$IMG" ] || { echo "no image $IMG"; exit 1; }
    DISK_GB=${DISK_GB:-$(( ($(stat -c%s "$IMG") + 1073741823) / 1073741824 ))}
    echo "== our image: $IMG (volume ${DISK_GB} GiB), marker=$MARKER, boot-mode=$BOOTMODE"
    VOL=$(aws ec2 create-volume --size "$DISK_GB" --availability-zone "$AZ" --volume-type gp3 \
      --tag-specifications 'ResourceType=volume,Tags=[{Key=Project,Value=talaria-v11}]' \
      --query VolumeId --output text)
  else
    REF_AMI=${REF_AMI:-$(aws ssm get-parameters \
      --names /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64 \
      --query 'Parameters[0].Value' --output text)}
    REF_SNAP=$(aws ec2 describe-images --image-ids "$REF_AMI" \
      --query 'Images[0].BlockDeviceMappings[0].Ebs.SnapshotId' --output text)
    echo "== reference: $REF_AMI (snapshot $REF_SNAP), marker=$MARKER, boot-mode=$BOOTMODE"
    VOL=$(aws ec2 create-volume --snapshot-id "$REF_SNAP" --availability-zone "$AZ" --volume-type gp3 \
      --tag-specifications 'ResourceType=volume,Tags=[{Key=Project,Value=talaria-v11}]' \
      --query VolumeId --output text)
  fi
  echo "volume $VOL" >> "$W/resources.txt"
  aws ec2 wait volume-available --volume-ids "$VOL"

  if [ -n "${IMG:-}" ] || [ -n "${GRAFT:-}" ] || [ "$MARKER" != none ]; then
    HELPER_AMI=$(aws ssm get-parameters --names /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64 \
      --query 'Parameters[0].Value' --output text)
    HELPER=$(aws ec2 run-instances --image-id "$HELPER_AMI" --instance-type t3.small --key-name talaria-v11 \
      --security-group-ids "$SG" --subnet-id "$SUBNET" --associate-public-ip-address \
      --placement "AvailabilityZone=$AZ" \
      --tag-specifications 'ResourceType=instance,Tags=[{Key=Project,Value=talaria-v11},{Key=Name,Value=talaria-refprobe}]' \
      --query 'Instances[0].InstanceId' --output text)
    echo "instance $HELPER" >> "$W/resources.txt"
    aws ec2 wait instance-running --instance-ids "$HELPER"
    HIP=$(aws ec2 describe-instances --instance-ids "$HELPER" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
    aws ec2 attach-volume --volume-id "$VOL" --instance-id "$HELPER" --device /dev/sdg >/dev/null
    aws ec2 wait volume-in-use --volume-ids "$VOL"
    echo "== helper $HELPER ($HIP), volume $VOL attached"
    # A fresh t3.small accepts ssh neither immediately nor reliably: the first probe may
    # pass while the very next command breaks off ("Connection closed").
    # We wait not for the first success but for THREE in a row, and for longer.
    OKN=0
    for i in $(seq 1 90); do
      if $SSH ec2-user@"$HIP" true 2>/dev/null; then OKN=$((OKN + 1)); [ "$OKN" -ge 3 ] && break; else OKN=0; fi
      sleep 5
    done
    [ "$OKN" -ge 3 ] || echo "!! helper $HELPER ($HIP) does not accept ssh — trying anyway"

    # The disk is found by the volume's SERIAL NUMBER (for EBS NVMe that is the vol-id
    # without dashes): the nvme order is not guaranteed, and a mistake here would mean
    # patching the helper's own root. lsblk is always there, nvme-cli is not (a pitfall:
    # on a clean AL2023 `nvme id-ctrl` may be missing, and the search silently found nothing).
    FINDDISK="VID=\$(echo $VOL | tr -d '-');
      D=\$(lsblk -dno NAME,SERIAL | awk -v v=\"\$VID\" '\$2 ~ \"^\"v {print \"/dev/\"\$1}' | head -1);
      [ -n \"\$D\" ] || D=\$(for d in /dev/nvme*n1; do s=\$(sudo nvme id-ctrl \$d 2>/dev/null | sed -n 's/^sn *: *//p' | tr -d ' '); case \"\$s\" in \$VID*) echo \$d;; esac; done | head -1);
      [ -n \"\$D\" ] || { echo 'volume not found by serial number; lsblk:'; lsblk -o NAME,SIZE,SERIAL,MOUNTPOINTS; exit 1; };
      echo \"DISK=\$D\";"

    if [ -n "${GRAFT:-}" ]; then
      # ── BISECTION: graft OUR slice onto the REFERENCE volume ───────────────
      # GRAFT="<our image>:<offset in bytes>:<length>[,...]" — take a piece from our
      # image and put it at the same place on the reference volume, leaving everything
      # else Amazon's. Exactly one thing changes per run:
      #   446:64     — the partition table in sector 0 (sector 0 code stays Amazon's);
      #   512:16896  — the primary GPT (sectors 1..33);
      #   1024:...   — the gap with core.img.
      # The first slice after which the reference stops booting is the difference we
      # are looking for. The method works regardless of which hypothesis is right.
      I=0
      for spec in $(echo "$GRAFT" | tr ',' ' '); do
        GIMG=${spec%%:*}; rest=${spec#*:}; GOFF=${rest%%:*}; GLEN=${rest##*:}
        [ -f "$GIMG" ] || { echo "no image to graft: $GIMG"; exit 1; }
        I=$((I + 1))
        dd if="$GIMG" of="$W/graft$I.bin" bs=1 skip="$GOFF" count="$GLEN" status=none
        echo "== graft $I: $GIMG [$GOFF, +$GLEN) -> the same address on the reference volume"
        od -An -tx1 -N32 "$W/graft$I.bin" | head -2
        scp -i "$W/talaria-v11.pem" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -q \
          "$W/graft$I.bin" ec2-user@"$HIP":/tmp/
        $SSH ec2-user@"$HIP" "set -e; $FINDDISK
          SIG=\$(sudo dd if=\$D bs=512 count=1 status=none | od -An -tx1 -j510 -N2 | tr -d ' \\n');
          [ \"\$SIG\" = 55aa ] || { echo \"no 55AA in sector 0 (\$SIG) — wrong disk\"; exit 1; };
          echo '--- before the graft:'; sudo dd if=\$D bs=1 skip=$GOFF count=$GLEN status=none | od -An -tx1 -N32 | head -2;
          sudo dd if=/tmp/graft$I.bin of=\$D bs=1 seek=$GOFF count=$GLEN conv=notrunc status=none;
          sync; sudo blockdev --flushbufs \$D;
          echo '--- after the graft:'; sudo dd if=\$D bs=1 skip=$GOFF count=$GLEN status=none | od -An -tx1 -N32 | head -2" | tee -a "$LOG"
      done
    elif [ -n "${IMG:-}" ]; then
      # The marker is applied LOCALLY to a copy of the image — the helper has no as/ld.
      cp "$IMG" "$W/probe.img"
      MOUT=""
      if [ "$MARKER" != none ]; then
        MOUT=$(CHAIN=1 TEXT=${TEXT:-TALARIA-MBR-CHAIN-OK} "$REPO/tools/mbr-marker.sh" "$W/probe.img")
        printf '%s\n' "$MOUT" | tee -a "$LOG"
      fi
      # The field offsets in the trace are taken FROM THE TOOL ITSELF, not as constants:
      # a previous run was wasted precisely because an offset had been written by hand
      # and went stale after the marker was edited.
      OFF_ST2=$(printf '%s' "$MOUT" | sed -n 's/.*offset st2 *= *\([0-9]*\).*/\1/p')
      OFF_REM=$(printf '%s' "$MOUT" | sed -n 's/.*offset remain *= *\([0-9]*\).*/\1/p')
        gzip -cf "$W/probe.img" > "$W/probe.img.gz"
      # scp with retries: a drop on a fresh helper broke the whole run through set -e.
      SCP_OK=0
      for a in 1 2 3 4 5; do
        scp -i "$W/talaria-v11.pem" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -q \
          "$W/probe.img.gz" ec2-user@"$HIP":/tmp/ && { SCP_OK=1; break; }
        echo "   scp attempt $a failed — waiting 15 s"; sleep 15
      done
      [ "$SCP_OK" = 1 ] || { echo "the image was not uploaded to the helper"; exit 1; }
      # WITHOUT conv=sparse. `conv=sparse` was once added here to avoid writing
      # zero megabytes: "a fresh EBS volume is zeroed anyway".
      # THAT IS WRONG on an account with EBS encryption by default: unwritten
      # blocks read not as zeros but as PSEUDORANDOM garbage (verified directly:
      # a freshly created volume returns random bytes at LBA 1024). The skipped
      # zero areas of the image stayed garbage on the volume — and that is exactly
      # where both traces land: the loader sector (2047) and the kernel log (talaria.trace).
      # Three runs were read as "the trace was not written", while there was nowhere
      # to write: the measurement channel was poisoned before the experiment began.
      # The price of dropping sparse is writing the image itself in full (tens of MB, seconds).
      $SSH ec2-user@"$HIP" "set -e; $FINDDISK
        sudo sh -c \"gzip -dc /tmp/probe.img.gz | dd of=\$D bs=1M status=none\"; sync; sudo blockdev --flushbufs \$D;
        echo '--- sector 0 on the volume:'; sudo dd if=\$D bs=512 count=1 status=none | hexdump -C | head -3;
        # CHANNEL CONTROL: the trace sectors must be ZERO before the run.
        # If they are not zero, there is nothing to measure, and that must be known BEFORE launch.
        RC=0;
        for s in ${TRACELOG:-1024} ${PROOF:-2047}; do
          NZ=\$(sudo dd if=\$D bs=512 skip=\$s count=1 status=none | tr -d '\\000' | wc -c);
          echo \"control: sector \$s — non-zero bytes \$NZ (expecting 0)\";
          [ \"\$NZ\" = 0 ] || RC=1;
        done;
        # The control must STOP the run, not merely print a number.
        # The very first suite run on EC2 caught exactly this: an image run locally
        # carries with it BOTH its logs AND the suite counter (the list is already
        # done) — on the instance it would silently run empty, while we read
        # SOMEONE ELSE'S local logs as the result of a run on hardware.
        [ \$RC = 0 ] || [ \"${FORCE:-0}\" = 1 ] || { echo 'CONTROL FAILED: the traces on the volume are NOT zero (has the image already been run? rebuild it). FORCE=1 — continue anyway'; exit 3; };
        true" | tee -a "$LOG"
    # The status is TAKEN FROM SSH, not from tee: through a pipeline `set -e` sees only
    # tee's success, and a failed control would have gone on to the instance.
    [ "${PIPESTATUS[0]}" = 0 ] || { echo "the volume control failed — the run is cancelled"; exit 3; }
    else
      # The marker code is assembled LOCALLY (as/ld exist on the host, not on the helper).
      # MARKER=halt — a marker that prints and HALTS, i.e. DESTROYS the volume's
      # BIOS path. On the reference that is the decisive experiment: if the volume
      # booted anyway, the firmware does NOT go through sector 0 (the UEFI path via the ESP),
      # and all the diagnostics around the MBR are beside the point. If it halted — the path is BIOS.
      CH=1; [ "$MARKER" = halt ] && CH=0
      CHAIN=$CH EMIT="$W/mbr-code.bin" TEXT=${TEXT:-TALARIA-MBR-MARKER-OK} "$REPO/tools/mbr-marker.sh" >/dev/null
      scp -i "$W/talaria-v11.pem" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -q \
        "$W/mbr-code.bin" ec2-user@"$HIP":/tmp/
      SZ=$(stat -c%s "$W/mbr-code.bin")
      SAVE=""; [ "$CH" = 1 ] && SAVE="sudo dd if=/tmp/orig0.bin of=\$D bs=512 seek=2047 count=1 conv=notrunc status=none;"
      $SSH ec2-user@"$HIP" "set -e; $FINDDISK
        sudo dd if=\$D of=/tmp/orig0.bin bs=512 count=1 status=none;
        SIG=\$(od -An -tx1 -j510 -N2 /tmp/orig0.bin | tr -d ' \\n');
        [ \"\$SIG\" = 55aa ] || { echo \"no 55AA in sector 0 (\$SIG) — wrong disk\"; exit 1; };
        $SAVE
        sudo dd if=/tmp/mbr-code.bin of=\$D bs=1 count=$SZ conv=notrunc status=none;
        sync; sudo blockdev --flushbufs \$D;
        echo '--- sector 0 after the patch:'; sudo dd if=\$D bs=512 count=1 status=none | hexdump -C | head -3;
        echo '--- LBA 2047 (the saved original):'; sudo dd if=\$D bs=512 skip=2047 count=1 status=none | hexdump -C | head -2" | tee -a "$LOG"
    fi

    aws ec2 detach-volume --volume-id "$VOL" >/dev/null
    aws ec2 wait volume-available --volume-ids "$VOL"
    aws ec2 terminate-instances --instance-ids "$HELPER" >/dev/null; HELPER=""
  fi

  SNAP=$(aws ec2 create-snapshot --volume-id "$VOL" --description "talaria-refprobe $TS" \
    --tag-specifications 'ResourceType=snapshot,Tags=[{Key=Project,Value=talaria-v11}]' --query SnapshotId --output text)
  echo "snapshot $SNAP" >> "$W/resources.txt"
  echo "== snapshot $SNAP"
  aws ec2 wait snapshot-completed --snapshot-ids "$SNAP"
  aws ec2 delete-volume --volume-id "$VOL" >/dev/null 2>&1 || true; VOL=""
  # We register it AS CLOSE AS POSSIBLE to the reference AMI (except boot-mode: the
  # reference is uefi-preferred, but on metal that reduces to legacy-bios anyway —
  # verified). The root device name on AL2023 is /dev/xvda.
  DOT=true                                     # with PROOF/TRACELOG the volume must survive the instance
  { [ -n "${PROOF:-}" ] || [ -n "${TRACELOG:-}" ]; } && DOT=false
  AMI=$(aws ec2 register-image --name "talaria-refprobe-$TS" --architecture x86_64 \
    --root-device-name /dev/xvda --boot-mode "$BOOTMODE" --ena-support --virtualization-type hvm \
    --sriov-net-support simple --imds-support v2.0 \
    --block-device-mappings "[{\"DeviceName\":\"/dev/xvda\",\"Ebs\":{\"SnapshotId\":\"$SNAP\",\"DeleteOnTermination\":$DOT,\"VolumeType\":\"gp3\"}}]" \
    --query ImageId --output text)
  echo "ami $AMI" >> "$W/resources.txt"
fi
echo "AMI: $AMI"

# ── 2. launch (metal holds the vCPU quota for some minutes after terminate) ──
echo "== run-instances ($TYPE)"
IID=""
for a in 1 2 3 4 5 6 7 8 9 10; do
  IID=$(aws ec2 run-instances --image-id "$AMI" --instance-type "$TYPE" --key-name talaria-v11 \
    --security-group-ids "$SG" --subnet-id "$SUBNET" --associate-public-ip-address \
    --placement "AvailabilityZone=$AZ" \
    --tag-specifications 'ResourceType=instance,Tags=[{Key=Project,Value=talaria-v11},{Key=Name,Value=talaria-refprobe-node}]' \
    --query 'Instances[0].InstanceId' --output text 2>"$W/runerr.txt") && break
  grep -q VcpuLimitExceeded "$W/runerr.txt" || { cat "$W/runerr.txt"; exit 1; }
  echo "   VcpuLimitExceeded — waiting 60 s (attempt $a)"; sleep 60; IID=""
done
[ -n "$IID" ] || { echo "the instance was not launched"; exit 1; }
echo "instance $IID" >> "$W/resources.txt"
echo "TALARIA_IID=$IID"

console_of() {
  raw=$(aws ec2 get-console-output --instance-id "$1" --latest --output text --query Output 2>/dev/null || true)
  [ -z "$raw" ] && return 0
  dec=$(printf '%s' "$raw" | base64 -d 2>/dev/null || true)
  case "$dec" in *TALARIA*|*Booting*|*Linux*) printf '%s' "$dec"; return 0;; esac
  printf '%s' "$raw"
}
TRIES=${TRIES:-96}
echo "== serial console (up to $((TRIES * 10 / 60)) min)"
BM=""
for i in $(seq 1 "$TRIES"); do
  sleep 10
  # The kernel, having finished the suite, POWERS THE MACHINE OFF (ACPI S5) — the instance
  # goes to stopped. That is the only unambiguous "it reached the end" signal on a machine
  # without a console: before, "the suite passed" and "it hung on the third item" differed
  # only by the clock, and the waiting was billed at $6 an hour.
  ST=$(aws ec2 describe-instances --instance-ids "$IID" \
      --query 'Reservations[0].Instances[0].State.Name' --output text 2>/dev/null || true)
  case "$ST" in
    stopping|stopped) echo "   [$i] instance $ST — THE KERNEL POWERED THE MACHINE OFF ITSELF (the suite is done)"; break;;
  esac
  [ -z "$BM" ] && BM=$(aws ec2 describe-instances --instance-ids "$IID" \
      --query 'Reservations[0].Instances[0].CurrentInstanceBootMode' --output text 2>/dev/null || true)
  OUT=$(console_of "$IID")
  [ -z "$OUT" ] && continue
  printf '%s\n' "$OUT" > "$W/console-refprobe-$TS.log"
  N=$(printf '%s' "$OUT" | wc -c)
  echo "   [$i] console $N bytes${BM:+, CurrentInstanceBootMode=$BM}"
  # TBOOT/talaria is the output of OUR chain: stage2 prints a line through the
  # BIOS teletype (int 10h), the kernel writes straight into the text screen buffer. If
  # they appeared, the "firmware screen" channel on metal works, and there is nothing more
  # to wait for: by then the kernel has already received control.
  # In SUITE mode we must wait not for the kernel's first line but for the end of the list:
  # the kernel powers the machine off itself (the stopping|stopped check above). The old
  # exit on the first line answered a different question — "did the kernel get control at
  # all" — and in a suite it killed the instance in the middle of the first item.
  [ -n "${SLOTS:-}" ] && continue
  printf '%s' "$OUT" | grep -aqE 'TALARIA-MBR-CHAIN|TBOOT|talaria|Booting|login:' && break
done
{
  echo "=== result: type $TYPE, AMI $AMI, boot-mode $BOOTMODE (actual ${BM:-?}), marker $MARKER"
  # An empty console is normal (on metal the guest's output is not visible at all), and the
  # run must NOT be aborted over it: what follows is reading the trace from the volume, which
  # is what all of this is for. An earlier version died here through set -e because the
  # console file simply did not exist, and the instance was left running.
  if [ -f "$W/console-refprobe-$TS.log" ]; then
    echo "--- console ($(wc -c < "$W/console-refprobe-$TS.log") bytes):"
    head -60 "$W/console-refprobe-$TS.log" || true
  else
    echo "--- console: empty (not a byte in $((TRIES * 10 / 60)) min)"
  fi
} | tee -a "$LOG"
echo "log: $LOG   console: $W/console-refprobe-$TS.log"

# ── 3. READING THE TRACE FROM DISK (PROOF): the only signal that does not depend on
# whether the guest's output is visible on metal. The instance is terminated, the root
# volume survives it (DeleteOnTermination=false), is attached to a helper, and the PROOF sector is read.
if [ -n "${PROOF:-}" ] || [ -n "${TRACELOG:-}" ]; then
  RVOL=$(aws ec2 describe-instances --instance-ids "$IID" \
    --query 'Reservations[0].Instances[0].BlockDeviceMappings[0].Ebs.VolumeId' --output text)
  echo "== looking for the trace on root volume $RVOL, sector ${PROOF:-—}, kernel log ${TRACELOG:-—}"
  aws ec2 terminate-instances --instance-ids "$IID" >/dev/null
  # Metal takes LONGER to stop than `aws ec2 wait` tolerates (40 attempts of 15 s):
  # the wait fell over with "Max attempts exceeded", the script died through
  # set -e, and the volume with the trace already taken was left running. We wait with our own loop.
  for _ in $(seq 1 80); do
    st=$(aws ec2 describe-volumes --volume-ids "$RVOL" --query 'Volumes[0].State' --output text 2>/dev/null || echo unknown)
    [ "$st" = available ] && break
    sleep 15
  done
  echo "   root volume $RVOL: $st"
  HELPER=$(aws ec2 run-instances --image-id "$(aws ssm get-parameters \
      --names /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64 \
      --query 'Parameters[0].Value' --output text)" \
    --instance-type t3.small --key-name talaria-v11 --security-group-ids "$SG" --subnet-id "$SUBNET" \
    --associate-public-ip-address --placement "AvailabilityZone=$AZ" \
    --tag-specifications 'ResourceType=instance,Tags=[{Key=Project,Value=talaria-v11},{Key=Name,Value=talaria-proofread}]' \
    --query 'Instances[0].InstanceId' --output text)
  echo "instance $HELPER" >> "$W/resources.txt"
  aws ec2 wait instance-running --instance-ids "$HELPER"
  HIP=$(aws ec2 describe-instances --instance-ids "$HELPER" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
  VOL="$RVOL"
  READ_PENDING=1                    # from now on the volume is worth more than its price
  aws ec2 attach-volume --volume-id "$RVOL" --instance-id "$HELPER" --device /dev/sdg >/dev/null
  aws ec2 wait volume-in-use --volume-ids "$RVOL"
  # We wait for ssh LONGER and say so if it never came: 200 seconds was not always enough, and
  # silently going on means a lost run.
  SSH_OK=0; OKN=0
  for i in $(seq 1 90); do
    if $SSH ec2-user@"$HIP" true 2>/dev/null; then OKN=$((OKN + 1)); [ "$OKN" -ge 3 ] && { SSH_OK=1; break; }; else OKN=0; fi
    sleep 5
  done
  [ "$SSH_OK" = 1 ] || echo "!! helper $HELPER ($HIP) did not accept ssh in 7.5 min — the volume will be kept"

  {
    echo "--- the volume after the run (loader trace: sector ${PROOF:-—}, kernel log: LBA ${TRACELOG:-—})"
    $SSH ec2-user@"$HIP" "set -e; VID=\$(echo $RVOL | tr -d '-');
      D=\$(lsblk -dno NAME,SERIAL | awk -v v=\"\$VID\" '\$2 ~ \"^\"v {print \"/dev/\"\$1}' | head -1);
      [ -n \"\$D\" ] || { echo 'volume not found'; exit 1; };
      if [ -n \"${PROOF:-}\" ]; then
      sudo dd if=\$D bs=512 skip=${PROOF:-0} count=1 status=none > /tmp/proof.bin;
      hexdump -C /tmp/proof.bin | head -3;
      if grep -aqE 'TALARIA|TBOOT' /tmp/proof.bin; then echo 'TRACE PRESENT: sector 0 WAS EXECUTED';
        echo '--- phase / int 13h status / sectors remaining (the tboot trace):';
        od -An -tu1 -j11 -N1 /tmp/proof.bin; od -An -tx1 -j12 -N2 /tmp/proof.bin; od -An -tu4 -j14 -N4 /tmp/proof.bin;
      else echo 'NO TRACE: sector 0 was not executed (or the int 13h write did not go through)'; fi;
      echo '--- the machine MEMORY MAP: MBI tag 6, as the loader built it (sectors trace-8..trace-1)';
      echo '    entries of 24 bytes: base(8) length(8) type(4) attributes(4), type 1 = RAM';
      sudo dd if=\$D bs=512 skip=\$(( ${PROOF:-0} - 8 )) count=8 status=none > /tmp/mbi.bin;
      hexdump -C -s 288 -n 1024 /tmp/mbi.bin;
      fi;
      if [ -n \"${SLOTS:-}\" ]; then
        echo '--- MODE SUITE: state (sector 1004) — magic, done, total';
        sudo dd if=\$D bs=512 skip=1004 count=1 status=none > /tmp/st.bin;
        head -c 13 /tmp/st.bin; echo; od -An -tu4 -j16 -N8 /tmp/st.bin;
        for i in \$(seq 0 ${SLOTS:-1}); do
          OFF=\$(( ${TRACELOG:-0} + i * 129 ));
          echo \"=== SLOT \$i (LBA \$OFF)\";
          sudo dd if=\$D bs=512 skip=\$OFF count=1 status=none | tr -d '\\000' | head -1;
          sudo dd if=\$D bs=512 skip=\$(( OFF + 1 )) count=128 status=none | tr -d '\\000' > /tmp/s.txt;
          cat /tmp/s.txt;
        done;
      fi;
      if [ -n \"${TRACELOG:-}\" ] && [ -z \"${SLOTS:-}\" ]; then
        echo '--- EARLY KERNEL LOG (talaria.trace): header, raw bytes';
        sudo dd if=\$D bs=512 skip=${TRACELOG:-0} count=1 status=none | hexdump -C | head -4;
        echo '--- the same as text:';
        sudo dd if=\$D bs=512 skip=${TRACELOG:-0} count=1 status=none | tr -d '\\000' | head -2;
        echo '--- the first sector of the log itself, raw bytes:';
        sudo dd if=\$D bs=512 skip=\$(( ${TRACELOG:-0} + 1 )) count=1 status=none | hexdump -C | head -4;
        echo '--- the log itself (the last lines are where the kernel stopped):';
        sudo dd if=\$D bs=512 skip=\$(( ${TRACELOG:-0} + 1 )) count=128 status=none | tr -d '\\000' > /tmp/klog.txt;
        wc -c /tmp/klog.txt; cat /tmp/klog.txt;
      fi;
      if [ -n \"${LSPART:-}\" ]; then
        echo '--- partition ${LSPART:-} after the run (traces of the KERNEL ITSELF at work):';
        sudo mount -o ro \${D}p${LSPART:-2} /mnt 2>&1 | head -2;
        sudo find /mnt -maxdepth 3 -printf '%10s  %p\\n' 2>/dev/null | head -20;
        sudo umount /mnt 2>/dev/null || true;
      fi;
      if [ -n \"${READ:-}\" ]; then
        echo '--- the status of the last int 13h (st2; the high byte is the error code, 00 = success):';
        od -An -tx1 -j${OFF_ST2:-0} -N2 /tmp/proof.bin;
        echo '--- remain (how many sectors REMAINED unread; 0000 = everything was read):';
        od -An -tx1 -j${OFF_REM:-0} -N2 /tmp/proof.bin;
        echo '--- trace 2 (sector $(( PROOF - 1 )) = the first tail sector, as GRUB would have loaded it):';
        sudo dd if=\$D bs=512 skip=$(( PROOF - 1 )) count=1 status=none > /tmp/proof2.bin;
        hexdump -C /tmp/proof2.bin | head -2;
        echo '--- REFERENCE: the real LBA ${READ%%:*} on this same volume:';
        sudo dd if=\$D bs=512 skip=${READ%%:*} count=1 status=none > /tmp/real.bin;
        hexdump -C /tmp/real.bin | head -2;
        if cmp -s /tmp/proof2.bin /tmp/real.bin; then echo 'MATCHES: the core.img tail is read correctly';
        else echo 'DOES NOT MATCH: something else was read'; fi;
      fi"
  } | tee -a "$LOG"
  # The log is ours — only now may the volume be touched.
  if [ "$SSH_OK" = 1 ]; then READ_PENDING=0; fi
  [ "$READ_PENDING" = 1 ] && { echo "!! the read failed — volume $RVOL kept deliberately"; exit 4; }
  [ "${KEEP:-0}" = 1 ] || { aws ec2 terminate-instances --instance-ids "$HELPER" >/dev/null; HELPER="";
                            aws ec2 detach-volume --volume-id "$RVOL" >/dev/null 2>&1 || true;
                            aws ec2 wait volume-available --volume-ids "$RVOL" 2>/dev/null || true;
                            aws ec2 delete-volume --volume-id "$RVOL" >/dev/null 2>&1 || true; VOL=""; }
else
  [ "${KEEP:-0}" = 1 ] || { echo "== terminate $IID"; aws ec2 terminate-instances --instance-ids "$IID" >/dev/null; }
fi
