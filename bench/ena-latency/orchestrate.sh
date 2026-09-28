#!/bin/sh
# Orchestration of the ENA benchmark (after terraform apply in ./terraform): assign
# the secondary IP, upload the tree, bring the guests up, run the scenarios of README §3,
# collect the logs. Run from the working machine; AWS credentials in the environment.
# SKELETON: scenarios 1-2 (RTT) are ready; 3 (one-way/tcpdump) is a stub.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
TF="$HERE/terraform"
LOGD="$HERE/logs/run-$(date +%Y%m%d-%H%M)"
mkdir -p "$LOGD" "$HERE/logs"
SSH="ssh -i $TF/talaria-ena.pem -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"

IPS=$(terraform -chdir="$TF" output -json public_ips | python3 -c 'import json,sys;print(" ".join(json.load(sys.stdin)))')
ENIS=$(terraform -chdir="$TF" output -json eni_ids | python3 -c 'import json,sys;print(" ".join(json.load(sys.stdin)))')
set -- $IPS; A=$1; B=$2
set -- $ENIS; ENIA=$1; ENIB=$2
REGION=$(terraform -chdir="$TF" output -raw region 2>/dev/null || echo ap-northeast-1)

echo "== secondary IP for the guests =="
for eni in $ENIA $ENIB; do
  aws ec2 assign-private-ip-addresses --region "$REGION" --network-interface-id "$eni" \
    --secondary-private-ip-address-count 1 >/dev/null 2>&1 || true
done
GA=$(aws ec2 describe-network-interfaces --region "$REGION" --network-interface-ids "$ENIA" \
  --query 'NetworkInterfaces[0].PrivateIpAddresses[?Primary==`false`].PrivateIpAddress | [0]' --output text)
GB=$(aws ec2 describe-network-interfaces --region "$REGION" --network-interface-ids "$ENIB" \
  --query 'NetworkInterfaces[0].PrivateIpAddresses[?Primary==`false`].PrivateIpAddress | [0]' --output text)
echo "guest A: $GA (host $A), guest B: $GB (host $B)"

echo "== provisioning + uploading the tree =="
for ip in $A $B; do
  $SSH ec2-user@"$ip" 'sudo dnf install -y -q docker git python3 tar && sudo systemctl enable --now docker && sudo usermod -aG docker ec2-user' >>"$LOGD/provision.log" 2>&1
  git -C "$REPO" archive HEAD | $SSH ec2-user@"$ip" 'mkdir -p talaria && tar -x -C talaria'
  $SSH ec2-user@"$ip" 'cd talaria && ./services/docker/build-images.sh' >>"$LOGD/provision.log" 2>&1
done
# the kernel is built locally and uploaded (or build it on the spot — see validate-remote.sh)
for ip in $A $B; do
  $SSH ec2-user@"$ip" 'mkdir -p talaria/kernel/target/x86_64-talaria/release talaria/bench/ena-latency/logs'
  scp -i "$TF/talaria-ena.pem" -o StrictHostKeyChecking=no -q \
    "$REPO/kernel/target/x86_64-talaria/release/talaria" \
    ec2-user@"$ip":talaria/kernel/target/x86_64-talaria/release/
done

echo "== host networking =="
$SSH ec2-user@"$A" "cd talaria && ./bench/ena-latency/setup-net.sh $GA" | tee "$LOGD/net-A.log"
$SSH ec2-user@"$B" "cd talaria && ./bench/ena-latency/setup-net.sh $GB" | tee "$LOGD/net-B.log"
echo "-- baseline: host-to-host RTT --"
$SSH ec2-user@"$A" "ping -c 20 -i 0.05 -q $(echo $B)" | tail -2 | tee "$LOGD/host-rtt.log" || true

echo "== disks: B=echo servers (userver UDP + epsrv TCP), A=the udp_ping client =="
$SSH ec2-user@"$B" "cd talaria && python3 kernel/tools/talaria-pack -o /tmp/srv.img --app kernel/user/userver --restart 0 && cp /tmp/srv.img bench/ena-latency/srv.img"
$SSH ec2-user@"$A" "cd talaria && python3 kernel/tools/talaria-pack -o /tmp/cli.img --app kernel/user/udp_ping --args=\"$GB 9998 100000\" --restart 0 && cp /tmp/cli.img bench/ena-latency/cli.img"

echo "== scenario 1: UDP RTT guest(A) -> guest(B) =="
$SSH ec2-user@"$B" "cd talaria && nohup ./bench/ena-latency/run-talaria.sh bench/ena-latency/srv.img $GB 10.200.0.1 >/dev/null 2>&1 &"
sleep 20
$SSH ec2-user@"$A" "cd talaria && timeout 300 ./bench/ena-latency/run-talaria.sh bench/ena-latency/cli.img $GA 10.200.0.1" || true
$SSH ec2-user@"$A" 'tail -20 talaria/bench/ena-latency/logs/serial-*.log' | tee "$LOGD/udp-rtt.log"

echo "== TODO: scenario 2 (TCP RTT: tcp_ping/epsrv), scenario 3 (one-way: tcpdump tap+ena + chronyc), scenario 5 (the Linux baseline) =="
echo "logs: $LOGD; the instances were NOT destroyed — run terraform -chdir=$TF destroy when done!"
