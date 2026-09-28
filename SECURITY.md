# Security Policy — Talaria OS

Talaria OS is a single-tenant OS for a trusted workload behind an external
security perimeter (see `docs/PRODUCT.md` §2). Its protection mechanisms (W^X,
per-process address spaces, the syscall allowlist, control-plane tokens) are
designed against workload bugs, not against hostile code. Vulnerability reports
are welcome all the same.

## What is protected and what is not

- **Kernel memory against the workload's syscalls.** Every pointer a process
  passes to a syscall is checked before the handler runs: the whole range the
  kernel will read or write must lie in memory the process itself can reach
  (`kernel/src/uaccess.rs`). Otherwise the call fails with `-EFAULT`, as on
  Linux, and the kernel is unaffected (gate `talaria.test=efault`). The cost is
  about 10 ns per syscall with a pointer argument. `talaria.uacheck=0` turns
  the check off (only to measure its cost).
- **Process isolation.** Every process has its own address space; the kernel,
  the page tables and kernel stacks are mapped in it without the U/S bit.
- **SMEP is on** where the CPU has it (CR4.SMEP, checked by CPUID 7): ring 0
  cannot FETCH from a page of the workload, so a call through a corrupted pointer
  into memory the workload has just written faults at the first instruction
  instead of executing. The boot log says which it is
  (`paging: SMEP is ON` / `is NOT SUPPORTED by this CPU`).
- **SMAP is deliberately NOT used.** The kernel copies to and from a process's
  memory with plain loads and stores (that is what makes a syscall cheap), and
  SMAP forbids exactly that. Turning it on means wrapping every such access in
  `stac`/`clac` — a different design, not a switch.
- **What is missing.** Modes that run the workload in the kernel's own address
  space (`talaria.cell=identity`, the embedded demo modes) protect only the
  kernel image from syscalls. The threat model is bugs in a trusted workload, not
  hostile code.

## What to know when deploying

Both network services of the control plane require a token, and **there is no
default** — without one the service is closed:

| service | boot parameter | without it |
|---|---|---|
| `/ctl` route (telemetry over HTTP) | `talaria.ctltoken=<secret>` | 401 on every request |
| `filed` (file upload/download) | `talaria.filedtoken=<secret>` | every command refused |

Tokens are compared in constant time. There is no TLS and no rate limiting,
and `filed` can WRITE to the node's file system: both services are meant for a
trusted management network (like IPMI). Use a tunnel on an untrusted network.

The kernel command line is visible in the hypervisor configuration and in the
instance user data: keep tokens where you keep the other deployment secrets.

## How to report

Write privately to tishden88@gmail.com (subject: "Talaria security"). Include
the version or commit, the QEMU configuration, steps to reproduce, and expected
versus actual behaviour. Please do not publish details before a fix.

## What to expect

The project is maintained as is: acknowledgement on a best-effort basis, fixes
without an SLA (priority response is available under a commercial support
agreement, see `SUPPORT.md`).
