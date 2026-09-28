#!/bin/sh
# THE SMOKE TEST OF THE DEPLOYMENT PATH — a gate run BEFORE any measurement on EC2:
#
#   build the AMI -> launch an instance -> a DHCP lease -> user-data read ->
#   the role from the manifest applied -> ONE request answered -> torn down ->
#   and in the region there is NO instance, NO AMI, NO snapshot, NO volume
#   and NO security group beyond what was there before the run.
#
# This exact path went unexecuted for seven sessions in a row, and the very first run
# found four mines in the stand and a blocker in the kernel. Every step here
# is judged by the instance's serial console (`get-console-output`) and by the server's
# answer from the public address, and the cleanup is judged by COMPARING the inventory of resources before and
# after: "terminate returned 0" does not count as a cleanup.
#
#   smoke.sh [<workload>=redis]
#   env: SMOKE_TYPE=c6i.large  AWS_DEFAULT_REGION=ap-northeast-1  SMOKE_KEEP=1
#        (do not tear down on failure — for analysis; the inventory will then certainly not match)
#
# It costs a few cents and ~6 minutes. It touches nothing foreign: its own resources are
# tagged Project=talaria-smoke and are deleted by id rather than by tag.
set -u
WL=${1:-redis}
HERE=$(cd "$(dirname "$0")" && pwd)
SUITE=$(cd "$HERE/.." && pwd)
REPO=$(cd "$SUITE/../.." && pwd)
TYPE=${SMOKE_TYPE:-c6i.large}
export AWS_DEFAULT_REGION=${AWS_DEFAULT_REGION:-ap-northeast-1}
W=${SMOKE_DIR:-$HOME/.talaria-lab/smoke-$(date +%Y%m%d-%H%M%S)}
mkdir -p "$W"
PASS=0; FAIL=0
ok()   { PASS=$((PASS + 1)); echo "  PASS  $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL  $1"; }
T0=$(date +%s); since() { echo "$(( $(date +%s) - T0 )) s"; }

command -v aws >/dev/null || { echo "no aws cli"; exit 1; }
aws sts get-caller-identity >/dev/null || { echo "the aws cli is not authenticated"; exit 1; }
. "$SUITE/workloads/$WL/meta.env"

# ── An inventory of the region's resources: everything that costs money or clutters the account ──
inventory() {
    {
        aws ec2 describe-instances --filters Name=instance-state-name,Values=pending,running,stopping,stopped \
            --query 'Reservations[].Instances[].[InstanceId]' --output text | sed 's/^/instance /'
        aws ec2 describe-images --owners self --query 'Images[].[ImageId]' --output text | sed 's/^/ami /'
        aws ec2 describe-snapshots --owner-ids self --query 'Snapshots[].[SnapshotId]' --output text | sed 's/^/snapshot /'
        aws ec2 describe-volumes --query 'Volumes[].[VolumeId]' --output text | sed 's/^/volume /'
        aws ec2 describe-security-groups --query 'SecurityGroups[].[GroupId]' --output text | sed 's/^/sg /'
    } | grep -v '^[a-z]* None$' | sort
}
echo "== the inventory of resources BEFORE ($AWS_DEFAULT_REGION)"
inventory > "$W/before.txt" || { echo "the inventory could not be taken"; exit 1; }
sed 's/^/   /' "$W/before.txt" | head -40
BEFORE_FREE=$(aws ec2 describe-volumes --filters Name=status,Values=available --query 'Volumes[].[VolumeId]' --output text | grep -v None | wc -l)
[ "$BEFORE_FREE" = 0 ] || echo "   WARNING: before the run the region has $BEFORE_FREE free volumes — they are not ours, but they are billed"

AMI=""; SNAP=""; IID=""; SG=""
cleanup() {
    echo "== cleanup ($(since))"
    if [ -n "$IID" ]; then
        aws ec2 terminate-instances --instance-ids "$IID" >/dev/null 2>&1 || echo "   terminate $IID refused"
        aws ec2 wait instance-terminated --instance-ids "$IID" 2>/dev/null || echo "   instance $IID did not reach terminated"
    fi
    if [ -n "$AMI" ]; then
        aws ec2 deregister-image --image-id "$AMI" >/dev/null 2>&1 || echo "   deregister $AMI refused"
    fi
    if [ -n "$SNAP" ]; then
        aws ec2 delete-snapshot --snapshot-id "$SNAP" >/dev/null 2>&1 || echo "   delete-snapshot $SNAP refused"
    fi
    if [ -n "$SG" ]; then
        # The instance's ENI is not detached from the group immediately after terminated
        i=0; while ! aws ec2 delete-security-group --group-id "$SG" >/dev/null 2>&1; do
            i=$((i + 1)); [ $i -lt 30 ] || { echo "   group $SG was not deleted"; break; }; sleep 5
        done
    fi
}

# ── 1. AMI ──────────────────────────────────────────────────────────────────
echo "== 1. the AMI for $WL ($(since))"
AMI=$(OUT="$W/bench-$WL.img" "$HERE/mk-ami.sh" "$WL" 2>&1 | tee "$W/mk-ami.log" | sed -n 's/^AMI_ID=//p' | tail -1)
if [ -n "$AMI" ]; then ok "the AMI was built: $AMI ($(since))"; else fail "the AMI was not built — $W/mk-ami.log"; fi
[ -n "$AMI" ] || { cleanup; echo "smoke: PASS=$PASS FAIL=$FAIL"; exit 1; }
SNAP=$(aws ec2 describe-images --image-ids "$AMI" --query 'Images[0].BlockDeviceMappings[0].Ebs.SnapshotId' --output text)
DOT=$(aws ec2 describe-images --image-ids "$AMI" --query 'Images[0].BlockDeviceMappings[0].Ebs.DeleteOnTermination' --output text)
if [ "$DOT" = True ]; then ok "the AMI's volume dies with the instance (DeleteOnTermination=true)"
else fail "the AMI's volume survives the instance (DeleteOnTermination=$DOT) — every run would leave a billable volume"; fi

# ── 2. the instance: its own group, the workload port open only to the operator ──
echo "== 2. the instance $TYPE ($(since))"
VPC=$(aws ec2 describe-vpcs --filters Name=isDefault,Values=true --query 'Vpcs[0].VpcId' --output text)
SUBNET=$(aws ec2 describe-subnets --filters Name=vpc-id,Values="$VPC" \
    --query 'Subnets[?MapPublicIpOnLaunch==`true`]|[0].SubnetId' --output text)
MYIP=$(curl -s --max-time 10 https://checkip.amazonaws.com)
SG=$(aws ec2 create-security-group --group-name "talaria-smoke-$$" --description "Talaria deploy smoke gate" \
    --vpc-id "$VPC" --tag-specifications "ResourceType=security-group,Tags=[{Key=Project,Value=talaria-smoke}]" \
    --query GroupId --output text)
aws ec2 authorize-security-group-ingress --group-id "$SG" --protocol tcp --port "$PORT" --cidr "$MYIP/32" >/dev/null
# The role comes from user-data, as on the stand: one image, the manifest chosen at launch.
UD="talaria.manifest=/cell.manifest"
IID=$(aws ec2 run-instances --image-id "$AMI" --instance-type "$TYPE" \
    --security-group-ids "$SG" --subnet-id "$SUBNET" --associate-public-ip-address \
    --user-data "$UD" \
    --tag-specifications "ResourceType=instance,Tags=[{Key=Project,Value=talaria-smoke},{Key=Name,Value=talaria-smoke-$WL}]" \
    --query 'Instances[0].InstanceId' --output text 2>"$W/run.err")
if [ -n "$IID" ] && [ "$IID" != None ]; then ok "the instance was launched: $IID"; else fail "run-instances refused: $(cat "$W/run.err")"; IID=""; fi
[ -n "$IID" ] || { cleanup; echo "smoke: PASS=$PASS FAIL=$FAIL"; exit 1; }
aws ec2 wait instance-running --instance-ids "$IID"
IP=$(aws ec2 describe-instances --instance-ids "$IID" --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
echo "   address $IP ($(since))"

# ── 3. one request from the public address ───────────────────────────────────
echo "== 3. one $PROTO request to :$PORT ($(since))"
probe() {
    python3 - "$IP" "$PORT" "$PROTO" <<'PY'
import socket, sys
ip, port, proto = sys.argv[1], int(sys.argv[2]), sys.argv[3]
req = {"resp": b"*1\r\n$4\r\nPING\r\n", "http": b"GET / HTTP/1.0\r\nHost: x\r\n\r\n"}.get(proto)
want = {"resp": b"+PONG", "http": b"HTTP/1."}.get(proto)
if req is None:
    print("protocol %s is not covered by the smoke test" % proto); sys.exit(3)
try:
    s = socket.create_connection((ip, port), timeout=3)
    s.sendall(req); d = s.recv(256); s.close()
except OSError as e:
    print("no answer: %s" % e); sys.exit(1)
print("answer: %r" % d[:40])
sys.exit(0 if d.startswith(want) else 2)
PY
}
i=0; RC=1
while [ $i -lt 60 ]; do
    OUTP=$(probe); RC=$?
    [ $RC = 0 ] || [ $RC = 2 ] || [ $RC = 3 ] && break
    i=$((i + 1)); sleep 5
done
case $RC in
0) ok "the server answered one request in $(since): $OUTP" ;;
2) fail "the server answered with the wrong thing: $OUTP" ;;
3) fail "$OUTP" ;;
*) fail "the server did not answer within 5 minutes: $OUTP" ;;
esac

# ── 4. the console: DHCP, user-data, the knobs, the role, no boot refusal ────
echo "== 4. the serial console ($(since))"
# The console on Nitro is published with a delay of up to a minute — we wait for the lines we need.
i=0
while [ $i -lt 24 ]; do
    aws ec2 get-console-output --instance-id "$IID" --latest --output text > "$W/console.log" 2>/dev/null
    grep -aq "a process by the manifest\|BOOT REFUSED" "$W/console.log" && break
    i=$((i + 1)); sleep 5
done
C=$W/console.log
if grep -aq "dhcp lease" "$C"; then ok "the DHCP lease: $(grep -a -m1 'dhcp lease' "$C" | sed 's/.*lease //')"; else fail "there is no DHCP lease in the console"; fi
if grep -aq "user-data -> cmdline += " "$C"; then ok "the user-data was read into the cmdline"; else fail "the user-data was not read (no \"user-data -> cmdline +=\")"; fi
if grep -aq "talaria: the knob talaria.manifest = /cell.manifest" "$C"; then ok "the manifest knob was parsed from user-data and printed"; else fail "the manifest knob from user-data was not printed"; fi
if grep -aq "BOOT REFUSED" "$C"; then fail "the kernel REFUSED to boot: $(grep -a -m1 'REFUSED' "$C")"; else ok "there is no boot refusal"; fi
if grep -aq "a process by the manifest" "$C"; then ok "the role was applied: a process by the manifest was started"; else fail "the role was not applied (no \"a process by the manifest\")"; fi
if grep -aq "panic\|PANIC\|#PF\|#GP" "$C"; then fail "a panic/exception in the console: $(grep -a -m1 'panic\|PANIC\|#PF\|#GP' "$C")"; else ok "there are no panics"; fi
[ -s "$C" ] || fail "the console is empty (get-console-output gave nothing)"

# ── 5. the cleanup and the inventory AFTER ───────────────────────────────────
if [ "$FAIL" != 0 ] && [ "${SMOKE_KEEP:-0}" = 1 ]; then
    echo "== SMOKE_KEEP=1: the instance $IID ($IP), the AMI $AMI, the snapshot $SNAP and the group $SG were LEFT — tear them down by hand"
    echo "smoke: PASS=$PASS FAIL=$FAIL"; exit 1
fi
cleanup
echo "== the inventory of resources AFTER ($(since))"
# A snapshot/AMI does not disappear from describe instantly — we give it a minute.
i=0
while [ $i -lt 12 ]; do
    inventory > "$W/after.txt"
    diff "$W/before.txt" "$W/after.txt" > "$W/diff.txt" && break
    i=$((i + 1)); sleep 5
done
if diff -q "$W/before.txt" "$W/after.txt" >/dev/null; then
    ok "the inventory matches: no instance, no AMI, no snapshot, no volume and no group were left"
else
    fail "LEFTOVERS after the cleanup (they are billed):"
    grep '^[<>]' "$W/diff.txt" | sed 's/^/        /'
fi
echo
echo "smoke: PASS=$PASS FAIL=$FAIL in $(since); the journal is $W"
[ "$FAIL" = 0 ]
