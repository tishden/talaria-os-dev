# Open source on Talaria — real programs in ring 3

A demonstration of Linux ABI compatibility: unmodified, popular open-source
programs run **in ring 3 on the Talaria kernel** (no Linux underneath), through
the kernel's subset of Linux syscalls, its network stack and virtio/NVMe
devices. Different classes of workload:

| Service | Class | What it proves | Kernel mode |
|--------|-------|----------------|-----------|
| [mongoose](mongoose/) 7.23 | network web server | epoll, sockets, TCP, HTTP responses | `talaria.mode=mongoose` |
| [lua](lua/) 5.4.7 | interpreter | compute, file I/O, math and strings | `talaria.mode=lua` |
| [sqlite](sqlite/) 3.46.1 | database engine | SQL parser, B-trees, aggregates | `talaria.mode=sqlite` |
| [busybox](busybox/) 1.36.1 | multi-tool | the awk applet (compute, strings) | `talaria.mode=busybox` |
| [quickjs](quickjs/) (ng) | JavaScript engine | JS VM, GC, JSON, Math | `talaria.mode=quickjs` |
| [redis](redis/) 7.2.5 | **server database** | RESP over TCP, epoll, strings/hashes/lists/sets | `talaria.mode=redis` |
| [nginx](nginx/) 1.24.0 | web server | the HTTP path with and without the file system | `talaria.mode=cell` (manifest) |
| [postgresql](postgresql/) 16.4 | **multi-process database** | postmaster + forked backends, signals/pipes/shm, ext2 RW, WAL, psql from the host, data survives a restart | `talaria.mode=proc` (manifest) |
| [mariadb](mariadb/) 10.11.9 | **multi-threaded database (InnoDB)** | threads in a process address space, InnoDB redo log and buffer pool on ext2, the MySQL protocol, persistence | `talaria.mode=proc` (manifest) |
| [mongodb](mongodb/) 4.0.5 | **document database, dynamic ELF** | ld-musl + 18 shared objects via file mmap/MAP_FIXED, WiredTiger on ext2, OP_MSG insert/find, eventfd/timerfd, persistence | `talaria.mode=proc` (manifest) |
| [jvm](jvm/) OpenJDK 21 | **managed runtime** | JIT, GC, CDS, threads, synchronous signals | `talaria.mode=proc` (manifest) |
| [clickhouse](clickhouse/) 25.8 | **analytics engine** | the official binary, vectorised execution, MergeTree | `talaria.mode=proc` (manifest) |
| [actors](actors/) SObjectizer-5 | actor framework | cross-core wakeups, the scheduler, libtsync | `talaria.mode=proc` |

Each directory has a `README.md`, a `mkdisk.py`/`mkdisk.sh` (packing into TFS1
or ext2) and a `run.sh`. Binaries and disk images are build artefacts (in
`.gitignore`), rebuilt from sources that are downloaded by the scripts. The
common approach: link at `0x10000000` (the Talaria user region),
`musl-gcc -static -no-pie`, run from disk. `docker/` holds the toolchain images
(`build-images.sh`).

**arm64.** `services/build-aarch64.sh` cross-builds Lua, SQLite, QuickJS, BusyBox,
mongoose and Redis with the aarch64 musl toolchain (`aarch64-linux-musl-gcc` on the PATH),
linked at `0x50000000` (RAM on `virt` and Graviton; x86's `0x10000000` is a PCIe window
there), into `kernel/target/a64-services/`; `kernel/test-aarch64.sh` runs them when they are
there. Redis on arm64 runs as a process (`talaria.mode=proc`, `ext2-redis.img` with a
manifest): at start-up it forks to check the kernel for the arm64 dirty-bit-across-fork bug.

The large ones on arm64 are the distributions' own aarch64 builds, unmodified:
`services/build-aarch64-db.sh` unpacks Alpine's packages (PostgreSQL 16, MariaDB 10.11 from
3.20; MongoDB 4.0.5 from 3.9 — x86's release) with `apk --arch aarch64 --no-scripts` in an
x86 container (nothing is executed, so no emulation and no arm64 builder), takes Temurin 21
for alpine-linux aarch64 (x86's build of the same release) and packs ext2 volumes with
manifests. The PostgreSQL and MariaDB data directories are the x86 builds' (both machines are
little-endian with the same alignment; `pg_control` checks it). `kernel/test-aarch64-db.sh`
is the arm64 twin of `validate.sh` B10-B11: each server from its volume in
`talaria.mode=proc`, answered by a Python client (`postgresql/pgprobe.py`,
`mariadb/mysqlprobe.py`, `mongodb/mongoprobe.py` — no x86 binary, so the arm64 stand runs it
too). ClickHouse is not there yet: its arm64 ET_EXEC wants the low 458 MB of the address
space, where this kernel keeps its identity map (`docs/ARM64-PLAN.md` §10.13).

## Results

- **mongoose:** serves HTTP over TCP with method/URI/Host parsing and keep-alive.
- **lua:** `fib(30)=832040`, table sorting, strings, float math, `sum(1..1e6)` —
  the whole script runs without a crash.
- **sqlite:** in-memory `CREATE/INSERT` + aggregates (`count/sum/avg`) +
  `WHERE/ORDER BY` + `sqlite_version()` — a real SQL engine answers.
- **busybox:** the `awk` applet computes `sum(i^2,1..1000)=333833500` and string functions.
- **quickjs:** fib, map/filter/reduce, JSON.stringify, Math — a complete JS runtime.
- **redis:** a real in-memory server database — PING/SET/GET/INCR, hashes,
  lists, sets, DBSIZE, INFO (an epoll event loop), answering RESP on TCP 6379.
- The databases, the JVM and ClickHouse are covered by `kernel/validate.sh`
  (B10-B12), and six server workloads are compared with Linux on EC2 by
  `bench/suite` (`docs/MEASUREMENTS.md` §2).

## The full process life cycle (fork + execve + wait4)

`talaria.mode=procdemo` — the launcher (`kernel/user/launcher.c`) calls `fork()`;
the child does `execve("/busybox", ...)` (the real busybox from disk) and the
parent `wait4()`:

```
[launcher] fork -> child execve /busybox, parent wait4
[child] busybox awk via fork+execve on Talaria: 6*7=42
[launcher] child exited, code=0
[launcher] full process cycle (fork+execve+wait4) on Talaria OK
```

This proves the full life cycle of a process on this kernel: the address space
is copied on fork (COW), the image is replaced on execve (the ELF is loaded
from disk into a new address space with a CR3 switch), and the parent waits for
the child. Zero changes to the programs' sources; missing syscalls were added
to the kernel as they were found.

## More candidates

Suitable for this profile (static musl, a syscall subset, epoll networking,
fork/exec):

- **Interpreters:** micropython, wren, janet, tcl, mruby, berry.
- **Web and network (epoll):** civetweb, libmicrohttpd.
- **Data and utilities:** jq, zlib/gzip and xz, miniz, bc/dc.
- **Crypto and hashes:** monocypher, tweetnacl, blake3.
- **Benchmarks:** coremark, dhrystone.
- **Through fork/exec:** toybox, simple shell pipelines.
