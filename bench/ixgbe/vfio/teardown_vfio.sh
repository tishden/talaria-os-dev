#!/bin/bash
# Give the 82599 port back to the kernel's ixgbe.
set -euo pipefail
DEV=${DEV:-0000:03:00.0}
echo > /sys/bus/pci/devices/$DEV/driver_override
CUR=$(basename "$(readlink -f /sys/bus/pci/devices/$DEV/driver 2>/dev/null || echo none)")
[ "$CUR" = vfio-pci ] && echo "$DEV" > /sys/bus/pci/devices/$DEV/driver/unbind
echo "$DEV" > /sys/bus/pci/drivers_probe
echo ">> $DEV returned to driver: $(basename "$(readlink -f /sys/bus/pci/devices/$DEV/driver 2>/dev/null || echo none)")"
