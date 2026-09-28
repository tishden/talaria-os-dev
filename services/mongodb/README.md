# MongoDB 4.0 on Talaria — a dynamic ELF (ld-musl) + WiredTiger in ring 3

Unmodified **`mongod` 4.0.5 from Alpine 3.9** (musl, *dynamically* linked:
`/lib/ld-musl-x86_64.so.1`, libstdc++, boost, libssl/libcrypto, snappy,
yaml-cpp, pcre, libcurl…) runs on the Talaria kernel (`talaria.mode=proc`): the
kernel loads the PIE image and the ld-musl interpreter, ld.so maps 18 shared
objects itself through file `mmap`/`MAP_FIXED`, WiredTiger opens and recovers
the database on an ext2 disk, the server listens on `:27017`, and a client
speaking the wire protocol (OP_MSG) from the host runs
`insert`/`find`/`count`/`listDatabases`; the data survives a VM restart.

Why dynamic and not static: MongoDB does not build statically against musl
without hours of patching, and glibc binaries are dynamic only. So the kernel
got a **dynamic loader** (PT_INTERP, ET_DYN/PIE, auxv for ld.so), which opens
the door to other ready-made binaries too.

Verified by `run.sh` (`mongoprobe.py` from the qemu container, tap 10.0.0.2:27017):

```
mongod: the server answers after 19 s
buildInfo: version=4.0.5 gitVersion=3739429dd92b92d1b0ab120911a23d50bf03c412 ok=1.0
insert: n=2 ok=1.0
find: ok=1.0 rows=2
   {'_id': 1787437652309, 'v': 'talaria', 'n': 1}
   {'_id': 1787437652310, 'v': 'bare metal', 'n': 2}
count: n=2 ok=1.0
databases: ['admin', 'local', 'talaria']
mongoprobe: ALL OK
```

A second `run.sh` without `mkdisk.sh`: `find` returns 4 documents (2 old + 2
new), the serial log shows `txn-recover: Recovering log 1 through 2`
(WiredTiger recovers its journal after the VM was killed); `e2fsck -fn
disk-mongo.img` is clean. (`count` after an unclean restart returns a stale
value — the known behaviour of the fast metadata count in 4.0.)

A clean shutdown: `WAIT=70 ./run.sh 120 "talaria.stop=60"` (SIGTERM →
`got signal 15 … shutting down with code:0`); after it `mongod.lock` is empty
and the next start needs no recovery.

## Build and run

```
# 1) extract mongod + its closure of shared objects from Alpine 3.9 -> build/root
docker run --rm -v "$PWD/build":/out alpine:3.9 sh -c 'apk add --no-cache mongodb; mkdir -p /out/root; \
  for f in $(ldd /usr/bin/mongod | awk "{print \$3}" | grep /) /lib/ld-musl-x86_64.so.1 /usr/bin/mongod; do \
  mkdir -p /out/root$(dirname $f); cp -L $f /out/root$(dirname $f)/; done; chown -R 1000:1000 /out'
services/mongodb/mkdisk.sh 512M   # ext2: build/root + /data/db, /tmp, /etc/passwd, /cell.manifest
services/mongodb/run.sh 100       # QEMU (tap, 1 GB) + an OP_MSG probe; serial -> build/serial.log
```

`cell.manifest`: `binary=/usr/bin/mongod`, `args=mongod --dbpath /data/db
--bind_ip_all --port 27017 --wiredTigerCacheSizeGB 0.25 --nounixsocket
--setParameter diagnosticDataCollectionEnabled=false --setParameter
disableLogicalSessionCacheRefresh=true`, `env=LD_LIBRARY_PATH=/usr/lib:/lib`.
The client is `mongoprobe.py` (a BSON codec + OP_MSG, ~100 lines, no dependencies).

## What it needed from the kernel

- **Dynamic ELF** (`user.rs`): PT_INTERP → ld-musl loaded at `0x7f00_0000_0000`,
  the ET_DYN/PIE main image at `0x5555_5555_0000`, segments sharing a page are
  written into one frame with the permissions ORed; auxv: `AT_BASE`,
  `AT_ENTRY` (of the main image), `AT_PHDR/PHENT/PHNUM`, `AT_PLATFORM`,
  `AT_EXECFN`, `AT_HWCAP`, `AT_CLKTCK`, `AT_UID..`, `AT_SECURE=0`, `AT_RANDOM`.
- **File `mmap`:** `MAP_PRIVATE` of a file and `MAP_FIXED` at any address (ld.so
  places the segments of shared objects over a reservation); `mprotect` keeps
  the PRIV/COW bits (otherwise fork stopped copying pages after RELRO/guard)
  and breaks COW when RW is requested.
- **Real eventfd/timerfd** (the ASIO reactor of mongod: epoll wakeups and
  deadline timers — with stubs the timers would never fire).
- `--nounixsocket` (there is no AF_UNIX), `/etc/passwd` on the disk
  (getpwuid), `/tmp`.

25 threads, ~120 MB RSS (256 MB of WiredTiger cache reserved), 8-13 s to
`waiting for connections` in QEMU/KVM on one vCPU. MongoDB is one of the six
workloads compared with Linux on EC2 (`docs/MEASUREMENTS.md` §2). FTDC is turned off
by a parameter; `Error checking directory '/sys/block'` in the log is expected.
