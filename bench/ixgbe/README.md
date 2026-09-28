# `talaria-ixgbe` — a userspace datapath on a live Intel 82599 (VFIO)

An 82599 driver and a frame-processing pipeline that run **from userspace on
bare Linux** through VFIO — no OS kernel on the hot path and no virtualisation.

## Why this is in the Talaria repository

It is **the second half of the proof**. Talaria's numbers under QEMU hit a floor
created by QEMU itself: a virtio kick costs a VM exit (~4 µs), and such
measurements cannot separate the cost of the OS from the cost of the
hypervisor. Here the same pipeline runs on a real card without a single VM
exit, which shows what it really costs:

| measurement | QEMU (virtio) | 82599 (VFIO) |
|---|---:|---:|
| rx -> tx, minimum | ~4 µs | **14 ns** |
| rx -> tx, average | ~4 µs | **52 ns** |
| share of packets < 100 ns | — | 92–94 % |

So the microseconds under QEMU were **entirely** the virtio kick, not the work
of the code. Full analysis and method: `docs/BENCH-B.md` §8-§9.

## What is inside

| file | what it does |
|---|---|
| `vfio.rs` | VFIO plumbing (including noiommu mode), BAR mapping, DMA |
| `dma.rs` | a 1 GiB hugepage as a physically contiguous DMA area |
| `ixgbe.rs` | the 82599 driver itself: RX/TX rings, descriptors, doorbells in the BAR |
| `reflect.rs` | an in-place frame reflector: ARP, ICMP echo, UDP echo :7777 |
| `l1pipe.rs` | the same stage on the full smoltcp stack |
| `spsc.rs` | a lock-free SPSC ring passing buffer ownership between shards |
| `crypto.rs` | AES-128-GCM on AES-NI + PCLMULQDQ, encryption in place |
| `trade.rs` | a demonstration strategy + risk pipeline: the order is built in the buffer of the received market data |
| `resctrl.rs` | L3 partitioning through resctrl (Intel CAT) |
| `pmu.rs` | dTLB/LLC counters via `perf_event_open` + `rdpmc` |
| `proxy.rs`, `shm.rs` | a syscall proxy for cold operations and telemetry over shared memory |
| `vfio/` | host preparation: binding the port to `vfio-pci`, hugepages, the peer port |

## Running

Needs root and two 82599 ports connected by a cable (f0 goes to VFIO, f1 stays
a kernel interface and acts as the traffic generator).

```sh
sudo bench/ixgbe/hw-bench.sh           # detects the ports, full run
sudo bench/ixgbe/hw-bench.sh cleanup   # give f0 back to the ixgbe driver
```

The script binds f0 to VFIO (noiommu), brings f1 up at 10.0.0.1, reserves 1 GB
hugepages, starts the cell and measures RTT **with the same `udp_ping`** as the
runs under QEMU — otherwise the numbers would not be comparable.

By hand (PCI address and group through the environment):

```sh
TALARIA_PCI=0000:03:00.0 TALARIA_VFIO_GROUP=42 \
  sudo ./target/release/talaria-ixgbe cell --core 3 --fifo
```

Modes: `cell` (the reflector, default), `link` (link check), `crypto` (AES-GCM
microbenchmark). Options: `--core N` and `--fifo` pin to a core with
SCHED_FIFO, `--l1` uses the smoltcp stack instead of the reflector, `--trade`
the trading pipeline, `--pipeline N` sharding, `--llc-mask 0x...` an L3 share
through CAT, `--numa N`, `--shm <path>` telemetry, `--proxy <path>` the syscall proxy.

## A warning about noiommu

`setup_vfio.sh` has a `NOIOMMU=1` mode — without VT-d. In it the device writes
DMA to physical addresses **without IOMMU protection**: a driver bug corrupts
any memory of the machine. That is acceptable on a test bench and nowhere
else; the proper path is to enable VT-d in the BIOS (`intel_iommu=on iommu=pt`).

## Boundaries

The driver is deliberately minimal: one RX/TX queue pair, no RSS, no power
management, no link error handling beyond the necessary. It is a measuring
instrument and a reference datapath, not a production NIC driver.
