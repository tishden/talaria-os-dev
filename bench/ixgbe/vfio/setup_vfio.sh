#!/bin/bash
# Prepares the host for VFIO passthrough of one Intel 82599 (ixgbe) port into a
# Talaria cell. Idempotent, run as root.
#   Phase 1 (IOMMU off): patches grub (intel_iommu=on iommu=pt) -> asks for a reboot.
#   Phase 2 (IOMMU on):  unbinds 03:00.0 from ixgbe and binds it to vfio-pci.
# The second port (03:00.1) stays on ixgbe as the traffic generator.
set -euo pipefail

DEV=${DEV:-0000:03:00.0}      # the port for Talaria
NOIOMMU=${NOIOMMU:-0}         # 1 = allow vfio without an IOMMU (unsafe, for a PoC)

iommu_active() { [ -e /sys/class/iommu/dmar0 ] || [ -n "$(ls -A /sys/kernel/iommu_groups 2>/dev/null)" ]; }

if ! iommu_active && [ "$NOIOMMU" != 1 ]; then
    echo ">> IOMMU is not active."
    if [ ! -e /sys/firmware/acpi/tables/DMAR ]; then
        echo "!! No ACPI DMAR table — VT-d is disabled in the BIOS. Enable VT-d"
        echo "   (Intel VT for Directed I/O), it is separate from VT-x, and boot again."
        echo "   If the BIOS cannot do VT-d — rerun as: NOIOMMU=1 $0"
        exit 2
    fi
    if ! grep -q intel_iommu=on /etc/default/grub; then
        cp /etc/default/grub /etc/default/grub.talaria.bak
        sed -i 's/\(GRUB_CMDLINE_LINUX="\)/\1intel_iommu=on iommu=pt /' /etc/default/grub
        for cfg in /boot/grub2/grub.cfg /boot/efi/EFI/*/grub.cfg; do
            [ -e "$cfg" ] && grub2-mkconfig -o "$cfg"
        done
        echo ">> Added intel_iommu=on iommu=pt to grub. REBOOT and run again."
    else
        echo ">> grub is already patched, but the IOMMU is not active — a reboot is needed."
    fi
    exit 3
fi

echo ">> IOMMU active (or NOIOMMU=1)."
modprobe vfio-pci
[ "$NOIOMMU" = 1 ] && { echo 1 > /sys/module/vfio/parameters/enable_unsafe_noiommu_mode 2>/dev/null || true; }

CUR=$(basename "$(readlink -f /sys/bus/pci/devices/$DEV/driver 2>/dev/null || echo none)")
if [ "$CUR" = vfio-pci ]; then
    echo ">> $DEV is already on vfio-pci."
else
    echo vfio-pci > /sys/bus/pci/devices/$DEV/driver_override
    [ "$CUR" != none ] && echo "$DEV" > /sys/bus/pci/devices/$DEV/driver/unbind
    echo "$DEV" > /sys/bus/pci/drivers_probe
    echo ">> $DEV bound to vfio-pci."
fi

GRP=$(basename "$(readlink -f /sys/bus/pci/devices/$DEV/iommu_group 2>/dev/null || echo '?')")
echo ">> IOMMU group: $GRP; devices in the group:"
ls /sys/bus/pci/devices/$DEV/iommu_group/devices 2>/dev/null | sed 's/^/     /'
echo ">> /dev/vfio:"; ls -l /dev/vfio/ 2>/dev/null | sed 's/^/     /'
echo ">> Done. Port 03:00.1 stays on ixgbe (the traffic generator)."
