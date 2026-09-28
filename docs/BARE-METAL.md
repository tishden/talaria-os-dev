# Bare metal: what QEMU hides

This document answers one question: **why code that is green in the regression
falls over on the very first real machine — and how to stop letting that happen.**

It was written after four sessions (A1, A9, A10-A12) in which Talaria was
brought up on a `c6i.metal`. Every item here is not a line of reasoning but a
measurement paid for by a run on hardware; in brackets it says what closes that
class in the regression, so as not to pay again.

## 1. The rule everything else follows from

**A hypervisor is not "slow hardware", it is ANOTHER machine.** It gives zeroed
memory, dense core numbers, a live COM1, assigned BARs, a fast disk and an
emulated APIC bus. A real machine gives none of that. Everything the kernel
silently assumes, the hypervisor silently confirms.

Hence the practical conclusion: **any "it works anyway" on QEMU is evidence of
nothing.** The evidence is either a measurement on hardware or a gate that
reproduces the hardware's behaviour at home (see §4).

## 2. The catalogue of pitfalls

### 2.1. The memory is not zeroed

A bootloader is not obliged to fill the `.bss`. Under QEMU the memory is zeroed —
the whole class of errors is invisible; on hardware the statics start with
garbage (armed locks, "already mounted", garbage pointers), and the kernel hangs
silently.

*What to do:* the kernel's entry zeroes the `.bss` itself (`linker.ld` gives out
the bounds).
*What it cost:* session 9, three runs.

### 2.2. The core numbers are sparse

The MADT on a `c6i.metal` gives out the APIC ids `0, 2, 4, 6, …` — between them
are the HT twins. Under QEMU and on Nitro guests they are dense, so "a core's
logical number" and "an APIC id" were one number for three generations. On
hardware the workload asks for core 1, the kernel brings up the core with APIC
id 2 and starts it as "core 2" — the task goes to nobody, while
`sched_setaffinity` returns SUCCESS.

*What to do:* an APIC id lives only on the boundary with the LAPIC
(`smp::start_ap`, `send_ipi`); the whole rest of the kernel uses logical numbers
(`smp::topology_init`).
*The gate:* `smp2-sparse`, `smp4-sparse`, `smp8-sparse`, `smp16-sparse` — QEMU
comes up with sparse ids (0,2 / 0,2,4,6 / 0..14) exactly like the metal's MADT.
*What it cost:* session 10, the analysis of A10.

### 2.3. A device that is not there is never "empty" — it is 0xFF

On a port that does not exist the bus floats, and ALL its registers read as
`0xFF`. For COM1 that means: the bit "a byte is ready" in the LSR is always set,
and the "data" is always `0xFF`. The loop `while let Some(..) = getc()` in the
console's poll had no bound and was called from the tick's handler — on a
`c6i.metal` (there is no COM1 there) the very first tick went into an infinite
loop INSIDE the interrupt. From the outside: ring 3 is alive, the syscalls go,
the timer ticks, and there is no pre-emption.

*What to do:* `LSR == 0xFF` means "there is no port", not "there is data". And a
loop in the body of an interrupt handler is bound to have a limit under any data.
*The gate:* `talaria.test=serial` (in the regression — three checks "serial: …").
*What it cost:* session 10, six runs — the project's most expensive pitfall.

### 2.4. The firmware does not assign a BAR to what it does not boot from

In BIOS mode the firmware touches exactly what it needs itself. The ENA on the
metal was left without an address, the kernel gave it its own out of the fixed
window `0xC0000000` — and all its registers read as `0xFFFFFFFF`. Under QEMU that
is allowed: there are no bridges there, and whatever is written into a BAR the
device decodes. On hardware a bridge forwards upwards only the addresses from its
own windows.

*What to do:* an unassigned BAR is taken from the window of the NEAREST bridge
above the device's bus (Memory Base/Limit, 0x20/0x22); our own window is a
fallback for QEMU.
*What it cost:* session 10, three runs.

### 2.5. A device may answer "in the wrong way"

In the ENA the reading of the registers is "readless": the driver writes a
request, the device puts the answer in by DMA. The mechanism was devised for
paravirtual platforms; on bare metal the device does not answer such a request at
all.

*What to do:* a direct read of the BAR as a fallback (at the first timeout
readless is declared dead), `talaria.ena=noreadless` — not to pay for the timeout
even once.

### 2.6. A driver is not re-entrant, and an interrupt does not know that

The flush of the trace to the disk writes through the NVMe. If the timer's tick
arrives while the main path is already submitting a command, the flush FROM THE
HANDLER gets into the same queue — both sides wait for each other. Under QEMU the
window is microscopic (the disk is fast), on the metal every write is
milliseconds.

*What to do:* the block layer declares itself busy (`fs::blk_busy`), the trace
from an interrupt writes only if the disk is free. The general rule: **I/O from
an interrupt handler is admissible only with a check that the device is free.**

### 2.7. The clocks and the timers go in other units

The LAPIC timer's period at one and the same initial counter: **320 ms on
hardware** (a 25 MHz crystal) against **8 ms under QEMU** (an emulated 1 GHz APIC
bus) — a difference of forty times. The scheduler's slice on hardware is forty
times longer than the home stand shows.

*What to do:* `talaria.slice=N` reproduces the hardware slice at home. Do not
rely on the tick's rate in the logic.
*The gate:* `timer` — the timer's very ability to interrupt the kernel (three
interrupts a second), checked in the regression.

### 2.8. There is no console

`get-console-output` on a `*.metal` is a SCREENSHOT of the firmware. A line that
lived on the screen for milliseconds will not reach it; a raw COM1 will not reach
it at all. A hang and a reboot loop look the same.

*What to do:* the kernel's log is written to the disk (`talaria.trace=<LBA>`),
the boot counter lives in the bootloader's trace, and at the end of the run the
machine **powers itself off** (ACPI S5) — then `stopped` means "it reached the
end" unambiguously.

### 2.9. Tables sized "with room to spare" run out

The PCI table of 32 records lived through three EC2 generations and died on the
first real machine (52 bridges + 307 functions). The MADT ceiling of 64 records
showed a 128-core machine as a 64-core one.

*What to do:* an overflow is bound to be LOUD rather than a silent truncation.

### 2.10. "The first one that turns up" is an assumption

On a Nitro metal every EBS volume is a separate NVMe controller; the boot one is
determined by a signature on the disk, not by the order of the buses. A device
may stand on a bus unreachable by a walk over the bridges from bus zero.

### 2.11. A mistake in the reporting costs more than it seems

`start_ap` returned control on "the core is alive", while the core gets into the
`ONLINE` mask later. At `talaria.smp=16` all the 15 APs started, while the mask
was printed as `0x7fff`. Worse than the reporting is that a workload that asked
for a core right after the start would have got an EINVAL on a live core.

*What to do:* "alive" and "ready" are different events; what one has to wait for
is readiness.
*The gate:* `smp16-sparse` requires the FULL mask `0xffff`.

**A continuation (v1.9):** "ready" turned out not to be one event either. For the
scheduler a core is ready when it PICKS UP TASKS, that is, has reached the pickup
loop and marked itself in `sched::POOL`; between `ONLINE` and the pool there lie
also the GDT/TSS and the setting up of syscall. A layout computed in that gap
would simply not have seen the core — and the thread would have stayed on the
control plane, while in the log there would have stood "the AP came up". The
`ONLINE` mask moreover includes the cores of the self-test mode, which do not pick
up tasks at all: to put a thread there is to lose it silently. Hence a separate
mask for the pool.

### 2.12. One variable for everyone lives until the second owner

The scheduler lived only on the BSP, so "the next task's CR3", "this is a
software yield, an EOI is not needed" and "what the LAPIC timer is configured
with" lay quietly in global variables. As soon as the scheduler appeared on every
core of the common pool (v1.9), each of the three became a race: two cores
switching simultaneously overwrite each other's target address space; a yield
flag eaten by a foreign core means an EOI not sent, that is, a stuck ISR bit and
pre-emption dead forever and silently (exactly A12); an AP stopping its own timer
zeroes a foreign one's configuration.

This is the same class as "a core's logical number against an APIC id" (§2.2):
what has to be checked is not "does it work" but **how many owners it is designed
for**. All three moved into the per-CPU area (`gs:40`) and into per-core arrays.

*A related pitfall from the same fix:* the IDT table is one for everyone, and
`idt::init` on a core brought up AFTER the scheduler was installed returned the
timer vector to the bare tick counter — there would have been no pre-emption on
that core, silently.

## 3. The toolbox

Every closed hypothesis left a knob behind. The table is what to ask with what.

| The question | The knob | What it will show |
|---|---|---|
| How far the kernel got on a machine without a console | `talaria.trace=<LBA>` | the whole log as raw sectors onto the disk |
| …and the last lines before the halt | `talaria.trace=<LBA>,eager` | a flush after EVERY line |
| Did the run reach the end or stall | ACPI S5 (by itself) / `talaria.poweroff=1` | the instance in `stopped` = it got there |
| Is pre-emption alive | `talaria.heartbeat=N` | the ticks SEPARATELY real and software ones, the live tasks, a stuck ISR |
| Does the timer interrupt the kernel at all | `talaria.test=timer` | the number of interrupts a second, the IRR/ISR, the LVT's state |
| Is it about the waiting tier? | `talaria.sync=hlt\|mwait` | a forced tier |
| Is it about the slice's length? | `talaria.slice=N` | the hardware slice at home |
| What the workload sees | `talaria.irr=N` | the RFLAGS (IF), the IRR, the ISR on the first N syscalls |
| Where the scheduler goes | `talaria.switches=N` | the switch's target, its RSP/CR3/RIP/RFLAGS |
| Where an AP stalled | `AP_STAGE` (printed by `start_ap` at a timeout) | the stage: the trampoline / the IDT / the CR4 / the PAT / ONLINE / the LAPIC / the mode |
| A matrix of modes in one launch | `talaria.suite=<LBA>` | an item per boot, the log into its own slot, the counter on the disk |
| The network: the ENA does not answer? | `talaria.ena=noreadless` | a direct read of the BAR instead of readless |
| There are more cores than are visible | the log `smp: the topology` | the logical cores, the APIC ids, the ceiling |

## 4. How to reproduce the hardware at home

The gates in `kernel/test.sh` that close the classes listed:

| The gate | What it reproduces |
|---|---|
| `smp2/smp4/smp8/smp16-sparse` | sparse APIC ids (`0,2,4,…`) — the metal's topology |
| `smp2-noap` | a `sched_setaffinity` onto a non-existent core -> an EINVAL rather than a quiet success |
| `smp16-sparse` (the mask) | "alive" != "ready": the ONLINE mask is full at once |
| `timer` | the periodic LAPIC timer interrupts the kernel (three interrupts a second) |
| `serial` (3 checks) | polling an ABSENT port does not hang the kernel and does not pass 0xFF off as data |
| `tboot-6g` | the BIOS path on 6 GB: an E820 map with areas beyond 4 GB and a 64-bit BAR above 4 GB |
| `suite` | a set of modes with a reboot between the items |
| `trace` | the kernel's log reaches the disk |
| `mbr-chain` | the metal's diagnostics tool itself does not break the boot |

The techniques by which this is done in QEMU:

- **sparse APIC ids** — one boot core plus cores added one per socket:
  `-smp 1,maxcpus=N,sockets=K,cores=1,threads=2` plus
  `-device host-x86_64-cpu,socket-id=i,core-id=0,thread-id=0`;
- **a lot of memory on the BIOS path** — `-m 6144`, and the devices' roles are
  SEPARATED: one and the same image is attached twice — a `virtio-blk-pci` as the
  firmware's boot device and an `nvme` as the kernel's working disk
  (`file.locking=off` on both). The reason is two limitations at once: SeaBIOS
  does not find a bootable NVMe as soon as QEMU splits the memory above 4 GB (on
  a q35 that is already from ~2.75 GB, on an i440fx — from 4 GB; in its own log
  through `-debugcon` one sees "Booting from ROM… No bootable device"), while the
  kernel on the BIOS path drives exactly the NVMe. As a bonus the NVMe gets a
  64-bit BAR ABOVE 4 GB (`0x380000000000`) — the very case for the sake of which
  there is a `map_mmio_1g` in the kernel, and which otherwise does not reproduce
  at home;
- **the hardware slice of the scheduler** — `talaria.slice=20000000` (about
  320 ms, as on an Ice Lake);
- **the waiting tier as on the metal** — `-overcommit cpu-pm=on` gives a real
  `mwait`; `talaria.sync=hlt` — back again.

## 5. The order of working with hardware

A run on a `*.metal` costs about $3 and 25 minutes (the POST on a machine with
256 GB takes minutes). The order that paid off:

1. **First at home.** Everything reproducible (§4) is checked in the regression.
2. **Then on the cheap one.** THE SAME image on a `c6i.large` (~$0.1): the boot
   chain is the same for virtual instances and for the metal. This step twice
   caught a mistake before the expensive one: an image that carried the set's
   state away with it, and a `talaria.ip=dhcp` that was not applied.
3. **Then on the metal** — as a set (`talaria.suite=`), so that one launch of an
   instance answers several questions, and with an eager trace.
4. **Every hypothesis leaves a knob behind.** A run that only answered "no" is
   wasted; a run that left an instrument in the kernel pays off on the next one.

The tools: `infra/aws/talaria-deploy.sh` (a binary → a working instance in one
command; on a metal — with `--keep-disk`, otherwise the volume with the trace
goes away together with the instance), `infra/aws/metal-ref-probe.sh` (a probe
with a set, `SLOTS=N`), `infra/aws/read-disk-trace.sh` (read the trace off the
volume).

Separately about the stand itself:

- **a script edited on the fly breaks on the fly**: bash re-reads the file as it
  executes. One has to run a copy;
- **a volume with a run's log must not be deleted until the log has been read** —
  otherwise a run of 40 minutes and $4 is lost entirely (`READ_PENDING` in
  `metal-ref-probe.sh`);
- **a security group lives for months, while the developer's address changes**:
  the ssh rule for the current address is checked at every launch;
- **an image run locally carries the set's state away with it** (the counter and
  the logs live IN THE IMAGE FILE) — the check "the trace's sectors are zero
  before the run" STOPS the run.

---

Related documents: `docs/PLAN-V1.7.md` (A9: the bring-up on hardware),
`docs/PLAN-V1.8.md` (A10-A12: SMP, the network, pre-emption), `docs/TODO.md` (the
open items and the pitfalls by session), `docs/USER-GUIDE.md` §9.10 (the
deployment on AWS).
