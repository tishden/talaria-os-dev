#!/bin/bash
# Bring the second 82599 port (f1, kernel ixgbe) up as the partner for wire-to-wire.
set -e
IF=${IF:-enp3s0f1}
F0_MAC=${F0_MAC:-90:e2:ba:ed:3d:34}
ip addr flush dev "$IF"
ip addr add 10.0.0.1/24 dev "$IF"
ip link set "$IF" up
ip neigh replace 10.0.0.2 lladdr "$F0_MAC" dev "$IF"
sysctl -w net.ipv4.conf.$IF.rp_filter=0
echo F1_READY
