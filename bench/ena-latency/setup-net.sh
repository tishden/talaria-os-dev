#!/bin/sh
# Runs ON the metal host (AL2023): prepares the network for a Talaria guest.
# Argument: GUEST_IP (a secondary private IP of this instance — the guest's address in the VPC).
# A scheme without NAT: tap0 + a /32 route + proxy_arp + ip_forward (README §1).
set -eu
GUEST_IP=${1:?usage: setup-net.sh <guest-ip>}
ENA=$(ip -o -4 route show default | awk '{print $5}' | head -1)

sudo dnf install -y -q iproute-tc chrony ethtool tcpdump iptables >/dev/null 2>&1 || true

# a tap for QEMU (user ec2-user)
sudo ip tuntap add tap0 mode tap user ec2-user 2>/dev/null || true
sudo ip addr add 10.200.0.1/32 dev tap0 2>/dev/null || true
sudo ip link set tap0 up
sudo ip route replace "$GUEST_IP/32" dev tap0

# forwarding + proxy_arp: the VPC delivers the secondary IP to the instance, the host into the tap
sudo sysctl -qw net.ipv4.ip_forward=1 net.ipv4.conf.all.proxy_arp=1 \
  "net.ipv4.conf.$ENA.proxy_arp=1" net.ipv4.conf.tap0.proxy_arp=1
# ARP replies for the guest on the ENA side are not needed (VPC is L3), rp_filter is relaxed
sudo sysctl -qw net.ipv4.conf.all.rp_filter=0 "net.ipv4.conf.$ENA.rp_filter=0" \
  net.ipv4.conf.tap0.rp_filter=0

# less jitter on the path: no GRO/LRO on the tap, no rps
sudo ethtool -K tap0 gro off 2>/dev/null || true

# clocks: chrony on AWS Time Sync; PHC if ENA provides it
if [ -e /dev/ptp0 ]; then
  echo 'refclock PHC /dev/ptp0 poll 0 delay 0.000010 prefer' | sudo tee -a /etc/chrony.conf >/dev/null
  echo "PHC: /dev/ptp0 found — sub-microsecond synchronisation"
else
  echo "PHC: no /dev/ptp0 — chrony over NTP 169.254.169.123 (tens of µs; one-way with a caveat)"
fi
sudo systemctl enable --now chronyd >/dev/null 2>&1 || sudo systemctl restart chronyd
sleep 2
chronyc tracking | sed -n '1,6p'
echo "setup-net: OK (ena=$ENA guest=$GUEST_IP)"
