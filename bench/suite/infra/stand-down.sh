#!/bin/sh
# Tear the EC2 stand down and clean up EVERYTHING after it: the instances, the placement group,
# the key, and also the AMI and the snapshot built for this run.
#
#   stand-down.sh [--keep-ami]
#
# The stand costs money every minute, so the cleanup is a separate visible command
# rather than the tail of a run that may not have finished.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
TF="$HERE/terraform"
STAND_DIR=${STAND_DIR:-$HOME/.talaria-lab/stand}
KEEP_AMI=${1:-}
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}

AMI=""
[ -f "$STAND_DIR/stand.env" ] && . "$STAND_DIR/stand.env" && AMI=${STAND_AMI:-}

echo "== terraform destroy"
terraform -chdir="$TF" destroy -auto-approve -input=false \
    -var "region=$AWS_DEFAULT_REGION" -var "talaria_ami=${AMI:-ami-00000000000000000}" \
    -var "arch=${STAND_ARCH:-x86_64}" | tail -10

if [ -n "$AMI" ] && [ "$KEEP_AMI" != "--keep-ami" ]; then
    echo "== removing AMI $AMI and its snapshot"
    SNAP=$(aws ec2 describe-images --image-ids "$AMI" \
        --query 'Images[0].BlockDeviceMappings[0].Ebs.SnapshotId' --output text 2>/dev/null || echo None)
    aws ec2 deregister-image --image-id "$AMI" 2>/dev/null || true
    [ "$SNAP" = None ] || aws ec2 delete-snapshot --snapshot-id "$SNAP" 2>/dev/null || true
fi

# Leftovers of previous runs: volumes and the disposable image-upload helpers may have
# stayed if the AMI build was interrupted halfway.
echo "== checking the project's leftovers"
# ONLY OUR OWN TAG. `Project=talaria` used to be cleaned up too — and that tag marks
# the SINGLE instances of `infra/aws/talaria-deploy.sh`, i.e. the stand's cleanup killed
# other probes running next to it (that is exactly how an ENA probe was lost).
# The stand marks its resources with `talaria-bench`, and we limit ourselves to those.
for iid in $(aws ec2 describe-instances --filters Name=tag:Project,Values=talaria-bench \
        Name=instance-state-name,Values=running,pending,stopping,stopped \
        --query 'Reservations[].Instances[].InstanceId' --output text 2>/dev/null); do
    echo "   terminating instance $iid"
    aws ec2 terminate-instances --instance-ids "$iid" >/dev/null 2>&1 || true
done
for vol in $(aws ec2 describe-volumes --filters Name=tag:Project,Values=talaria-bench \
        Name=status,Values=available --query 'Volumes[].VolumeId' --output text 2>/dev/null); do
    echo "   deleting volume $vol"
    aws ec2 delete-volume --volume-id "$vol" >/dev/null 2>&1 || true
done

# LEFTOVERS OF AN INTERRUPTED IMAGE BUILD. `mk-ami.sh` calls `talaria-deploy.sh`, and that one
# marks its resources with `Project=talaria` — the COMMON tag of single probes, by which
# cleaning up is not allowed (that is how another ENA probe was lost). But their name is their own
# and narrow: `tlbench-<workload>-<stamp>`, and selecting by it is safe.
#
# Why this is here. A suite killed FROM OUTSIDE leaves an orphaned `mk-ami.sh`: it
# finishes without its parent and leaves a volume, a snapshot and an AMI that are in neither
# terraform's state nor under the stand's tag. That is what happened once — the cleanup
# said "zero", while the volume and the snapshot stayed. They are billed silently.
echo "== picking up the image build's leftovers (Name=tlbench-*)"
for iid in $(aws ec2 describe-instances --filters "Name=tag:Name,Values=tlbench-*" \
        Name=instance-state-name,Values=running,pending,stopping,stopped \
        --query 'Reservations[].Instances[].InstanceId' --output text 2>/dev/null); do
    echo "   terminating the image-build instance $iid"
    aws ec2 terminate-instances --instance-ids "$iid" >/dev/null 2>&1 || true
done
# THE AMI GOES BEFORE the snapshot: a snapshot with a registered image against it
# cannot be deleted.
for img in $(aws ec2 describe-images --owners self --filters "Name=tag:Name,Values=tlbench-*" \
        --query 'Images[].ImageId' --output text 2>/dev/null); do
    [ "$img" = "${AMI:-}" ] && continue
    snap=$(aws ec2 describe-images --image-ids "$img" \
        --query 'Images[0].BlockDeviceMappings[0].Ebs.SnapshotId' --output text 2>/dev/null || echo None)
    echo "   removing the orphaned AMI $img"
    aws ec2 deregister-image --image-id "$img" >/dev/null 2>&1 || true
    [ "$snap" = None ] || aws ec2 delete-snapshot --snapshot-id "$snap" >/dev/null 2>&1 || true
done
for snap in $(aws ec2 describe-snapshots --owner-ids self --filters "Name=tag:Name,Values=tlbench-*" \
        --query 'Snapshots[].SnapshotId' --output text 2>/dev/null); do
    echo "   deleting the orphaned snapshot $snap"
    aws ec2 delete-snapshot --snapshot-id "$snap" >/dev/null 2>&1 || true
done
for vol in $(aws ec2 describe-volumes --filters "Name=tag:Name,Values=tlbench-*" \
        Name=status,Values=available --query 'Volumes[].VolumeId' --output text 2>/dev/null); do
    echo "   deleting the orphaned volume $vol"
    aws ec2 delete-volume --volume-id "$vol" >/dev/null 2>&1 || true
done
# LOCAL LEFTOVERS OF THE SAME BUILD: `talaria-deploy.sh` removes its own
# tlbench-*.img files after registering the AMI (before that they piled up to
# 150 GB), but a build killed in the middle of the upload leaves them on disk. While
# mk-ami.sh is alive, do not touch them — it is still uploading.
if ! pgrep -f 'mk-ami\.sh|talaria-deploy\.sh' >/dev/null 2>&1; then
    for f in "${WORKDIR:-$HOME/.talaria-lab}"/tlbench-*.img "${WORKDIR:-$HOME/.talaria-lab}"/tlbench-*.img.gz; do
        [ -f "$f" ] || continue
        echo "   deleting the local build image $(basename "$f")"
        rm -f "$f"
    done
fi

# FREE VOLUMES ARE ALWAYS SUSPICIOUS, even ones with a foreign tag. The intermediate volumes of
# an image upload (`infra/aws/talaria-deploy.sh`) are left WITHOUT tags when a build is
# interrupted, so a selection by tag does not see them. Deleting someone else's silently is not
# allowed — so we simply SHOW them: an unaccounted volume costs money every hour.
free=$(aws ec2 describe-volumes --filters Name=status,Values=available \
    --query 'Volumes[].[VolumeId,Size]' --output text 2>/dev/null)
if [ -n "$free" ]; then
    echo "== WARNING: there are FREE volumes in the region (they are billed):"
    echo "$free" | sed 's/^/   /'
    echo "   remove with: aws ec2 delete-volume --volume-id <vol-...>"
fi

rm -f "$STAND_DIR/stand.env"
echo "== the stand is torn down"
echo "   check for leftovers: aws ec2 describe-instances --filters Name=tag:Project,Values=talaria-bench --query 'Reservations[].Instances[].[InstanceId,State.Name]' --output text"
