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
