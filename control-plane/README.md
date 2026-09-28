# talaria — the Linux-hosted control plane (experimental)

> **Experimental (Track A of the design document, `docs/talaria-os.html`
> §6.1-6.2).** This is the early host-side model, in which Linux is the control
> plane and a cell is a partitioned runtime on a Linux host (VFIO). The product
> today is the bootable kernel in `kernel/`, whose own control plane lives in the
> kernel (`talaria.mode=cell`, the operator console, `/ctl`). This crate is kept
> as a working reference; it has no tests and no stability promise.

`talaria` reads a cell manifest, **guarantees the contract** (cores, memory,
device) and starts and supervises the cell.

## Commands

```
talaria up <manifest.toml>     # check the contract and start the cell
talaria supervise <manifest>   # start with automatic restart (backoff)
talaria status                 # live state of the cells (shared-memory telemetry)
talaria down <name>            # graceful stop (the cell exits on its telemetry tick)
talaria coloc <lat-manifest>   # latency cell + batch cell with core donation
talaria serve [addr]           # TCP control server (default 0.0.0.0:7373)
talaria remote <host[:port]> status|list|down <name>|ping
```

## The manifest is a contract

`control-plane/manifests/md-echo.toml` declares a cell: cores, hugepages, NIC,
driver, policy. On `up` the control plane checks every item and, if a resource
cannot be given, **does not start the cell** (instead of assembling isolation
from boot parameters and only checking it afterwards):

```
$ talaria up strict.toml       # require_isolation=true on a non-isolated core
  ✓ binary ...
  ✓ core 2 exists
  ✗ core 2 is isolated (isolcpus)
contract not met — cell not started   (exit=1)
```

## How it works

- The control plane is an ordinary process; all contract checks are read-only
  and need no root: cores (`/sys/.../cpu/present`, `.../isolated`), hugepages
  (`/sys/kernel/mm/hugepages/...`), the device driver
  (`/sys/bus/pci/devices/<bdf>/driver`), the VFIO group.
- The cell is started as `sudo <cell-binary> --core N --fifo --status F --stop F`
  and pins **itself** (`sched_setaffinity` + `SCHED_FIFO`).
- Telemetry: the cell writes `Stats` into an mmap region
  (`control-plane/shared/stats.rs`, the shared ABI) under a seqlock; `status`
  reads it live, without parsing files and without a syscall in the cell.
- Contract items: cores, isolation (`require_isolation`), 1 GB hugepages, the
  NIC on the right driver, **LLC/CAT** (`llc_mask`, needs resctrl; on a CPU
  without RDT the contract is refused).
- Co-location and core donation (`coloc`): a latency cell plus a batch cell
  (`control-plane/batch`, `SCHED_IDLE`) on the hyperthread sibling; the control
  plane reads the latency cell's load signal (the `pkts` delta from shared
  memory) and parks or releases the batch cell with SIGSTOP/SIGCONT. Idle
  latency cell: the batch collects the full idle time (75 k units/s); loaded
  latency cell: the batch is parked ~95 % of the time (3 k units/s); the latency
  cell's rx→tx stays at 47 ns in both cases. A Caladan/Shenango model in PoC
  form (donation granularity ~2 ms here).

## Network access

The protocol is line-based (one connection, many commands): `status`, `list`,
`down <name>`, `ping`, `help`, `quit`, and works from a raw socket:

```
$ printf 'status\nquit\n' | nc <host> 7373
```

**Security:** there is no authentication. The control plane lives behind an
external perimeter (a trusted management network, like IPMI/BMC). On an
untrusted network put a TLS tunnel in front of the port, or bind to `127.0.0.1`
and use SSH forwarding. `serve` is read-mostly (status/list/ping) with one
mutation (`down`); `up`/`supervise` need sudo and stay local.

Limitations of the PoC: one cell = one core and one NIC; syscall proxy,
capability isolation and memory-bandwidth partitioning are the next layer.
