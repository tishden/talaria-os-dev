# Lua 5.4 on Talaria — an open-source interpreter in ring 3

Unmodified [Lua](https://www.lua.org) 5.4.7, built statically against musl
(linked at 0x10000000), runs **in ring 3 on the Talaria kernel** (`kernel`) and
executes `script.lua` from disk (TFS1 over virtio-blk). It shows the
compute/file class of workloads (unlike the networked mongoose).

- `script.lua` — the demo: recursion (fib), table sorting, strings, float math,
  a loop of 1e6.
- `mkdisk.py` — packs `lua` and `script.lua` into a TFS1 image.
- `run.sh` — runs it under QEMU.

## Build and run

```
curl -sSL https://www.lua.org/ftp/lua-5.4.7.tar.gz | tar xz
docker run --rm -v "$PWD":/w -w /w talaria-musl sh -c \
  'musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x10000000 -DLUA_USE_POSIX \
     -I lua-5.4.7/src -o lua $(ls lua-5.4.7/src/*.c | grep -v luac.c) -lm'
python3 mkdisk.py disk-lua.img
services/lua/run.sh     # -> fib(30)=832040, sorted, string ops, ... "Lua OK on Talaria"
```

Kernel mode: `talaria.mode=lua` (runs `lua /script.lua` from disk). Zero changes
to Lua, zero unknown syscalls. The binary and the image are in `.gitignore`.
