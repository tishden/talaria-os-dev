#!/bin/sh
# The arm64 KVM stand in one run (docs/ARM64-PLAN.md §9): bring a `c7g.metal` up with the
# terraform in this directory, provision it (docker, git, binutils, the pinned nightly with
# the aarch64 target), upload the tree, build the arm64 stand image NATIVELY, the kernel and
# the raw Image, run the bring-up gate under KVM and the four probes TCG cannot judge
# (`ring`, `icache`, `bbm`, `svccost`), and destroy the instance unless KEEP=1.
#
#   infra/aws/kvm-stand-aarch64.sh [type] [region]      # default c7g.metal ap-northeast-1
#   KEEP=1 ...                                          # leave the instance up for hand work
#   AZ=ap-northeast-1d,ap-northeast-1a ...              # the zones to try, in order (a metal type is
#                                                       # out of capacity per zone, and the zone with
#                                                       # it changes by the minute — 2026-09-30: 1c
#                                                       # refused, then 1a refused naming 1c and 1d)
#   RINGN=5000000 ...                                   # slots for the ring probe (default 2M)
#   SOAKN=200 ...                                       # boots per mode of the soak (default 50)
#   LOOPN=10 ...                                        # runs of the whole gate in a loop (default 3:
#                                                       # the 229-check gate is ~40 min under KVM)
#   PARN=96 PAR=24 ...                                  # the parallel race detector: PARN boots of
#                                                       # forkap/shmrace/memrace/piperace, PAR at once (96 vCPUs
#                                                       # on 64 cores — §10.5's recipe, default 96/24)
#   UPONLY=1 ...                                        # stop after the build, keep the instance: hand work
#   BISECT="f3f06bc a843638" VARIANTS=dir ABROUNDS=5    # the A/B of a regression: futexlat + svccost on
#                                                       # each commit and each dir/*.patch over HEAD
#   ABONLY=1 ...                                        # the A/B alone: no gate, numbers, soak or loop
#
# The ring-3 set travels as BINARIES: kernel/user/aarch64/ (`make -C kernel/user aarch64` on the
# dev box — the cross musl is there, the stand's Amazon Linux has none) is uploaded next to the
# tree, the kernel embeds it, and the gate builds its ext2 images from it on the stand.
#
# Everything the run prints lands in infra/aws/logs/aarch64-<stamp>/ — the gate's and the
# probes' output is what goes to docs/MEASUREMENTS.md and the plan's §4.13. A metal
# instance takes 10-20 minutes to come up; the run records how long, and the whole
# thing costs about an hour of the instance if nothing is kept.
set -eu
TYPE=${1:-c7g.metal}
REGION=${2:-ap-northeast-1}   # the vCPU quota there fits a 64-core metal
AZS=${AZ:-ap-northeast-1d,ap-northeast-1a,ap-northeast-1c}
AZ=${AZS%%,*}
TFVARS="-var instance_type=$TYPE -var region=$REGION -var arch=arm64 -var az=$AZ"
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
STAMP=$(date +%Y%m%d-%H%M%S)
LOGD="$HERE/logs/aarch64-$STAMP"; mkdir -p "$LOGD"
SSH_OPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=4 -o LogLevel=ERROR -i $HERE/talaria-stand.pem"

# rrun NAME SECS CMD: a LONG step runs on the stand DETACHED and is polled for. An ssh that
# carries a ten-minute build dies with "Broken pipe" when the dev box's network blinks
# (2026-10-02: the kernel build, a minute in — and the exit trap destroyed a stand that was
# doing fine). The command goes into /tmp/NAME.sh, runs under nohup, leaves its exit code
# in /tmp/NAME.rc; the output comes home into $LOGD/NAME.log whatever happened to any one
# connection. Returns the command's exit code (124: SECS ran out).
rrun() {
  name=$1; secs=$2; cmd=$3
  n=0; until printf '%s\n' "$cmd" | ssh $SSH_OPTS ec2-user@"$IP" "cat > /tmp/$name.sh && rm -f /tmp/$name.rc && (nohup sh -c 'sh /tmp/$name.sh > /tmp/$name.log 2>&1; echo \$? > /tmp/$name.rc' >/dev/null 2>&1 &)"; do
    n=$((n+1)); [ $n -gt 5 ] && { echo "rrun $name: could not start it"; return 125; }; sleep 5
  done
  t=0; rc=""
  while [ $t -lt "$secs" ]; do
    rc=$(ssh $SSH_OPTS ec2-user@"$IP" "cat /tmp/$name.rc 2>/dev/null" 2>/dev/null) && [ -n "$rc" ] && break
    rc=""; sleep 10; t=$((t+10))
  done
  n=0; until ssh $SSH_OPTS ec2-user@"$IP" "cat /tmp/$name.log" > "$LOGD/$name.log" 2>/dev/null; do n=$((n+1)); [ $n -gt 5 ] && break; sleep 5; done
  return "${rc:-124}"
}
T0=$(date +%s)

cleanup() {
  if [ "${KEEP:-0}" = 1 ]; then
    echo "KEEP=1: the instance stays up. Destroy it yourself:"
    echo "   terraform -chdir=$HERE destroy -auto-approve $TFVARS"
    return
  fi
  echo "== terraform destroy =="
  terraform -chdir="$HERE" destroy -auto-approve $TFVARS >>"$LOGD/terraform.log" 2>&1 || \
    echo "destroy FAILED — check the console, a metal instance left running is expensive"
}
trap cleanup EXIT

terraform -chdir="$HERE" init -input=false >>"$LOGD/terraform.log" 2>&1
# A capacity refusal is retried by the AWS provider FOREVER ("Still creating..." with no
# instance behind it): each zone gets 6 minutes of apply; if no instance exists by then,
# CloudTrail has the reason (printed), and the next zone of the list is tried.
for AZ in $(echo "$AZS" | tr ',' ' '); do
  TFVARS="-var instance_type=$TYPE -var region=$REGION -var arch=arm64 -var az=$AZ"
  echo "== terraform apply: $TYPE in $REGION/$AZ (metal takes 10-20 min to come up) =="
  if timeout 360 terraform -chdir="$HERE" apply -auto-approve $TFVARS >>"$LOGD/terraform.log" 2>&1; then break; fi
  echo "apply did not finish in 6 min in $AZ — the last RunInstances error from CloudTrail:"
  aws cloudtrail lookup-events --region "$REGION" --lookup-attributes AttributeKey=EventName,AttributeValue=RunInstances --max-results 1 \
    --query 'Events[0].CloudTrailEvent' --output text 2>/dev/null | sed -n 's/.*"errorMessage":"\([^"]*\)".*/   \1/p'
  AZ=""
done
[ -n "$AZ" ] || { echo "no zone of $AZS gave a $TYPE"; exit 1; }
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
# Provisioning is idempotent, so it goes detached too (the second stand of 2026-10-02 lost
# its ssh here, "timed out during banner exchange", and was destroyed for it).
rrun provision 1200 "sudo dnf install -y -q docker git gcc binutils python3 tar e2fsprogs gdb rsync && \
  sudo systemctl enable --now docker && sudo usermod -aG docker ec2-user && sudo chmod 666 /dev/kvm && \
  curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none -q && . \$HOME/.cargo/env && \
  rustup toolchain install $PIN --profile minimal -c rust-src -c clippy >/dev/null && \
  rustup target add aarch64-unknown-none aarch64-unknown-uefi --toolchain $PIN && \
  (sudo modprobe tun; sudo modprobe vhost_net; sudo chmod 666 /dev/vhost-net 2>/dev/null; true)" || { echo "provisioning failed — $LOGD/provision.log"; tail -5 "$LOGD/provision.log"; exit 1; }
T2=$(date +%s); echo "provisioned after $((T2-T1)) s" | tee -a "$LOGD/timing.log"

echo "== uploading the tree (git archive HEAD of the checked-out branch) =="
git -C "$REPO" archive HEAD | ssh $SSH_OPTS ec2-user@"$IP" 'mkdir -p talaria && tar -x -C talaria'
if [ -f "$REPO/kernel/user/aarch64/mhello" ]; then
  echo "== uploading the ring-3 set ($(ls "$REPO/kernel/user/aarch64" | wc -l) binaries from kernel/user/aarch64) =="
  tar -C "$REPO/kernel/user" -c aarch64 | ssh $SSH_OPTS ec2-user@"$IP" 'tar -x -C talaria/kernel/user'
else
  echo "== NO kernel/user/aarch64 here (make -C kernel/user aarch64): the gate will skip ring 3, the soak will not run =="
fi

# The large services' volumes (services/build-aarch64-db.sh on the dev box: the PostgreSQL and
# MariaDB data come from the x86 builds there) travel as sparse files, compressed.
DBIMGS=$(cd "$REPO/kernel/target" 2>/dev/null && ls a64-services/ext2-pg.img a64-services/ext2-mdb.img a64-services/ext2-mongo.img a64-services/ext2-jvm.img a64-services/ext2-ch.img 2>/dev/null | tr '\n' ' ')
if [ -n "$DBIMGS" ]; then
  # A FILE AND A RESUMABLE COPY, not a pipe: the first try streamed ~600 MB of tar through one
  # ssh, the connection blinked ("client_loop: send disconnect: Broken pipe") and `set -e`
  # destroyed a stand that had been provisioned for 40 minutes. A failed upload now costs the
  # db step only.
  echo "== uploading the services' volumes: $DBIMGS =="
  DBTGZ="$REPO/kernel/target/a64-services/volumes.tgz"
  tar -C "$REPO/kernel/target" -cSzf "$DBTGZ" $DBIMGS
  n=0; until rsync -q --partial --append-verify -e "ssh $SSH_OPTS" "$DBTGZ" ec2-user@"$IP":volumes.tgz; do
    n=$((n+1)); [ $n -ge 6 ] && { echo "the volumes did not upload — the db step is skipped"; DBIMGS=""; break; }; sleep 10
  done
  [ -n "$DBIMGS" ] && { ssh $SSH_OPTS ec2-user@"$IP" 'mkdir -p talaria/kernel/target && tar -xSzf volumes.tgz -C talaria/kernel/target && rm -f volumes.tgz' \
    || { echo "the volumes did not unpack — the db step is skipped"; DBIMGS=""; }; }
fi

echo "== the arm64 stand image, built natively (fedora:40 pulls its arm64 variant) =="
rrun image 1200 'cd talaria && docker build -t talaria-qemu-aarch64 -f services/docker/Dockerfile.qemu-aarch64 services/docker' || { echo "the image did not build — $LOGD/image.log"; tail -5 "$LOGD/image.log"; exit 1; }

echo "== the kernel and the raw Image (native objcopy) =="
rrun build 1800 '. $HOME/.cargo/env && cd talaria && AARCH64_OBJCOPY=objcopy VDSO_CC_AARCH64=gcc kernel/build-aarch64.sh' || { echo "the kernel did not build — $LOGD/build.log"; tail -8 "$LOGD/build.log"; exit 1; }
T3=$(date +%s); echo "built after $((T3-T2)) s" | tee -a "$LOGD/timing.log"

# The UEFI stub carries the kernel inside: after the kernel, before the gate (its UEFI block
# is skipped without a fresh stub).
rrun stub 900 '. $HOME/.cargo/env && cd talaria && kernel/uefi-stub/build.sh aarch64' || { echo "the UEFI stub did not build — $LOGD/stub.log (the gate will skip its UEFI block)"; tail -5 "$LOGD/stub.log"; }

if [ "${UPONLY:-0}" = 1 ]; then
  KEEP=1
  echo "UPONLY=1: built and kept. ssh $SSH_OPTS ec2-user@$IP"
  exit 0
fi

if [ "${ABONLY:-0}" != 1 ]; then
echo "== the gate under KVM (TAL_KVM=1: the timing lines are judged too) =="
rrun gate 3600 'cd talaria && TAL_KVM=1 kernel/test-aarch64.sh' || true
grep -E 'FAIL|TOTAL|SKIP' "$LOGD/gate.log" || true
# The consoles of the boots that failed a check come home too.
ssh $SSH_OPTS ec2-user@"$IP" 'tar -C talaria/kernel/target -c a64-fail 2>/dev/null' | tar -x -C "$LOGD" 2>/dev/null || true

# The large services under KVM (kernel/test-aarch64-db.sh, validate.sh B10-B11's arm64 twin):
# with the product's default wait (a mixed epoll set spins) and with `talaria.netmix=`.
if [ -n "$DBIMGS" ]; then
  echo "== the large services under KVM: PostgreSQL, MariaDB, MongoDB, the JVM (default wait, then netmix) =="
  rrun db 3600 'cd talaria && for app in "" "talaria.netmix=1000000"; do echo "## TAL_DBAPP=$app"; \
    docker run --rm --device /dev/kvm --device /dev/net/tun --cap-add NET_ADMIN -v "$PWD:/w" -w /w -e TAL_KVM=1 \
      -e TAL_ACCEL="-enable-kvm -cpu host" -e TAL_DBAPP="$app" talaria-qemu-aarch64 bash kernel/test-aarch64-db.sh; done' || true
  grep -E "##|PASS|FAIL|TOTAL" "$LOGD/db.log" | cut -c1-200 || true
  ssh $SSH_OPTS ec2-user@"$IP" 'tar -C talaria/kernel/target -c a64-fail-db 2>/dev/null' | tar -x -C "$LOGD" 2>/dev/null || true
fi

# The network numbers: the guest on a tap with vhost, the meters (the arm64 builds of
# udp_ping/tcp_bench) next to it in the stand image.
echo "== the network numbers (kernel/netnum-aarch64.sh: udp_ping against userver, tcp_bench against diskserver) =="
rrun netnum 900 'cd talaria && docker run --rm --device /dev/kvm --device /dev/net/tun $( [ -c /dev/vhost-net ] && echo --device /dev/vhost-net ) --cap-add NET_ADMIN -v "$PWD:/w" -w /w -e TAL_ACCEL="-enable-kvm -cpu host" talaria-qemu-aarch64 bash kernel/netnum-aarch64.sh' || true
grep -E "netnum|udp \[|tcp \[" "$LOGD/netnum.log" | cut -c1-200 || true

# The numbers of the set (docs/MEASUREMENTS.md): the whole console of each, three boots —
# `mbench` is the twin of the x86 scoreboard's primitives, `futexlat` the wakeup latency,
# `smp2`/`smp4` the parallel speedup. The ext2 image is the one the gate has just built.
echo "== the numbers: mbench, futexlat, smp2, smp4 (three boots each, the consoles kept) =="
DISKARG='-drive id=d0,file=/w/kernel/target/a64-disks/ext2.img,format=raw,if=none -device virtio-blk-device,drive=d0'
for n in 1 2 3; do
  for p in mbench futexlat smp2 smp4; do
    extra=""; [ "$p" = mbench ] && extra="$DISKARG"
    ssh $SSH_OPTS ec2-user@"$IP" "cd talaria && TAL_APPEND='console=ttyAMA0 talaria.test=$p talaria.smp=4 talaria.poweroff=1' kernel/runq-aarch64.sh 120 -smp 4 $extra" \
      > "$LOGD/num-$p-$n.log" 2>&1 || true
    grep -aE "RESULT mbench|FUTEXLAT|SMP[24]: (t1|the parallel)|\[trap\]|PANIC" "$LOGD/num-$p-$n.log" | cut -c1-160 | sed "s/^/   [$n] /"
  done
done

fi # ABONLY

# The A/B of a regression (BISECT/VARIANTS): each commit of BISECT and each patch of VARIANTS
# (applied over HEAD) is built in a tree of its own, then `futexlat` and `svccost` boot
# ABROUNDS rounds, the variants in turn inside a round — a drift of the stand lands on all of
# them alike. The table is ab.log's `AB` lines.
if [ -n "${BISECT:-}${VARIANTS:-}" ]; then
  echo "== the A/B: ${BISECT:-} ${VARIANTS:+patches from $VARIANTS} (${ABROUNDS:-5} rounds) =="
  names=""
  for r in ${BISECT:-}; do
    git -C "$REPO" archive "$r" | ssh $SSH_OPTS ec2-user@"$IP" "mkdir -p ab/$r && tar -x -C ab/$r && cp -a talaria/kernel/user/aarch64 ab/$r/kernel/user/"
    names="$names $r"
  done
  for p in ${VARIANTS:+$VARIANTS/*.patch}; do
    v=$(basename "$p" .patch)
    ssh $SSH_OPTS ec2-user@"$IP" "mkdir -p ab/$v && tar -C talaria --exclude=./kernel/target -c . | tar -x -C ab/$v && cd ab/$v && git apply" < "$p" || { echo "variant $v: the patch did not apply"; continue; }
    names="$names $v"
  done
  rrun ab 3600 ". \$HOME/.cargo/env; built=''; for v in $names; do \
      (cd ab/\$v && AARCH64_OBJCOPY=objcopy VDSO_CC_AARCH64=gcc kernel/build-aarch64.sh > build.log 2>&1) && built=\"\$built \$v\" || echo \"AB \$v: the build FAILED\"; done; \
    for n in \$(seq 1 ${ABROUNDS:-5}); do for v in \$built; do \
      o=\$(cd ab/\$v && TAL_APPEND='console=ttyAMA0 talaria.test=futexlat talaria.smp=4 talaria.poweroff=1' kernel/runq-aarch64.sh 120 -smp 4 2>&1 | grep -a 'FUTEXLAT .*RTT' | sed 's/.*FUTEXLAT \\([a-z]*\\) \\([a-z-]*\\): RTT p50=\\([0-9]*\\).*/\\1-\\2=\\3/' | tr '\\n' ' '); \
      s=\$(cd ab/\$v && TAL_APPEND='console=ttyAMA0 talaria.test=svccost talaria.smp=4 talaria.poweroff=1' kernel/runq-aarch64.sh 120 -smp 4 2>&1 | grep -a 'WITH the FP' | grep -ao '[0-9]* ns per' | head -1); \
      echo \"AB \$v round \$n: \$o svc=\$s\"; done; done" || true
  grep -a '^AB' "$LOGD/ab.log" || true
fi

if [ "${ABONLY:-0}" = 1 ]; then T4=$(date +%s); echo "ABONLY=1: the A/B alone, done after $((T4-T0)) s"; echo "logs: $LOGD"; exit 0; fi

echo "== the probes TCG cannot judge (docs/ARM64-PLAN.md §9.2 B-E) =="
for p in "ring talaria.ringn=${RINGN:-2000000}" icache bbm svccost; do
  name=${p%% *}
  echo "-- talaria.test=$p"
  ssh $SSH_OPTS ec2-user@"$IP" "cd talaria && TAL_APPEND='console=ttyAMA0 talaria.test=$p talaria.smp=4 talaria.poweroff=1' kernel/runq-aarch64.sh 120 -smp 4" \
    2>&1 | tee "$LOGD/probe-$name.log" | grep -E "probe|as expected|NOT|corrupt|stale|conflict|ns per|\[trap\]|PANIC" | cut -c1-200
done

# The soak BEFORE anything can destroy the instance: the modes whose verdict depends on what
# two cores see of each other's writes, SOAKN boots each; the failing consoles come home.
echo "== the soak (kernel/soak-aarch64.sh: ${SOAKN:-50} boots of each mode under KVM) =="
rrun soak 5400 "cd talaria && kernel/soak-aarch64.sh ${SOAKN:-50} /tmp/soak" || true
tail -15 "$LOGD/soak.log" || true
ssh $SSH_OPTS ec2-user@"$IP" 'tar -C /tmp -c soak 2>/dev/null' | tar -x -C "$LOGD" 2>/dev/null || echo "the soak's consoles did not come home (the verdict lines above are what is left)"

# The parallel race detector of §10.5: PAR copies of one boot at once, more vCPUs than the
# host has cores — the three races of the shared code that session found never showed on a
# quiet host and did in 1 of 60-160 boots this way.
echo "== the parallel race detector: ${PARN:-96} boots of forkap/shmrace/memrace/piperace/futexlat/syncbench, ${PAR:-24} at a time =="
rrun par 2400 "cd talaria && PAR=${PAR:-24} MODES='forkap shmrace memrace piperace futexlat syncbench' kernel/soak-aarch64.sh ${PARN:-96} /tmp/par" || true
grep -E 'soak:' "$LOGD/par.log" || true
ssh $SSH_OPTS ec2-user@"$IP" 'tar -C /tmp -c par 2>/dev/null' | tar -x -C "$LOGD" 2>/dev/null || true

LOOPN=${LOOPN:-3}
echo "== $LOOPN runs of the whole gate in a loop (the SMP paths under a real weak machine) =="
rrun loop 7200 "cd talaria && fails=0; for i in \$(seq 1 $LOOPN); do TAL_KVM=1 kernel/test-aarch64.sh > /tmp/g.log 2>&1 || { fails=\$((fails+1)); cp /tmp/g.log /tmp/g-fail-\$i.log; }; done; echo \"loop: \$fails of $LOOPN gate runs failed\"; grep -h FAIL /tmp/g-fail-*.log 2>/dev/null | sort | uniq -c | head -20" || true
cat "$LOGD/loop.log" || true
T4=$(date +%s); echo "all done after $((T4-T0)) s; the stand ran $((T4-T1)) s" | tee -a "$LOGD/timing.log"
echo "logs: $LOGD"
