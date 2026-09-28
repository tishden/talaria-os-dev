#!/bin/bash
# lab-down.sh: tear down EVERYTHING an EC2 working session created.
# Goes by the tag Project=talaria-v11 and by the names talaria-v11*: instances, AMIs,
# snapshots, volumes, the security group, the placement group, the key pair.
# Prints what is left — to check by eye that nothing is being billed.
set -uo pipefail
W=${1:-$HOME/.talaria-lab}
AWS_INI=${AWS_INI:-}   # optional: a file "Access key=…/Secret=…"; otherwise the AWS CLI chain (~/.aws, AWS_PROFILE, env)
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
if [ -z "${AWS_ACCESS_KEY_ID:-}" ] && [ -n "$AWS_INI" ] && [ -f "$AWS_INI" ]; then
  AWS_ACCESS_KEY_ID=$(sed -n 's/^ *Access key *= *//Ip' "$AWS_INI" | tr -d '\r')
  AWS_SECRET_ACCESS_KEY=$(sed -n 's/^ *Secret[ a-z]* *= *//Ip' "$AWS_INI" | tr -d '\r')
  export AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
fi

IIDS=$(aws ec2 describe-instances --filters Name=tag:Project,Values=talaria-v11 \
  Name=instance-state-name,Values=pending,running,stopping,stopped \
  --query 'Reservations[].Instances[].InstanceId' --output text)
if [ -n "$IIDS" ]; then
  echo "== terminate: $IIDS"
  aws ec2 terminate-instances --instance-ids $IIDS >/dev/null
  aws ec2 wait instance-terminated --instance-ids $IIDS
fi

for AMI in $(aws ec2 describe-images --owners self --filters Name=name,Values='talaria-v11*' \
    --query 'Images[].ImageId' --output text); do
  echo "== deregister $AMI"; aws ec2 deregister-image --image-id "$AMI" >/dev/null
done
for SNAP in $(aws ec2 describe-snapshots --owner-ids self \
    --filters Name=tag:Project,Values=talaria-v11 --query 'Snapshots[].SnapshotId' --output text) \
           $(aws ec2 describe-snapshots --owner-ids self \
    --filters Name=description,Values='talaria-v11*' --query 'Snapshots[].SnapshotId' --output text); do
  aws ec2 delete-snapshot --snapshot-id "$SNAP" >/dev/null 2>&1 && echo "== snapshot $SNAP deleted"
done
for VOL in $(aws ec2 describe-volumes --filters Name=status,Values=available \
    Name=tag:Project,Values=talaria-v11 --query 'Volumes[].VolumeId' --output text); do
  aws ec2 delete-volume --volume-id "$VOL" >/dev/null 2>&1 && echo "== volume $VOL deleted"
done
aws ec2 delete-placement-group --group-name talaria-v11-cpg >/dev/null 2>&1 && echo "== placement group deleted"
SG=$(aws ec2 describe-security-groups --filters Name=group-name,Values=talaria-v11 \
  --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null)
[ -n "$SG" ] && [ "$SG" != None ] && aws ec2 delete-security-group --group-id "$SG" >/dev/null 2>&1 && echo "== SG $SG deleted"
aws ec2 delete-key-pair --key-name talaria-v11 >/dev/null 2>&1 && echo "== key pair deleted"
rm -f "$W/talaria-v11.pem" "$W/lab.env" "$W/last-node.sh"

echo "== LEFT OVER (should be empty or only resources that are not ours):"
aws ec2 describe-instances --filters Name=instance-state-name,Values=pending,running \
  --query 'Reservations[].Instances[].[InstanceId,InstanceType,Tags[?Key==`Name`].Value|[0]]' --output text
aws ec2 describe-images --owners self --query 'Images[].[ImageId,Name]' --output text
aws ec2 describe-volumes --filters Name=status,Values=available --query 'Volumes[].VolumeId' --output text
