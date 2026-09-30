#!/bin/sh
# The arm64 KVM stand in one run (docs/ARM64-PLAN.md §9): bring a `c7g.metal` up with the
# terraform in this directory, provision it (docker, git, binutils, the pinned nightly with
# the aarch64 target), upload the tree, build the arm64 stand image NATIVELY, the kernel and
# the raw Image, run the bring-up gate under KVM and the four probes TCG cannot judge
# (`ring`, `icache`, `bbm`, `svccost`), and destroy the instance unless KEEP=1.
#
#   infra/aws/kvm-stand-aarch64.sh [type] [region]      # default c7g.metal ap-northeast-1
#   KEEP=1 ...                                          # leave the instance up for hand work
#   RINGN=5000000 ...                                   # slots for the ring probe (default 2M)
#
# Everything the run prints lands in infra/aws/logs/aarch64-<stamp>/ — the gate's and the
# probes' output is what goes to docs/MEASUREMENTS.md and the plan's §4.13. A metal
# instance takes 10-20 minutes to come up; the run records how long, and the whole
# thing costs about an hour of the instance if nothing is kept.
set -eu
TYPE=${1:-c7g.metal}
REGION=${2:-ap-northeast-1}   # the vCPU quota there fits a 64-core metal
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
STAMP=$(date +%Y%m%d-%H%M%S)
LOGD="$HERE/logs/aarch64-$STAMP"; mkdir -p "$LOGD"
SSH_OPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -i $HERE/talaria-stand.pem"
T0=$(date +%s)

cleanup() {
  if [ "${KEEP:-0}" = 1 ]; then
    echo "KEEP=1: the instance stays up. Destroy it yourself:"
    echo "   terraform -chdir=$HERE destroy -auto-approve -var instance_type=$TYPE -var region=$REGION -var arch=arm64"
    return
  fi
  echo "== terraform destroy =="
  terraform -chdir="$HERE" destroy -auto-approve -var "instance_type=$TYPE" -var "region=$REGION" -var "arch=arm64" >>"$LOGD/terraform.log" 2>&1 || \
    echo "destroy FAILED — check the console, a metal instance left running is expensive"
}
trap cleanup EXIT

echo "== terraform apply: $TYPE in $REGION (metal takes 10-20 min to come up) =="
terraform -chdir="$HERE" init -input=false >>"$LOGD/terraform.log" 2>&1
terraform -chdir="$HERE" apply -auto-approve -var "instance_type=$TYPE" -var "region=$REGION" -var "arch=arm64" >>"$LOGD/terraform.log" 2>&1
IP=$(terraform -chdir="$HERE" output -raw public_ip)
echo "instance: $IP"

echo "== waiting for ssh (up to 25 min) =="
i=0
until ssh $SSH_OPTS ec2-user@"$IP" true 2>/dev/null; do
  i=$((i+1)); [ $i -gt 150 ] && { echo "ssh did not come up"; exit 1; }
  sleep 10
done
T1=$(date +%s); echo "ssh ready after $((T1-T0)) s from apply" | tee "$LOGD/timing.log"

echo "== the host: arm64? KVM? which core? =="
ssh $SSH_OPTS ec2-user@"$IP" 'uname -m; ls -la /dev/kvm 2>&1; lscpu | grep -E "Model name|Vendor|Flags|^CPU\(s\)" | cut -c1-200' | tee "$LOGD/host-info.log"
ssh $SSH_OPTS ec2-user@"$IP" 'test "$(uname -m)" = aarch64 && test -c /dev/kvm' || { echo "not an arm64 host with KVM — the stand is useless, stopping"; exit 1; }

echo "== provisioning (docker, git, binutils, python3, the pinned nightly + aarch64-unknown-none) =="
PIN=$(sed -n 's/^channel = "\(.*\)"/\1/p' "$REPO/kernel/rust-toolchain.toml")
ssh $SSH_OPTS ec2-user@"$IP" "sudo dnf install -y -q docker git binutils python3 tar && \
  sudo systemctl enable --now docker && sudo usermod -aG docker ec2-user && sudo chmod 666 /dev/kvm && \
  curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none -q && . \$HOME/.cargo/env && \
  rustup toolchain install $PIN --profile minimal -c rust-src -c clippy >/dev/null && \
  rustup target add aarch64-unknown-none --toolchain $PIN" >"$LOGD/provision.log" 2>&1 || { echo "provisioning failed — $LOGD/provision.log"; tail -5 "$LOGD/provision.log"; exit 1; }
T2=$(date +%s); echo "provisioned after $((T2-T1)) s" | tee -a "$LOGD/timing.log"

echo "== uploading the tree (git archive HEAD of the checked-out branch) =="
git -C "$REPO" archive HEAD | ssh $SSH_OPTS ec2-user@"$IP" 'mkdir -p talaria && tar -x -C talaria'

echo "== the arm64 stand image, built natively (fedora:40 pulls its arm64 variant) =="
ssh $SSH_OPTS ec2-user@"$IP" 'cd talaria && docker build -t talaria-qemu-aarch64 -f services/docker/Dockerfile.qemu-aarch64 services/docker' >"$LOGD/image.log" 2>&1 || { echo "the image did not build — $LOGD/image.log"; tail -5 "$LOGD/image.log"; exit 1; }

echo "== the kernel and the raw Image (native objcopy) =="
ssh $SSH_OPTS ec2-user@"$IP" '. $HOME/.cargo/env && cd talaria && AARCH64_OBJCOPY=objcopy kernel/build-aarch64.sh' >"$LOGD/build.log" 2>&1 || { echo "the kernel did not build — $LOGD/build.log"; tail -8 "$LOGD/build.log"; exit 1; }
T3=$(date +%s); echo "built after $((T3-T2)) s" | tee -a "$LOGD/timing.log"

echo "== the bring-up gate under KVM (TAL_KVM=1: the timing lines are judged too) =="
ssh $SSH_OPTS ec2-user@"$IP" 'cd talaria && TAL_KVM=1 kernel/test-aarch64.sh' 2>&1 | tee "$LOGD/gate.log" | grep -E 'FAIL|TOTAL'

echo "== the probes TCG cannot judge (docs/ARM64-PLAN.md §9.2 B-E) =="
for p in "ring talaria.ringn=${RINGN:-2000000}" icache bbm svccost; do
  name=${p%% *}
  echo "-- talaria.test=$p"
  ssh $SSH_OPTS ec2-user@"$IP" "cd talaria && TAL_APPEND='console=ttyAMA0 talaria.test=$p talaria.smp=4' kernel/runq-aarch64.sh 120 -smp 4" \
    2>&1 | tee "$LOGD/probe-$name.log" | grep -E "probe|as expected|NOT|corrupt|stale|conflict|ns per|\[trap\]|PANIC" | cut -c1-200
done

echo "== fifty boots of the gate in a loop (the SMP paths under a real weak machine) =="
ssh $SSH_OPTS ec2-user@"$IP" 'cd talaria && fails=0; for i in $(seq 1 50); do TAL_KVM=1 kernel/test-aarch64.sh > /tmp/g.log 2>&1 || { fails=$((fails+1)); cp /tmp/g.log /tmp/g-fail-$i.log; }; done; echo "loop: $fails of 50 boots failed"; ls /tmp/g-fail-*.log 2>/dev/null | head' 2>&1 | tee "$LOGD/loop.log"
T4=$(date +%s); echo "all done after $((T4-T0)) s; the stand ran $((T4-T1)) s" | tee -a "$LOGD/timing.log"
echo "logs: $LOGD"
