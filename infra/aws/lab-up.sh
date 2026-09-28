#!/bin/bash
# lab-up.sh: bring up a minimal stand for iterating on Talaria on EC2.
#
# What it creates (all tagged Project=talaria-v11 — lab-down.sh tears it down by that tag):
#   - the key pair talaria-v11 (.pem in the work directory),
#   - a security group: SSH from your IP + ALL traffic within the group (RTT measurements),
#   - builder — a small Amazon Linux with an empty /dev/sdf volume: the Talaria image is
#     dd'ed onto it (ec2-boot-talaria.sh snapshots the volume -> AMI),
#   - peer — a Linux neighbour of the same type as the instance under test: RTT is
#     measured from it (udp_ping/tcp_ping are built on it) and it is also the Linux baseline.
# Both instances are in one subnet and one cluster placement group, otherwise the RTT
# numbers measure the distance between racks, not the stack.
#
# Usage:
#   infra/aws/lab-up.sh [peer-type] [work-dir]   # credentials as for the AWS CLI
# Then:
#   . <work-dir>/lab.env && ENVF=<work-dir>/lab.env PEM=<work-dir>/talaria-v11.pem \
#     infra/aws/ec2-boot-talaria.sh "console=ttyS0 talaria.mode=userver ..." c6i.large
set -euo pipefail
PEER_TYPE=${1:-c6i.large}
W=${2:-$HOME/.talaria-lab}
REPO=$(cd "$(dirname "$0")/../.." && pwd)
AWS_INI=${AWS_INI:-}   # optional: a file "Access key=…/Secret=…"; otherwise the AWS CLI chain (~/.aws, AWS_PROFILE, env)
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
mkdir -p "$W"

# credentials: the AWS CLI chain or AWS_INI — a file like "Access key=...\nSecret=..."
if [ -z "${AWS_ACCESS_KEY_ID:-}" ] && [ -n "$AWS_INI" ] && [ -f "$AWS_INI" ]; then
  AWS_ACCESS_KEY_ID=$(sed -n 's/^ *Access key *= *//Ip' "$AWS_INI" | tr -d '\r')
  AWS_SECRET_ACCESS_KEY=$(sed -n 's/^ *Secret[ a-z]* *= *//Ip' "$AWS_INI" | tr -d '\r')
  export AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
fi
aws sts get-caller-identity >/dev/null || { echo "no AWS credentials (~/.aws, AWS_PROFILE, env or AWS_INI=<file>)"; exit 1; }
TAG='ResourceType=instance,Tags=[{Key=Project,Value=talaria-v11}]'

echo "== region $AWS_DEFAULT_REGION, account $(aws sts get-caller-identity --query Account --output text)"

# ── key ──────────────────────────────────────────────────────────────────────
# The key is recreated if it is missing EITHER in AWS or locally (otherwise run-instances
# fails with InvalidKeyPair.NotFound while a .pem lies around).
HAVE_KEY=$(aws ec2 describe-key-pairs --key-names talaria-v11 --query 'KeyPairs[0].KeyName' --output text 2>/dev/null || echo None)
if [ ! -f "$W/talaria-v11.pem" ] || [ "$HAVE_KEY" != talaria-v11 ]; then
  aws ec2 delete-key-pair --key-name talaria-v11 >/dev/null 2>&1 || true
  rm -f "$W/talaria-v11.pem"
  aws ec2 create-key-pair --key-name talaria-v11 --query KeyMaterial --output text > "$W/talaria-v11.pem"
  chmod 600 "$W/talaria-v11.pem"
  echo "key: $W/talaria-v11.pem"
fi

# ── network: the default VPC, the first subnet, the SG ───────────────────────
VPC=$(aws ec2 describe-vpcs --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text)
SUBNET=$(aws ec2 describe-subnets --filters Name=vpc-id,Values="$VPC" \
  --query 'Subnets[?MapPublicIpOnLaunch==`true`]|[0].SubnetId' --output text)
AZ=$(aws ec2 describe-subnets --subnet-ids "$SUBNET" --query 'Subnets[0].AvailabilityZone' --output text)
SG=$(aws ec2 describe-security-groups --filters Name=group-name,Values=talaria-v11 Name=vpc-id,Values="$VPC" \
  --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null || true)
if [ "$SG" = "None" ] || [ -z "$SG" ]; then
  SG=$(aws ec2 create-security-group --group-name talaria-v11 --description "Talaria v1.1 lab" \
    --vpc-id "$VPC" --query GroupId --output text)
  MYIP=$(curl -s https://checkip.amazonaws.com || echo 0.0.0.0)
  aws ec2 authorize-security-group-ingress --group-id "$SG" --protocol tcp --port 22 --cidr "$MYIP/32" >/dev/null
  # all traffic within the group: UDP echo, tcp_ping, ICMP between Talaria and the neighbour
  aws ec2 authorize-security-group-ingress --group-id "$SG" --protocol -1 --source-group "$SG" >/dev/null
  echo "SG: $SG (SSH from $MYIP)"
fi

# ── placement group (cluster): neighbours in one rack ────────────────────────
aws ec2 create-placement-group --group-name talaria-v11-cpg --strategy cluster >/dev/null 2>&1 || true

AMI=$(aws ssm get-parameters --names /aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64 \
  --query 'Parameters[0].Value' --output text)
echo "== Amazon Linux AMI: $AMI"

run_instance() { # $1 = type, $2 = name, $3 = extra block-device json ("" — none), $4 = 1 -> into the placement group
  local extra=()
  [ -n "$3" ] && extra=(--block-device-mappings "$3")
  # not all types support a cluster placement group (t3 does not), and the builder
  # does not need it: it is the node under test and the neighbour that must share a rack.
  local place="AvailabilityZone=$AZ"
  [ "${4:-0}" = 1 ] && place="GroupName=talaria-v11-cpg,AvailabilityZone=$AZ"
  aws ec2 run-instances --image-id "$AMI" --instance-type "$1" --key-name talaria-v11 \
    --security-group-ids "$SG" --subnet-id "$SUBNET" --associate-public-ip-address \
    --placement "$place" \
    --tag-specifications "ResourceType=instance,Tags=[{Key=Project,Value=talaria-v11},{Key=Name,Value=$2}]" \
    "${extra[@]}" --query 'Instances[0].InstanceId' --output text
}

# The builder's empty volume sets the ROOT SIZE of the future AMI: the snapshot is taken
# of it whole, and register-image takes the size from the snapshot. It was 2 GiB — the
# AL2023-like layout (an 8 GiB volume, see mk-bios.sh LAYOUT=amzn) needs at least
# the image size, otherwise dd gets truncated and the backup GPT ends up in the wrong place.
BUILDER_VOL=${BUILDER_VOL:-8}
BUILDER=$(run_instance t3.small talaria-builder \
  "[{\"DeviceName\":\"/dev/sdf\",\"Ebs\":{\"VolumeSize\":${BUILDER_VOL},\"VolumeType\":\"gp3\",\"DeleteOnTermination\":true}}]" 0)
PEER=$(run_instance "$PEER_TYPE" talaria-peer "" 1)
echo "builder=$BUILDER peer=$PEER — waiting for running"
aws ec2 wait instance-running --instance-ids "$BUILDER" "$PEER"
ip_of() { aws ec2 describe-instances --instance-ids "$1" --query "$2" --output text; }
BUILDER_IP=$(ip_of "$BUILDER" 'Reservations[0].Instances[0].PublicIpAddress')
PEER_IP=$(ip_of "$PEER" 'Reservations[0].Instances[0].PublicIpAddress')
PEER_PRIV=$(ip_of "$PEER" 'Reservations[0].Instances[0].PrivateIpAddress')

cat > "$W/lab.env" <<EOF
BUILDER=$BUILDER
BUILDER_IP=$BUILDER_IP
PEER=$PEER
PEER_IP=$PEER_IP
PEER_PRIV=$PEER_PRIV
SG=$SG
SUBNET=$SUBNET
AZ=$AZ
CPG=talaria-v11-cpg
AWS_DEFAULT_REGION=$AWS_DEFAULT_REGION
EOF
echo "== the stand is ready: $W/lab.env"
cat "$W/lab.env"

# ── tools on the neighbour: udp_ping/tcp_ping/udp_echo from bench/ ───────────
SSH="ssh -i $W/talaria-v11.pem -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10"
for i in $(seq 1 30); do $SSH ec2-user@"$PEER_IP" true 2>/dev/null && break; sleep 5; done
scp -i "$W/talaria-v11.pem" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -q \
  "$REPO/bench/udp_ping.c" "$REPO/bench/tcp_ping.c" "$REPO/bench/udp_echo.c" "$REPO/bench/udp_flood.c" ec2-user@"$PEER_IP":/tmp/
$SSH ec2-user@"$PEER_IP" 'sudo dnf install -q -y gcc >/dev/null 2>&1; \
  for f in udp_ping tcp_ping udp_echo udp_flood; do gcc -O2 -o /tmp/$f /tmp/$f.c 2>/dev/null || echo "failed to build: $f"; done; ls -l /tmp/udp_ping /tmp/tcp_ping /tmp/udp_echo /tmp/udp_flood 2>&1 | tail -4'
echo "== the neighbour is equipped (udp_ping/tcp_ping/udp_echo in /tmp)"
echo "TEAR DOWN THE STAND: infra/aws/lab-down.sh"
