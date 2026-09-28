#!/bin/bash
# ena-devinit-trace.sh: capture the DEVICE-INIT SEQUENCE of the live Linux ENA
# driver.
#
# Why a separate technique: a kprobe on a MODULE function goes away together with
# the module unload, so `modprobe -r ena; modprobe ena` under bpftrace catches
# only DESTROY_*. Here the module stays loaded, and the device is re-initialised
# through a PCI hot-remove + rescan: the driver's probe() runs again IN FULL,
# and the kprobe sees it.
#
# The instance is disposable: the whole script goes into user-data, runs as root
# at start, and the result is printed TO THE SERIAL CONSOLE — visible through
# get-console-output even if the network does not come back after the rescan.
#
# Usage: ENVF=~/.talaria-lab/lab.env infra/aws/ena-devinit-trace.sh [type]
set -euo pipefail
TYPE=${1:-c6i.large}
ENVF=${ENVF:?no ENVF}; . "$ENVF"
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
TS=$(date +%H%M%S)
W=$(dirname "$ENVF")
AMI=$(aws ssm get-parameters --names /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64 \
  --query 'Parameters[0].Value' --output text)

UD=$(cat <<'EOS'
#!/bin/bash
exec > >(tee /dev/console) 2>&1
echo "== devinit-trace: start"
dnf install -y -q bpftrace >/dev/null 2>&1 || dnf install -y -q bcc-tools bpftrace >/dev/null 2>&1
IF=$(ls /sys/class/net | grep -v lo | head -1)
BDF=$(basename "$(readlink -f /sys/class/net/$IF/device)")
echo "== interface $IF, PCI $BDF, driver $(basename $(readlink -f /sys/class/net/$IF/device/driver))"
cat > /tmp/tr.bt <<'EOB'
kprobe:ena_com_execute_admin_command
{
  printf("AQ op=%d size=%d %r\n", *(uint8*)arg1, (uint64)arg2, buf(arg1, 64));
}
EOB
timeout 70 bpftrace /tmp/tr.bt > /tmp/aq.txt 2>/tmp/aq.err &
BP=$!
sleep 15
echo "== hot-remove $BDF (the network goes away now)"
echo 1 > /sys/bus/pci/devices/$BDF/remove
sleep 3
echo 1 > /sys/bus/pci/rescan
sleep 25
wait $BP 2>/dev/null || true
echo "== bpftrace stderr:"; tail -5 /tmp/aq.err
echo "== device-init admin commands (after rescan):"
cat /tmp/aq.txt
echo "== devinit-trace: END"
EOS
)

echo "== disposable Linux instance $TYPE from $AMI"
IID=$(aws ec2 run-instances --image-id "$AMI" --instance-type "$TYPE" --key-name talaria-v11 \
  --security-group-ids "$SG" --subnet-id "$SUBNET" --associate-public-ip-address \
  --user-data "$UD" \
  --tag-specifications "ResourceType=instance,Tags=[{Key=Project,Value=talaria-v11},{Key=Name,Value=ena-devinit-$TS}]" \
  --query 'Instances[0].InstanceId' --output text)
echo "instance $IID" >> "$W/resources.txt"
trap '[ "${KEEP:-0}" = 1 ] || aws ec2 terminate-instances --instance-ids "$IID" >/dev/null 2>&1 || true' EXIT
aws ec2 wait instance-running --instance-ids "$IID"
LOG="$W/devinit-$TS.log"
for i in $(seq 1 30); do
  sleep 20
  raw=$(aws ec2 get-console-output --instance-id "$IID" --latest --output text --query Output 2>/dev/null || true)
  dec=$(printf '%s' "$raw" | base64 -d 2>/dev/null || true)
  case "$dec" in *devinit-trace*) printf '%s' "$dec" > "$LOG";; *) printf '%s' "$raw" > "$LOG";; esac
  grep -qa "devinit-trace: END" "$LOG" && break
done
echo "== log: $LOG"
sed -n '/devinit-trace: start/,$p' "$LOG" | tail -60
