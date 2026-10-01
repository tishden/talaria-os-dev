# Contributing to Talaria OS

Thank you for your interest. This page describes how development works and what
a change needs to get into `main`. For orientation in the code start with
`docs/DEVELOPMENT-GUIDE.md` (module map §4, recipes §10, **invariants §12 —
read them before changing the kernel**). If the change is about performance, or
if you are going to measure anything, read `docs/ENGINEERING-NOTES.md` first: it
holds the rules of judging a measurement, the ways the instrument has lied
before, and a table of hypotheses already closed by measurement.

## License and CLA

The project is licensed under Apache-2.0 (`LICENSE`, `NOTICE`). Before a
contribution can be merged, its author signs the Contributor License Agreement
(`CLA.md`). There is no bot: you sign by leaving one line as a comment on your
first pull request — `CLA.md` says which line, and the maintainer records it in
`CLA-SIGNERS.md`. You keep the copyright to your contribution; the CLA lets the
project manage licensing as a whole.

## The gate: what must be green

Changes to the kernel or tooling pass the local gate. It needs Linux with
`/dev/kvm` and docker; the images come from `services/docker/build-images.sh`.

```sh
make -C kernel/user all && (cd kernel && python3 mkfs.py)   # regression binaries and disk
(cd kernel && cargo -Z json-target-spec build --release)    # toolchain pinned in rust-toolchain.toml
(cd kernel && cargo clippy -Zjson-target-spec --release -- -D warnings)
kernel/build-aarch64.sh                                     # the arm64 kernel (docs/ARM64-PLAN.md): it must still build
kernel/uefi-stub/build.sh aarch64                           # the arm64 UEFI stub carries the kernel inside: rebuild it too
kernel/test-aarch64.sh                                      # the arm64 gate under TCG: 229 checks with kernel/user/aarch64 built (36 without), ~20 min
(cd kernel && cargo clippy -Zjson-target-spec --release --target aarch64-talaria.json -- -D warnings)
(cd kernel-unit && cargo test && cargo clippy --all-targets -- -D warnings)
kernel/test.sh          # QEMU regression: "TOTAL: ... FAIL=0"
kernel/validate.sh      # heavy validation (when possible; SKIP without artefacts is fine)
```

CI (`.github/workflows/ci.yml`) runs everything above except the FULL `test.sh` and
`validate.sh`, which need KVM and about half an hour. It does run the deterministic
subset of the regression on a runner's `/dev/kvm` (`TAL_CI=1`, 181 asserts plus 16
host checks, ~7 minutes): the network probes, the latency gates and the boot-image
chains stay the local gate, because on a shared runner they judge the neighbour
rather than the kernel.

On a clean clone `test.sh` SKIPs exactly three gates, and all three want a
THIRD-PARTY artefact that the repository does not carry: `dyn` and `dynsync`
(a patched `ld-musl` plus zlib from Alpine) and `dynglibc` (the loader and libc
from Ubuntu). How to obtain each one is in `kernel/user/README.md`; any other
SKIP means something of ours is missing and is a defect, not an artefact.

Known flakes (do not "fix" blindly, re-run):
- `talaria.test=connect` — a rare DNS flake (EAI_AGAIN, 10-30 % in series);
- `gdb-stub: RSP` — occasionally under host load;
- `tsync`, `clock` — sensitive to host load right after heavy runs;
- `evictbatch=64` — the gate gives the boot a fixed 14 s and reads a `stat:`
  snapshot from it; a busy host (a parallel build) does not let the run reach
  the batched evictions, and the knob looks inert;
- `forkap` — 40 fork/exit cycles across four vCPUs inside a fixed 8 s window; on
  a loaded host the workload is still working when the window closes (the two
  assertions after it, about the APs and the freed address spaces, still pass —
  which is the tell that the run was alive, just slower);
- `idleshoot`, `netshoot` — both wait for acknowledgements from a neighbouring
  core, and on a loaded host the neighbour is TAKEN OFF ITS CORE by the
  hypervisor; the gate says so in its own log (`taken by the host true`). Both
  are green in a single repeat on a quiet machine.

Tests that need a fresh `kernel/uefi-stub` are skipped honestly otherwise: the
stub carries the kernel ELF inside, so rebuild it with `kernel/uefi-stub/build.sh`
(and `kernel/uefi-stub/build.sh aarch64` for the arm64 gate's UEFI block)
after every kernel build (`test.sh` does it when the stub is stale).

## Rules for changes

1. **A kernel feature comes with a test.** Behaviour visible from a workload
   gets a regression assertion in `kernel/test.sh` through a mode
   `talaria.test=<name>` (the name is compared whole, `kernel/src/cmdline.rs`;
   dispatch in `kernel/src/selftest/`; recipe in DEVELOPMENT-GUIDE §10.2/§10.5).
2. **Logic that can run without the machine lives in a module covered by
   `kernel-unit`** and comes with unit tests next to it (`#[cfg(test)] mod
   tests`); kernel glue in such a module is `#[cfg(not(talaria_host))]`.
   Examples: `uaccess.rs`, `placement.rs`, `clock.rs`, `ext2hash.rs`.
3. **One commit, one finished thing**, and the message explains WHAT and WHY
   (see `git log`: detailed messages with measured numbers).
4. **Performance claims come with numbers and the stand they were measured on.**
5. **Documentation is part of the change:** USER-GUIDE for user-visible
   behaviour, DEVELOPMENT-GUIDE for internals and invariants, CHANGELOG at
   releases.
6. **The invariants of DEVELOPMENT-GUIDE §12 are mandatory** (examples: copy the
   `Frame` in syscall handlers; EFER.NXE on APs; `wake_addr` when the futex word
   changes; `sti; mwait` only after `lapic_enable`; anonymous mmap is zeroed).
7. **Rust:** `no_std`, the style of the surrounding code; English in new code,
   comments and messages; byte strings `b"..."` are ASCII only.
8. **Third-party code:** do not vendor code under licenses incompatible with
   Apache-2.0 (GPL etc.) — download it from upstream with a script (example:
   `services/mongoose/fetch.sh`); update `NOTICE` for new dependencies.

## Layout

- `kernel/src` — the kernel (module map: DEVELOPMENT-GUIDE §4);
- `kernel-unit` — host unit tests of the kernel's pure modules;
- `kernel/tools` — `talaria-pack`, `talaria-check`;
- `kernel/user` — ring-3 test binaries (sources; built with the docker images
  `talaria-musl`/`talaria-alpine-build`);
- `examples/hello-talaria` — the reference path for users;
- `services` — open-source workloads (build scripts; no binaries are stored).

## How to propose a change

An issue (<https://github.com/tishden/talaria-os-dev/issues>) describing the
problem or idea, then a pull request with a test and a green gate. For anything large (a new subsystem, an ABI change) open an issue
with the design first — it saves everyone time. For "how does X work" read
DEVELOPMENT-GUIDE first: almost every mechanism is described there together
with its pitfalls.
