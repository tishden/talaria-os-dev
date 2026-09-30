# kernel/user — ring-3 test binaries of the regression

Every binary has a source here; everything is built from source:

```sh
make all      # docker toolchains talaria-musl / talaria-alpine-build
make disks    # rebuild ../disk.img (mkfs.py: hello.txt + mhello + epsrv + memhog)
```

The kernel embeds these binaries when built with its default `selftest`
feature (`user_elf!` in `kernel/src/main.rs`); `cargo build
--no-default-features` builds the kernel without them.

Target classes (see the Makefile): C (musl-gcc, static at `0x10000000`), C++
(Alpine g++), assembly (`as` + `ld -T user.ld`), freestanding (`launcher` with
its own `_start`), libtsync (`tsync/*`, linked with `tsync.c`/`fiber.c`), and
dynamic PIE (`dyn/hello`, `tsync/tsynctest.dyn`).

## The arm64 set

```sh
make aarch64              # docker toolchain talaria-musl-aarch64 (services/docker/Dockerfile.musl-aarch64)
make aarch64 MUSLA64=     # ...or a cross `musl-gcc` for aarch64 that is already on PATH
```

The SAME sources build into `aarch64/` (static, linked at `0x50000000` — RAM on
QEMU's `virt`; x86's `0x10000000` is a PCIe window there): every C program except
the three that are x86 machine code by design (`fastapp`, `fastflood`, `sysbench`),
plus `mhello`, `mprintf`, `crashme`, `profdeep` and the assembly `hello.elf`
(`hello-aarch64.s`). The few machine-dependent lines live in one header,
`tal_arch.h` — the cycle counter (`rdtsc` / `CNTVCT_EL0`), the spin hint, the
thread pointer (`%fs:0` / `TPIDR_EL0`), the pc in a signal's `ucontext` — so a
test is one file for both machines, with an `#if defined(__aarch64__)` only where
the thing tested is itself the machine's (`fputest`'s registers, `efault`'s kernel
addresses, the legacy `open`/`pipe` numbers arm64 does not have). Not ported yet:
`launcher`, the other `.s` files, libtsync's fibers, the C++ set (it needs a
libstdc++ built against the cross musl) and the dynamic ones.

The arm64 kernel embeds whatever `aarch64/` holds (`build.rs` generates
`user_elf!` from the directory: a name that is there is embedded, any other is an
empty slice), and `kernel/test-aarch64.sh` builds its disk images from it. The
binaries are not in git; a checkout without them skips the ring-3 part of the
arm64 gate and says so.

## Third-party artefacts (NOT built by the Makefile)

- `dyn/ld-musl-x86_64.so.1` — **patched musl 1.2.5** (the umwait tier in the
  futex paths): built by `tsync/build-musl.sh` (downloads musl 1.2.5, applies
  `tsync/musl-1.2.5-talaria.patch`, builds in docker).
- `dyn/libz.so.1` — zlib from Alpine (`apk fetch zlib` / from the alpine:3.20 image).
- `glibc/ld-linux-x86-64.so.2`, `glibc/libc.so.6`, `glibc/libz.so.1` — from
  Ubuntu 22.04 (packages libc6, zlib1g); `glibc/hello` — `gcc hello.c -ldl -lz`
  on Ubuntu 22.04 (dynamic, PIE).

Without these artefacts the corresponding tests (`dyn`, `dynsync`, `dynglibc`)
are skipped by the regression (their disk images cannot be built — `test.sh`
says so).
