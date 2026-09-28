# ENA latency: Talaria ↔ Talaria over the real AWS network (test design)

> **STATUS: this design is OBSOLETE and was never executed.** It was drawn up
> when Talaria had no ENA driver of its own and the guest reached the network
> through a host tap/proxy_arp. Since v1.1 Talaria IS the EC2 instance (a UEFI
> AMI with in-kernel ENA), so latency is measured without a QEMU middleman:
>   - method and numbers — `docs/POC-RESULTS.md`, section "§7 ENA datapath";
>   - tools — `infra/aws/lab-up.sh` (the stand: the subject plus a Linux
>     neighbour in one cluster placement group), `bench/udp_ping.c`,
>     `bench/udp_owd.c` (one-way delays), `talaria.netprof=N` (per-stage profile);
>   - result: after the LLQ work, UDP RTT 33.6 µs p50 against 43.0 µs for
>     Linux → Linux on the same VPC path (`docs/PRODUCT.md` §3).
> The original design below is kept as a reference for the one-way method (§2)
> and the secondary-IP/proxy_arp layout, should a "guest in QEMU" scheme be
> needed again.

Two AWS metal instances, each running Talaria in QEMU/KVM; the guests send each
other TCP and UDP traffic through **ENA** (the real data-centre VPC network, not
a local tap). We measure RTT percentiles and **one-way latency with PTP-synced
hosts**; the baseline is Linux guests on the same hosts.

## 1. Topology

```
[instance A: c6a.metal]                         [instance B: c6a.metal]
 Talaria (QEMU/KVM)                              Talaria (QEMU/KVM)
   virtio-net                                      virtio-net
     │ tap0 (10.200.1.2/32)                          │ tap0 (10.200.2.2/32)
 host A: route + proxy_arp ──── ENA / VPC ──── host B: route + proxy_arp
         (secondary private IP)   same AZ,             (secondary private IP)
                              cluster placement group
```

- **A cluster placement group in one AZ** — the minimum network distance
  (expected host↔host RTT ~40-80 µs).
- The guest reaches out WITHOUT NAT: each guest gets a **secondary private IP**
  on the instance's ENI; the host has `ip route add <IP>/32 dev tap0`,
  `proxy_arp` and `ip_forward`. The VPC delivers packets to the instance (the
  IP is assigned to it) and the host routes them into the tap. No conntrack
  overhead and no address rewriting. (Fallback for debugging: MASQUERADE.)

## 2. Clocks and PTP (for one-way)

Three levels, each with its own error — report it next to the number:

1. **Host ↔ host:** chrony on AWS Time Sync. If the instance type supports the
   **ENA PHC** (`/dev/ptp*`, check with `ethtool -T ens…`), use the PHC as the
   source (sub-µs error); otherwise NTP at 169.254.169.123 (tens of µs — then
   one-way is only an estimate). The script records `chronyc tracking` before
   and after the run.
2. **The main one-way method — host timestamps, guest clocks NOT needed:**
   tcpdump with software timestamps simultaneously on tap0 (the guest boundary)
   and on the ENA interface of both hosts. The four points (tapA→enaA→enaB→tapB)
   give: network one-way = t(enaB_rx) − t(enaA_tx) (valid with synchronised
   hosts); each side's guest stack contribution = the tap↔ena deltas (local
   clocks, no PTP needed). Packets are matched by UDP payload (the sequence
   number in the first 8 bytes of udp_ping).
3. **Secondary method — guest timestamps:** Talaria's wall clock is disciplined
   against kvmclock with a stable offset to the host. Calibrate guest ↔ ITS host:
   the median of (t_host − t_guest − RTT/2) over 1000 UDP pings through the
   tap. Useful as a cross-check of method 2.

## 3. Scenarios

| # | Scenario | Tools (in the repo) | Metric |
|---|---|---|---|
| 1 | UDP RTT guest↔guest | `udp_ping` (client on Talaria A) ↔ `userver` UDP echo (Talaria B, mode=cell) | p50/p99/p99.9, N=100k |
| 2 | TCP RTT guest↔guest | `tcp_ping` ↔ `epsrv` (epoll TCP echo) | p50/p99/p99.9 |
| 3 | UDP one-way + breakdown | a udp_ping stream + tcpdump on tap/ENA of both hosts (§2.2) | one-way p50/p99; share of guest A / network / guest B |
| 4 | Under load | `tcp_bench` keep-alive + udp_ping in parallel | pps + RTT tails under a stream |
| 5 | Linux baseline | the same 1-4 with a Linux guest (host vmlinuz + initramfs) | the same |

## 4. Expectations and sanity checks

- Host↔host RTT is the base; guest↔guest RTT ≈ base + 2×(virtio + tap) ≈
  +40-120 µs. Guest↔guest below host↔host means a broken stand.
- The sum of the §3.3 breakdown (guest A + network + guest B) must match RTT/2
  of §3.1 within the clock error — a self-check of the method.
- Talaria vs Linux: a win is expected in the guest contribution and the tails
  (no NAPI batching or softirq jitter), parity in the network part (it is shared).
- QEMU: `-overcommit cpu-pm=on` on both; vCPUs pinned to cores isolated from
  host interrupts (`irqbalance` off, RPS off on the tap).

## 5. Skeleton (this directory)

- `terraform/` — 2 × metal in a cluster placement group, a secondary private
  IP per instance, a security group with ssh and all traffic inside the group.
- `setup-net.sh` — on a host: tap0, the secondary-IP→tap route, proxy_arp,
  ip_forward, chrony (PHC if available), RPS/GRO off on the tap.
- `run-talaria.sh` — QEMU with virtio-net on tap0 (`talaria.mode=cell`).
- `orchestrate.sh` — setup → run → measure → destroy.

```sh
cd bench/ena-latency/terraform && terraform apply   # 2 instances
../orchestrate.sh
```
