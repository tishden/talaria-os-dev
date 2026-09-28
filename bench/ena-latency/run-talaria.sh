#!/bin/sh
# Runs ON the metal host: start a Talaria guest on tap0 with a guest IP from the VPC.
# Arguments: DISK GUEST_IP GW_IP [extra-append]
# The disk is built with talaria-pack (server role: userver/epsrv; client: udp_ping).
# netsys is configured from the cmdline: talaria.ip=A.B.C.D/P talaria.gw= (netsys.rs,
# the configuration section). For the secondary-IP scheme: ip=<GIP>/32, gw=10.200.0.1 (the
# address of the host's tap0; proxy_arp on tap0 answers for the gateway). The exact mask comes from the run.
set -eu
DISK=${1:?disk}; GIP=${2:?guest-ip}; GW=${3:?gw-ip}; EXTRA=${4:-}
cd "$(dirname "$0")/../.."   # the root of the talaria tree
exec docker run --rm -i --device /dev/kvm --device /dev/net/tun --cap-add NET_ADMIN \
  --network host -v "$PWD":/w talaria-qemu2 sh -c "
  qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu host -smp 2 -m 1024 \
    -overcommit cpu-pm=on \
    -kernel /w/kernel/target/x86_64-talaria/release/talaria \
    -append 'console=ttyS0 talaria.mode=cell talaria.ip=$GIP/32 talaria.gw=$GW $EXTRA' \
    -display none -serial file:/w/bench/ena-latency/logs/serial-\$\$.log -no-reboot \
    -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
    -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
    -drive id=d0,file=/w/$DISK,format=raw,if=none -device virtio-blk-device,drive=d0"
