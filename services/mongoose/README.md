# mongoose on Talaria — a real open-source web server in ring 3

The unmodified source of [mongoose](https://github.com/cesanta/mongoose) 7.23
(Cesanta's embedded web server), built statically against musl, runs **in
ring 3 on the Talaria kernel** and serves HTTP over TCP through the kernel's
network stack.

- `server.c` — a minimal HTTP handler on top of the mongoose API
  (`mg_http_listen` + `mg_http_reply`).
- `mongoose.c` / `mongoose.h` — the mongoose 7.23 source (unmodified). It is
  NOT stored in this repository (GPLv2/commercial license by Cesanta, not
  compatible with the Apache-2.0 tree); `./fetch.sh` downloads it before the build.
- `mkdisk.py` — packs the binary into a TFS1 image (`disk-mongoose.img`).
- `run.sh` — runs it under QEMU with a tap network and sends HTTP probes.

## Build and run

```
./fetch.sh
# 1. build the static binary. MG_ENABLE_LOG=0 is required for measurements:
#    with logging mongoose prints two lines per request, which on Talaria go
#    to the serial console — you would measure the console. (Linked at
#    0x10000000, the Talaria user region.)
docker run --rm -v "$PWD":/w -w /w talaria-musl sh -c \
  'musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x10000000 \
     -o mongoose_srv server.c mongoose.c \
     -DMG_ENABLE_EPOLL=1 -DMG_ENABLE_POLL=0 -DMG_ENABLE_LOG=0'
# 2. pack the disk
python3 mkdisk.py disk-mongoose.img
# 3. build the kernel (kernel/BUILD.md) and run
services/mongoose/run.sh
```

`run.sh` sends a few GETs and checks keep-alive; the server answers with a
dynamic page (method, URI and Host parsed) and serves `index.html` from disk
on `/` through `mg_http_serve_file` (incremental streaming via stat/open/read).

## What it needed from the kernel

mongoose uses its epoll backend (`MG_ENABLE_EPOLL`):
socket/bind/listen/accept4/epoll_*/read/write/close/fcntl, plus
getsockname(51)/getpeername(52)/getsockopt(55). Serving a file required real
stat(4)/newfstatat(262) and fixed a file-system bug: `open` now returns a
SEPARATE descriptor for every call (the slot used to be indexed by the file
number, so opening the same file twice aliased the descriptors and closing one
broke the other; mongoose opens a file twice — fopen for the stream plus stat).
Kernel mode: `talaria.mode=mongoose`; in `bench/suite` it runs by manifest.
