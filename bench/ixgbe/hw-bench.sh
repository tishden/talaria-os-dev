#!/bin/bash
# A turnkey run on a real 82599: bind to VFIO (noiommu) + the peer port as a
# kernel interface + start the reflect cell + measure RTT from the host.
# Needs root. Two 82599 ports joined by one AOC cable (f0<->f1).
#   sudo ./hw-bench.sh            — auto-detect the ports and run
#   sudo ./hw-bench.sh cleanup    — give f0 back to ixgbe
# Env: NIC0/NIC1 — the ports' PCI addresses (otherwise auto-detected by 8086:10fb).
set -u
DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="$DIR/target/release/talaria-ixgbe"
PING="$DIR/../../kernel/user/udp_ping"
DEV_ID="8086:10fb"   # 82599ES; with a different NIC, set NIC0/NIC1 yourself

[ "$(id -u)" = 0 ] || { echo "root is needed (sudo)"; exit 1; }

detect_ports() {
  if [ -n "${NIC0:-}" ] && [ -n "${NIC1:-}" ]; then echo "$NIC0 $NIC1"; return; fi
  mapfile -t P < <(lspci -Dn | awk -v id="$DEV_ID" '$3==id{print $1}')
  [ "${#P[@]}" -ge 2 ] || { echo "did not find 2 ports of $DEV_ID (set NIC0/NIC1)"; exit 1; }
  echo "${P[0]} ${P[1]}"
}

read -r P0 P1 <<< "$(detect_ports)"

if [ "${1:-}" = cleanup ]; then
  echo "cleanup: giving $P0 back to ixgbe"
  echo "" > /sys/bus/pci/devices/$P0/driver_override 2>/dev/null
  echo "$P0" > /sys/bus/pci/drivers/vfio-pci/unbind 2>/dev/null
  echo "$P0" > /sys/bus/pci/drivers/ixgbe/bind 2>/dev/null
  echo "done"; exit 0
fi

echo "== ports: f0(VFIO)=$P0  f1(kernel/traffic)=$P1 =="

# 1) VFIO noiommu bind of f0
modprobe vfio-pci 2>/dev/null
echo 1 > /sys/module/vfio/parameters/enable_unsafe_noiommu_mode 2>/dev/null
if [ -e /sys/bus/pci/devices/$P0/driver ]; then
  echo "$P0" > /sys/bus/pci/devices/$P0/driver/unbind 2>/dev/null
fi
echo vfio-pci > /sys/bus/pci/devices/$P0/driver_override
echo "$P0" > /sys/bus/pci/drivers/vfio-pci/bind 2>/dev/null
GROUP=$(basename "$(readlink /sys/bus/pci/devices/$P0/iommu_group)")
[ -e "/dev/vfio/noiommu-$GROUP" ] || { echo "no /dev/vfio/noiommu-$GROUP — the bind failed"; exit 1; }
echo "VFIO: group $GROUP, /dev/vfio/noiommu-$GROUP ready"

# 2) the peer port f1 as a kernel interface with IP 10.0.0.1
IF1=$(ls /sys/bus/pci/devices/$P1/net/ 2>/dev/null | head -1)
[ -n "$IF1" ] || { echo "$P1 has no netdev (not on ixgbe?) — bind it to ixgbe"; exit 1; }
ip addr flush dev "$IF1" 2>/dev/null
ip addr add 10.0.0.1/24 dev "$IF1"; ip link set "$IF1" up
echo "peer: $IF1 = 10.0.0.1/24 (up)"

# 3) 1G hugepages (needed for the DMA region)
HP=/sys/kernel/mm/hugepages/hugepages-1048576kB/nr_hugepages
if [ -e "$HP" ]; then
  cur=$(cat "$HP"); [ "$cur" -ge 1 ] || echo 2 > "$HP"
  echo "1G hugepages: $(cat "$HP") (if 0 — add hugepagesz=1G hugepages=2 to the cmdline)"
else
  echo "WARNING: 1G hugepages are unavailable — add hugepagesz=1G hugepages=2 to the boot line"
fi

# 4) start the reflect cell on a dedicated core
echo "== starting the cell (reflect :7777) on core 2 =="
[ -x "$BIN" ] || { echo "no binary $BIN — build it: (cd bench/ixgbe && cargo build --release)"; exit 1; }
TALARIA_PCI=$P0 TALARIA_VFIO_GROUP=$GROUP taskset -c 2 "$BIN" cell --core 2 > /tmp/cell.log 2>&1 &
CELL=$!
# wait for the link (10G takes a few seconds to come up)
echo -n "waiting for the cell's link: "
for i in $(seq 1 20); do
  grep -q "UP (10G)" /tmp/cell.log && { echo "UP"; break; }
  grep -q "DOWN" /tmp/cell.log && { echo "DOWN (see /tmp/cell.log)"; break; }
  kill -0 $CELL 2>/dev/null || { echo "THE CELL DIED"; break; }
  sleep 1; echo -n "."
done
echo "--- cell log ---"; grep -iE "device|mac|UP|DOWN|hugepage|cell" /tmp/cell.log | head -6
MAC=$(grep -oE 'mac [0-9a-f:]+' /tmp/cell.log | awk '{print $2}' | head -1)
if [ -n "$MAC" ]; then ip neigh replace 10.0.0.2 lladdr "$MAC" dev "$IF1"; echo "static ARP: 10.0.0.2 -> $MAC"; fi
# wait for the peer link to come up (it depends on f0)
for i in $(seq 1 10); do ip link show "$IF1" | grep -q "state UP" && break; sleep 1; done
ip -br link show "$IF1"

# 5) measure RTT from the host through f1 (the same tool as for QEMU)
echo "== measuring RTT (real hardware, no VMEXIT) =="
if [ -x "$PING" ]; then
  "$PING" 10.0.0.2 7777 200000
else
  echo "no $PING — build udp_ping (see kernel/user)"
fi

echo "== cell telemetry (rx->tx from the inside, TSC) =="
grep -iE "rx->tx|pkts|ns|min/avg" /tmp/cell.log | tail -3

kill $CELL 2>/dev/null
echo ""
echo "Done. Compare the RTT with docs/MEASUREMENTS.md §5 (QEMU): on hardware the VMEXIT floor disappears."
echo "To give the port back to ixgbe: sudo $0 cleanup"
