#!/bin/bash
# variants.sh — one workload, ONE Talaria AMI, N kernel variants side by side, and a Linux
# control running the same binary on the same instance type, in the same hour.
#
# Why. A knob's effect on a tail is smaller than the drift between two bring-ups (the
# scoreboard's own lesson, bench/suite/ab-phase.sh), so variants must run at the same time on
# instances of one launch, and Linux must run next to them: "8.2 us" means nothing without
# "Linux 4.6-5.7 in that hour". The image is built once with `--userdata` (talaria-deploy.sh):
# each instance gets its variant as user-data, i.e. as the cmdline tail, without a rebuild.
# Written for the mdpipe tail on a Graviton (docs/ARM64-PLAN.md §10.15, ENGINEERING-NOTES 47),
# usable for any workload that prints its result on the console.
#
#   infra/aws/variants.sh --app PATH --args "..." [options] -- "tail 1" "tail 2" ...
#     --type T       instance type (default c7g.large; the Linux control is AL2023 of its arch)
#     --smp N        cores for the workload (default 2)
#     --ok REGEX     the console line that ends a variant's run (default "mdpipe: (OK|FAIL)")
#     --linux N      how many runs on the Linux control (default 3; 0 = no control)
#     --wait SEC     how long to wait for every verdict, from the launch (default 900)
#     --out DIR      where the results go (default bench/suite/results/variants-<time>)
#   A tail of "-" is the default kernel (empty user-data).
#   Out: DIR/v<i>.console (each variant's serial), DIR/linux.txt, DIR/ids.
#
# Everything it creates (the AMI, its snapshot, the instances) is removed at the end and on
# an interrupt; the instances are tagged Project=talaria, Name=variants-<time>.
set -u
REPO=$(cd "$(dirname "$0")/../.." && pwd)
TYPE=c7g.large SMP=2 OK='mdpipe: (OK|FAIL)' NLINUX=3 WAIT=900 OUT= APP= ARGS=
while [ $# -gt 0 ]; do
  case "$1" in
    --app) APP=$2; shift 2;; --args) ARGS=$2; shift 2;; --type) TYPE=$2; shift 2;;
    --smp) SMP=$2; shift 2;; --ok) OK=$2; shift 2;; --linux) NLINUX=$2; shift 2;;
    --wait) WAIT=$2; shift 2;; --out) OUT=$2; shift 2;;
    --) shift; break;;
    *) echo "variants.sh: unknown option $1"; exit 1;;
  esac
done
[ -f "$APP" ] && [ $# -gt 0 ] || { sed -n '13,22p' "$0"; exit 1; }
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
STAMP=$(date +%Y%m%d-%H%M%S)
OUT=${OUT:-$REPO/bench/suite/results/variants-$STAMP}; mkdir -p "$OUT"
case "$TYPE" in [a-z]*[0-9]g*.*) LSSM=al2023-ami-kernel-default-arm64;; *) LSSM=al2023-ami-kernel-default-x86_64;; esac
KEY=$HOME/.talaria-lab/talaria-v11.pem
SSHO="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o LogLevel=ERROR"
AMI= SNAP= IIDS=
cleanup() {
  [ -n "$IIDS" ] && { aws ec2 terminate-instances --instance-ids $IIDS >/dev/null; aws ec2 wait instance-terminated --instance-ids $IIDS; }
  [ -n "$AMI" ] && aws ec2 deregister-image --image-id "$AMI"
  [ -n "$SNAP" ] && [ "$SNAP" != None ] && aws ec2 delete-snapshot --snapshot-id "$SNAP"
  IIDS= AMI= SNAP=
}
trap 'cleanup; exit 1' INT TERM

echo "== variants -> $OUT ($# variants on $TYPE, the Linux control: $NLINUX runs)"
"$REPO/infra/aws/talaria-deploy.sh" --app "$APP" --args "$ARGS" --type "$TYPE" --smp "$SMP" --restart 0 \
  --stop-after $((WAIT - 120)) --userdata --name "variants-$STAMP" --ami-only > "$OUT/deploy.log" 2>&1
AMI=$(sed -n 's/^AMI_ID=//p' "$OUT/deploy.log" | tail -1)
[ -n "$AMI" ] || { echo "no AMI (see $OUT/deploy.log)"; tail -5 "$OUT/deploy.log"; exit 1; }
SNAP=$(aws ec2 describe-images --image-ids "$AMI" --query 'Images[0].BlockDeviceMappings[0].Ebs.SnapshotId' --output text)
VPC=$(aws ec2 describe-vpcs --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text)
SUBNET=$(aws ec2 describe-subnets --filters Name=vpc-id,Values="$VPC" --query 'Subnets[?MapPublicIpOnLaunch==`true`]|[0].SubnetId' --output text)
SG=$(aws ec2 describe-security-groups --filters Name=group-name,Values=talaria-v11 Name=vpc-id,Values="$VPC" --query 'SecurityGroups[0].GroupId' --output text)
TAGS="ResourceType=instance,Tags=[{Key=Project,Value=talaria},{Key=Name,Value=variants-$STAMP}]"
launch() {  # launch AMI [user-data] -> instance id
  aws ec2 run-instances --image-id "$1" --instance-type "$TYPE" --key-name talaria-v11 \
    --security-group-ids "$SG" --subnet-id "$SUBNET" --associate-public-ip-address \
    ${2:+--user-data "$2"} --tag-specifications "$TAGS" --query 'Instances[0].InstanceId' --output text
}
VIDS= i=0
for v in "$@"; do
  i=$((i+1)); ud=$v; [ "$v" = - ] && ud=
  id=$(launch "$AMI" "$ud") || { cleanup; exit 1; }
  echo "v$i $id ${v}" | tee -a "$OUT/ids"; VIDS="$VIDS $id"; IIDS="$IIDS $id"
done
LID=
if [ "$NLINUX" -gt 0 ]; then
  LAMI=$(aws ssm get-parameter --name "/aws/service/ami-amazon-linux-latest/$LSSM" --query Parameter.Value --output text)
  LID=$(launch "$LAMI") && { echo "linux $LID" | tee -a "$OUT/ids"; IIDS="$IIDS $LID"; }
fi
aws ec2 wait instance-running --instance-ids $IIDS
if [ -n "$LID" ]; then
  LIP=$(aws ec2 describe-instances --instance-ids "$LID" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
  for k in $(seq 1 40); do ssh $SSHO ec2-user@"$LIP" true 2>/dev/null && break; sleep 5; done
  scp $SSHO "$APP" ec2-user@"$LIP":/tmp/app >/dev/null
  # the Linux run gets the same argv; an --out under / is moved to /tmp (ec2-user cannot write /)
  LARGS=$(echo "$ARGS" | sed 's| --out /| --out /tmp/|')
  for r in $(seq 1 "$NLINUX"); do ssh $SSHO ec2-user@"$LIP" "cd /tmp && ./app $LARGS 2>&1 | tail -6" >> "$OUT/linux.txt" 2>&1; done
  ssh $SSHO ec2-user@"$LIP" "nproc; uname -r" >> "$OUT/linux.txt" 2>&1
fi
# ONE deadline for all, from the launch: EC2 serves a console minutes late, and a per-variant
# window that starts at once (no Linux control to wait for) gave the first variant an empty
# file while its run had finished. A variant without a verdict is polled again until the end.
T0=$SECONDS; LEFT=$VIDS
while [ -n "$LEFT" ] && [ $((SECONDS - T0)) -lt "$WAIT" ]; do
  NEXT=
  i=0
  for id in $VIDS; do
    i=$((i+1)); case " $LEFT " in *" $id "*) ;; *) continue;; esac
    aws ec2 get-console-output --instance-id "$id" --latest --output text > "$OUT/v$i.console" 2>/dev/null
    if grep -aqE "$OK|PANIC" "$OUT/v$i.console"; then echo "v$i: $(grep -aoE "$OK|PANIC" "$OUT/v$i.console" | tail -1)"
    else NEXT="$NEXT $id"; fi
  done
  LEFT=$NEXT; [ -n "$LEFT" ] && sleep 15
done
i=0; for id in $VIDS; do i=$((i+1)); case " $LEFT " in *" $id "*) echo "v$i: NO VERDICT in ${WAIT}s";; esac; done
cleanup
echo "== done: $OUT"
