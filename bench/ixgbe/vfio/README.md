# VFIO passthrough — the path without a VMM

Goal: give one port of an Intel 82599 (ixgbe) directly to a Talaria cell through
vfio-pci, bypassing the host kernel and QEMU transport, and measure
wire-to-wire over an AOC loop between ports f0 (Talaria) and f1 (a traffic
generator on ixgbe) on one host.

## Steps

1. **BIOS:** enable **VT-d** (Intel VT for Directed I/O) — separate from VT-x.
   Check that the OS sees it: `ls /sys/firmware/acpi/tables/DMAR`.
2. **Host:** `sudo ./setup_vfio.sh`
   - if the IOMMU is not active yet, it edits grub (`intel_iommu=on iommu=pt`)
     and asks for a reboot; after the reboot run it again — it binds `03:00.0`
     to vfio-pci;
   - fallback without VT-d (unsafe, PoC only): `sudo NOIOMMU=1 ./setup_vfio.sh`.
3. **The ixgbe driver in the cell** (Rust, `bench/ixgbe`): the 82599 RX/TX rings
   mapped into the application through VFIO.

## Undo

`sudo bench/ixgbe/vfio/teardown_vfio.sh` gives the port back to ixgbe (or just
reboot — `driver_override` is not persistent).
