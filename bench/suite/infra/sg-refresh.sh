#!/bin/sh
# REFRESH THE RUNNER'S ADDRESS IN THE STAND'S SECURITY GROUP BEFORE A RUN.
#
#   infra/sg-refresh.sh            # add the current address if it is not there
#   env: STAND_DIR=
#
# A DEFECT OF THE INSTRUMENT THAT COST A RUN. The stand's security
# group is open for EXACTLY the runner's address at the moment of `terraform apply`
# (see main.tf, `local.ssh_cidr`). The runner's VPN reconnects several times
# a day and CHANGES the public address — at that moment the stand becomes unreachable
# entirely, and the run loses everything it has not taken yet. That is how the whole
# Linux side of the run `ev36fix-20260917-203755` was lost: Talaria finished, the address
# changed, and ten Linux points honestly spent their retry budget (600 s each)
# with "Connection refused".
#
# The rule: the address is refreshed BEFORE every run and costs one second.
# Previous addresses are NOT removed — a run may come from two runners, and an extra
# /32 in the group of a stand that lives for hours costs nothing.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
STAND_DIR=${STAND_DIR:-$HOME/.talaria-lab/stand}
[ -f "$STAND_DIR/stand.env" ] || { echo "sg-refresh: the stand is not up"; exit 0; }
# shellcheck disable=SC1091
. "$STAND_DIR/stand.env"
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-${STAND_REGION:-ap-northeast-1}}

IP=$(curl -s --max-time 10 https://checkip.amazonaws.com | tr -d '\n\r')
case "$IP" in
    *.*.*.*) ;;
    *) echo "sg-refresh: our own address could not be determined ($IP) — skipping"; exit 0 ;;
esac
CIDR="$IP/32"

SGS=$(aws ec2 describe-security-groups --filters "Name=tag:Project,Values=talaria-bench" \
        --query 'SecurityGroups[].GroupId' --output text 2>/dev/null)
[ -n "$SGS" ] || { echo "sg-refresh: no stand groups found"; exit 0; }

added=0
for sg in $SGS; do
    have=$(aws ec2 describe-security-groups --group-ids "$sg" \
            --query "SecurityGroups[].IpPermissions[].IpRanges[?CidrIp=='$CIDR'].CidrIp" --output text 2>/dev/null)
    [ -n "$have" ] && continue
    # The same ports terraform opens: ssh and the stand's range.
    for spec in "22 22" "9000 9999"; do
        set -- $spec
        aws ec2 authorize-security-group-ingress --group-id "$sg" --protocol tcp \
            --port "$1-$2" --cidr "$CIDR" >/dev/null 2>&1 && added=1
    done
done
if [ $added = 1 ]; then
    echo "sg-refresh: the runner's address HAS CHANGED, $CIDR added to the stand's groups"
else
    echo "sg-refresh: the runner's address $CIDR is already allowed"
fi
