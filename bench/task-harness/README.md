# task-harness — the "Low-latency data transfer" challenge on Talaria vs Linux (two VMs)

The harness of a low-latency transport challenge (producer → sender → network →
receiver → consumer; the fixed ends measure latency by `CLOCK_REALTIME`, shared
memory rings from `shm_ring.h`) reproduced with the "middle" in two VMs on one
host. The same static musl binaries `sender`/`receiver` (the `udp` backend,
the FEC/NACK solution unchanged) run either on the **Talaria** kernel or on
**Linux** (the host's vmlinuz). The ends run on the host (one clock source, as
in the solution's loopback baseline); their rings are given to the guests
through **ivshmem** (host shared memory = PCI BAR2; on Talaria `/dev/shm/<name>`
maps to the BAR, see `pci.rs`).

The challenge sources are not part of this repository; the scripts expect them
in `$TASK` (default `~/sources/task`).

```
host: producer --(shm /fanout_ring = ivshmem)--> [VM1 sender] --UDP tap/bridge--> [VM2 receiver] --(ivshmem = /fanout_rx shm)--> consumer: host
```

Files:
- `build_musl.sh` — static build of sender/receiver (g++ from
  `talaria-alpine-build`, base `0x10000000`) → `task/transport/bin-musl/`.
- `run_case_vm.sh RATE LOSS COUNT TYPE OUT MODE` — one case (arguments as in
  `transport/bench/run_case.sh`); `STAND=talaria|linux`; tap + bridge, ivshmem,
  pinning of vCPUs/producer/consumer (`CORE_*`), `LAT_BD=1` — per-stage
  breakdown in the serial logs.
- `matrix.sh [OUTDIR]` — the loopback baseline + both stands × {base 0 %, nack
  1 %, fecnack 1 %} × repeats; `summary.py OUTDIR` — medians of
  p50/p99/p99.9/p99.99/drop over the repeats.
- `linux-guest/` — `init.c` (static: mounts sys/proc, loads virtio_net, eth0
  from `ip=`, ivshmem → `/tmp/shm/*` symlinks + `HARNESS_SHM_DIR`, execs `app=`
  with `args=`), `mk_initramfs.sh` (modules from the initramfs built by
  `bench/linux-guest/mk_guest.sh`, plus sender/receiver).
- `task-shmdir.patch` — an additive patch to the solution: `HARNESS_SHM_DIR` in
  `shm_segment.h`/`attach.h` (a file instead of POSIX shm, without ftruncate) —
  needed only by the Linux guest (sysfs `resource2`); on Talaria the binaries
  run without it.
- `talaria_backend.cpp` (+ `task-talaria-backend.patch`, which registers
  `--io talaria` in `make_backend`) — an `io::Backend` on top of the
  zero-syscall rings of a Talaria cell (`talaria.fast=1`, `-smp 2`, raw
  Ethernet/IPv4/UDP through `io/pkt.h`, the peer's static MAC `--dst-mac`, our
  MAC `TALARIA_MAC`, our IP `--dpdk-ip`). `IO=talaria` in `run_case_vm.sh`.
  Under QEMU it hits a virtio kick per frame (see the results).
- `fastflood` (`kernel/user/fastflood.c`, mode `talaria.mode=fastflood`) — a
  microbenchmark of the cell rings' TX throughput. **Batched notify:** the cell
  puts frames into the virtio queue WITHOUT a kick and notifies once per batch
  (vendored virtio-drivers: `transmit_begin_no_notify` + `transmit_notify`, a
  mandatory barrier before reading `avail_event`, otherwise a kick is lost):
  **517 k frames/s (1.9 µs per frame), one kick per ~40 frames; with vhost
  busy-poll (`POLL_US=50` in `run_case_vm.sh` → `-netdev ...,poll-us=50`) —
  682 k frames/s (1.5 µs per frame)**. Before: 230 k (4.3 µs per frame, a kick
  per frame). Along the way the AP cell's stack grew from 4 KB to 16 KB (the
  locals of `run_cell` exceeded 6 KB and overflowed into neighbouring frames).

What the Talaria kernel got for this: `pci.rs` (ECAM for microvm `pcie=on`, BAR
assignment, ivshmem-plain 1af4:1110), `/dev/shm/*` objects + `mmap(MAP_SHARED,
fd)` → the BAR (cacheable, user), `sendmmsg/recvmmsg`, larger virtio queues and
UDP buffers, one pass of the stack per batch.

## Why the harness does not run on NATIVE Talaria

The harness transport is ivshmem between two VMs on one host (shared memory,
wakeup by a store without a virtio kick). EC2 has nothing like it: two
instances share no memory, and there is no second VM inside an instance. The
native equivalent is a network round trip over ENA, measured separately
(`docs/POC-RESULTS.md` §7). Moving the harness to a network transport between
two native instances is separate work.

## Results on c6a.metal (quiet stand)

The same harness on a dedicated AWS server (EPYC 7R13, 192 vCPUs, no
neighbours and no desktop noise) — unlike the local i7 below, here the
behaviour of the stands shows, not host noise. 100 k messages per case, mixed,
SLOTS=8192.

| rate, msg/s | Talaria p50 | Talaria drop | Linux guest p50 | Linux guest drop |
|---:|---:|---:|---:|---:|
| 50 000  | **13.9 µs** | 0.000 % | 22.7 µs | 0.000 % |
| 100 000 | **14.5 µs** | 0.000 % | 28.9 µs | 0.000 % |
| 150 000 | — | — | 46.2 **ms** | 16.9 % |
| 200 000 | **14.6 µs** | 0.000 % | 53.4 **ms** | 41.0 % |

The 200 k × {base, fecnack} × 3 matrix (medians): Talaria base p50 14.6 / p99
37.2 / p99.9 123.2 µs, no drops; fecnack p50 16.9 / p99 365 µs, no drops. The
Linux guest collapses at 200 k (p50 in milliseconds, 41-46 % loss) — its limit
on this stand is ~100 k msg/s.

On quiet hardware Talaria gives a **p50 1.6-2× lower at equal load** (13.9 vs
22.7 at 50 k; 14.5 vs 28.9 at 100 k) and, above all, **holds twice the load
without a single loss** where the Linux guest already chokes. Talaria's tails
on the quiet stand stopped being a problem too (p99 37 µs against hundreds of
µs to ms on the noisy desktop) — the earlier difference in tails was a
property of the stand, not of the kernel.

## Results on a noisy desktop (second series)

The harness came back to life after two fixes of the stand (not the kernel):
1. **vCPU pinning was silent:** QEMU in the rebuilt `talaria-qemu2` without
   `-name ...,debug-threads=on` no longer names its threads `CPU 0/KVM` —
   `debug-threads=on` is added to both boot paths (`vCPU thread not found` is
   the symptom).
2. **vhost kthreads are invisible from the container:** on this host (el9 5.14)
   vhost threads are classic kthreads in the host pid namespace; the docker
   container got `--pid=host`, otherwise scanning `/proc/[0-9]*` for
   `vhost-*` finds nothing and vhost shares a core with the busy-polling vCPU
   (tails in ms).

Matrix: base 0 % + fecnack 1 % × 3 repeats, 200 k msg/s, 400 k mixed,
SLOTS=8192, LOSS_FIXED=3, NRX 128; medians over the repeats. The same 4C/8T
desktop with MORE background noise than in the first series (rsync at 10-14 %
CPU during the whole run plus firefox) — Talaria's p50 is more sensitive to it
than the Linux guest's (busy-poll without CPU headroom); compare columns within
a run.

| stand | mode | n | p50 µs | p99 | p99.9 | p99.99 | drop % |
|---|---|---|---:|---:|---:|---:|---:|
| loopback (solution on the host) | base | 3 | 4.7 | 13.1 | 36.9 | 258.0 | 0.000 |
| loopback | fecnack 1 % | 3 | 5.7 | 47.0 | 376.0 | 526.1 | 0.000 |
| Linux guest ×2 | base | 3 | 45.2 | 827.8 | 3230.5 | 3886.7 | 0.346 |
| Linux guest ×2 | fecnack 1 % | 3 | 56.3 | 526.0 | 1915.6 | 2810.4 | 0.000 |
| **Talaria ×2** | base | 3 | **28.2** | 4957.8 | 16739.3 | 18348.9 | 1.766 |
| **Talaria ×2** | fecnack 1 % | 3 | **33.1** | 6167.8 | 10958.6 | 12385.6 | 0.000 |

The first series reproduces: **Talaria's p50 is ~1.6-1.7× lower than the Linux
guest's** (28.2/33.1 vs 45.2/56.3); the tails and drops of the base mode are
worse (stalls of the busy-polling vCPU on a noisy host, see below; in fecnack
mode FEC masks the drops completely). The absolute Talaria p50 is higher than
in the first series (28 vs 19-24) with the same Linux (45 ≈ 43.4), consistent
with the increased host noise (the best repeat with quieter noise: 24.4 / 0.07 %
drops); the quiet-stand check is the c6a.metal section above.

## Results on a noisy desktop (first series)

| stand | mode | n | p50 µs | p99 | p99.9 | p99.99 | drop % |
|---|---|---|---:|---:|---:|---:|---:|
| loopback (solution on the host) | base | 3 | 4.5 | 12.0 | 26.8 | 266.8 | 0.000 |
| loopback | nack 1 % | 3 | 4.7 | 32.0 | 72.3 | 307.5 | 0.000 |
| loopback | fecnack 1 % | 3 | 5.9 | 43.9 | 372.9 | 412.4 | 0.000 |
| Linux guest ×2 (host vmlinuz) | base | 3 | 43.4 | 431.4 | 1908.8 | 2425.6 | 0.071 |
| Linux guest ×2 | nack 1 % | 3 | 47.0 | 1570.1 | 7495.2 | 8216.5 | 0.000 |
| Linux guest ×2 | fecnack 1 % | 3 | 59.1 | 8796.2 | 15905.6 | 16613.2 | 0.000 |
| **Talaria ×2** (kernel network stack) | base | 3 | **18.7–23.8** | 1600–11530 | 8126–20110 | 9780–21324 | 0.4–9.0 |
| **Talaria ×2** | nack 1 % | 3 | **21.7–22.2** | 32663–103327 | 42620–111280 | 49837–111802 | 0–3.2 |
| **Talaria ×2** | fecnack 1 % | 3 | **24.7–26.2** | 14053–37389 | 28898–67732 | 29675–68960 | 0–0.005 |

Conditions: one 4C/8T desktop (HT pairs), no isolcpus, background load;
200 k msg/s, 400 k messages, mixed, SLOTS=8192, LOSS_FIXED=3; medians over 3
repeats (for Talaria — the range of medians of two series: NRX 128 and NRX
250). The ends on the host, the "middle" in two VMs (tap + bridge, vhost),
vCPUs pinned; the Linux guest and Talaria run **the same** static musl
`sender`/`receiver` (the udp backend).

Conclusions:
- **p50: Talaria is ~2-2.3× lower than the Linux guest** in the same topology
  (18.7-23.8 vs 43.4 base; 22 vs 47 nack; 25-26 vs 59 fecnack): Talaria's
  kernel stack (virtio polling without interrupts, a 28 ns syscall, smoltcp)
  is cheaper than interrupt + softirq + socket of Linux in a VM. Both are
  ~4-10× the host loopback (4.5): the price of the VM path itself (2× virtio /
  vhost / bridge).
- **Tails and drops are worse and noisier on Talaria** (p99 1.6-100 ms, drops
  0-9 % vs 0-0.07 % on Linux). The cause is vCPU stalls on a noisy host:
  without interrupts and NAPI the guest must keep up polling virtio; when vhost
  is descheduled it drops (an RX ring of 250 frames ≈ 1.7 ms at 150 k pps).
  The Linux guest queues more and recovers. On an isolcpus stand or a real NIC
  this goes away (see c6a.metal above); here it is an honest limitation.
- **The zero-syscall path (IO=talaria, the cell on an AP)** gives no win in
  QEMU: every `transmit_begin` is a virtio kick ≈ 11 µs, and 2 vCPUs per VM do
  not fit into 4 physical cores (the cells share HT with vCPUs) → 40 %+ drops.
  Batched notify (above) addresses the kick.
