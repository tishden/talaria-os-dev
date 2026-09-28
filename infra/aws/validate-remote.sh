#!/bin/sh
# Validate Talaria OS on AWS bare metal in one run (it removes the risk of having a
# single local stand). Brings up a *.metal instance with terraform from this
# directory, uploads the git tree, installs docker/rust, runs build-images -> the kernel
# build (pinned toolchain) -> the full test.sh -> a futexlat measurement, collects the logs
# and DESTROYS the instance (on error too). Logs: infra/aws/logs/<type>-<date>/.
#
# Usage: AWS credentials in the environment (AWS_ACCESS_KEY_ID/...), then
#   infra/aws/validate-remote.sh c6a.metal        # AMD EPYC Milan (monitorx/mwaitx)
#   infra/aws/validate-remote.sh m7i.metal-24xl   # Sapphire Rapids (WAITPKG)
# Cost: ~$8-12/hour, a full run takes ~1 hour.
set -eu
TYPE=${1:?usage: validate-remote.sh <instance-type> [region]}
REGION=${2:-ap-northeast-1}  # its vCPU quota is 256 (us-east-1: 32 — metal does not fit)
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
TS=$(date +%Y%m%d-%H%M)
LOGD="$HERE/logs/${TYPE}-${TS}"
mkdir -p "$LOGD"
SSH_OPTS="-i $HERE/talaria-stand.pem -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10"

cleanup() {
  if [ "${KEEP:-0}" = 1 ]; then
    echo "== KEEP=1: the instance is LEFT RUNNING (it is billed!). destroy it by hand:"
    echo "   terraform -chdir=$HERE destroy -auto-approve -var instance_type=$TYPE -var region=$REGION"
    return 0
  fi
  echo "== destroy ($TYPE) =="
  terraform -chdir="$HERE" destroy -auto-approve -var "instance_type=$TYPE" -var "region=$REGION" >>"$LOGD/terraform.log" 2>&1 || \
    echo "WARNING: destroy failed — check the AWS console (the instance is billed!)"
}
trap cleanup EXIT

echo "== terraform apply: $TYPE (metal takes 10-20 min to come up) =="
terraform -chdir="$HERE" init -input=false >"$LOGD/terraform.log" 2>&1
terraform -chdir="$HERE" apply -auto-approve -var "instance_type=$TYPE" -var "region=$REGION" >>"$LOGD/terraform.log" 2>&1
IP=$(terraform -chdir="$HERE" output -raw public_ip)
echo "instance: $IP"

echo "== waiting for ssh (up to 25 min) =="
i=0
until ssh $SSH_OPTS ec2-user@"$IP" true 2>/dev/null; do
  i=$((i+1)); [ $i -gt 150 ] && { echo "ssh did not come up"; exit 1; }
  sleep 10
done
echo "ssh ready (after ~$((i*10)) s)"

echo "== provisioning (docker, gcc, rustup) =="
ssh $SSH_OPTS ec2-user@"$IP" 'sudo dnf install -y -q docker git gcc e2fsprogs python3 tar && \
  sudo systemctl enable --now docker && sudo usermod -aG docker ec2-user && \
  curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none -q' \
  >"$LOGD/provision.log" 2>&1
ssh $SSH_OPTS ec2-user@"$IP" 'ls -la /dev/kvm && grep -m1 "model name" /proc/cpuinfo && grep -o -m1 -E "mwaitx|waitpkg" /proc/cpuinfo | sort -u | head -2 || true' | tee "$LOGD/host-info.log"

echo "== uploading the tree (git archive HEAD) =="
git -C "$REPO" archive HEAD | ssh $SSH_OPTS ec2-user@"$IP" 'mkdir -p talaria && tar -x -C talaria'

# FULL=1: additionally upload the READY workload artifacts (they are not in git:
# services/*/disk-*.img) and run validate.sh.
# Building the databases on the stand takes hours, moving the images takes minutes.
if [ "${FULL:-0}" = 1 ]; then
  echo "== FULL=1: uploading the ready workload images =="
  # The HOST DATABASE CLIENTS travel together with the images. Without them B10 failed
  # on the remote stand: run.sh looks for psql and mariadb in the BUILD tree (services/*/
  # build/...), while with KEEPIMG=1 only the .img files were uploaded — there is no build
  # tree on the stand and there cannot be (building the databases there takes hours). The
  # MongoDB probe is mongoprobe.py, which is in git and always present.
  ART=$(cd "$REPO" && ls services/*/disk-*.img \
        services/postgresql/build/pg-static/opt/pg/bin/psql \
        services/mariadb/build/bc/client/mariadb 2>/dev/null)
  [ -n "$ART" ] || echo "WARNING: no local images — B10 will be SKIP"
  if [ -n "$ART" ]; then
    (cd "$REPO" && tar czf - $ART) | ssh $SSH_OPTS ec2-user@"$IP" 'tar xzf - -C talaria'
    ssh $SSH_OPTS ec2-user@"$IP" 'du -sh talaria/services/*/disk-*.img 2>/dev/null | tail -3
      for c in talaria/services/postgresql/build/pg-static/opt/pg/bin/psql talaria/services/mariadb/build/bc/client/mariadb; do
        [ -x "$c" ] && echo "database client present: $c" || echo "WARNING: no client $c — the corresponding B10 degrades to a probe"
      done'
  fi
fi

echo "== toolchain docker images (from scratch) =="
ssh $SSH_OPTS ec2-user@"$IP" 'cd talaria && ./services/docker/build-images.sh' >"$LOGD/images.log" 2>&1

echo "== regression user binaries and disk (docker talaria-musl/alpine-build) =="
ssh $SSH_OPTS ec2-user@"$IP" 'cd talaria && make -C kernel/user all && (cd kernel && python3 mkfs.py)' >"$LOGD/user.log" 2>&1 \
  || { echo "make -C kernel/user all failed — see $LOGD/user.log"; tail -5 "$LOGD/user.log"; }

echo "== kernel build (pinned toolchain) =="
ssh $SSH_OPTS ec2-user@"$IP" '. "$HOME/.cargo/env" && cd talaria/kernel && \
  rustup toolchain install $(sed -n "s/^channel = \"\(.*\)\"/\1/p" rust-toolchain.toml) --profile minimal -c rust-src >/dev/null 2>&1; \
  cargo -Z json-target-spec build --release 2>&1 | tail -2' | tee "$LOGD/build.log"

if [ "${SKIP_TEST:-0}" = 1 ]; then
  echo "== SKIP_TEST=1: the stand is ready, the regression is skipped =="
  echo "ssh: ssh -i $HERE/talaria-stand.pem ec2-user@$IP"
  exit 0
fi
echo "== the full test.sh regression =="
ssh $SSH_OPTS ec2-user@"$IP" 'cd talaria && ./kernel/test.sh' >"$LOGD/test.log" 2>&1 || true
grep -aE 'TOTAL|FAIL |SKIP' "$LOGD/test.log" | tail -12

if [ "${FULL:-0}" = 1 ]; then
  echo "== FULL=1: the heavy validator validate.sh (open source, SLA, soak, fuzzing, databases) =="
  ssh $SSH_OPTS ec2-user@"$IP" 'cd talaria && KEEPIMG=1 ./kernel/validate.sh' >"$LOGD/validate.log" 2>&1 || true
  grep -aE "PASS |FAIL |SKIP |VALIDATION" "$LOGD/validate.log" | tail -25
fi

echo "== futexlat: synchronisation numbers on this hardware =="
ssh $SSH_OPTS ec2-user@"$IP" 'cd talaria && docker run --rm -i --device /dev/kvm -v "$PWD":/w talaria-qemu2 sh -c "
  timeout 90 qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 2 -m 512 -overcommit cpu-pm=on \
    -kernel /w/kernel/target/x86_64-talaria/release/talaria \
    -append \"console=ttyS0 talaria.test=futexlat\" -display none -serial file:/tmp/o.log -no-reboot \
    -drive id=d0,file=/w/kernel/disk.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/dev/null 2>&1
  grep -aE \"sync: the wait mode|FUTEXLAT\" /tmp/o.log"' | tee "$LOGD/futexlat.log"

echo "== SUMMARY ($TYPE) — logs in $LOGD =="
grep -a 'TOTAL' "$LOGD/test.log" || echo "test.log has no TOTAL — look at the log"
