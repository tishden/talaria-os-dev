# PostgreSQL 16 on Talaria — a real database server in ring 3

Unmodified [PostgreSQL 16.4](https://www.postgresql.org) (static musl, linked at
`0x10000000`) runs **as a multi-process server on the Talaria kernel**
(`talaria.mode=proc`): the postmaster listens on `:5432` and forks the
checkpointer, bgwriter, walwriter and backends; `psql` from the host runs DDL
and DML; the data lives on a **writable ext2 disk** and survives a hard restart
of the VM (WAL + crash recovery).

Verified by `run.sh` (a static psql from the qemu container, tap 10.0.0.2):

```
psql: the server answers after 1 s
 PostgreSQL 16.4 on x86_64-pc-linux-musl, compiled by gcc (Alpine 13.2.1) 13.2.1 20240309, 64-bit
CREATE TABLE
INSERT 0 2
 id |     v
----+------------
  1 | talaria
  2 | bare metal
(2 rows)
```

A second `run.sh` **without** rebuilding the disk: rows 1-2 are still there
(`INSERT` gives 3-4); the serial log shows `database system was shut down at
...` / crash recovery and `checkpoint complete` every 5 minutes; after the VM is
killed, `e2fsck -fn disk-pg.img` is clean.

## Build and run

```
services/postgresql/build.sh         # Alpine docker: dynamic build + initdb (build/pgdata)
services/postgresql/build-static.sh  # static postgres (+ psql, see below) -> build/pg-static
services/postgresql/mkdisk.sh 256M   # ext2 image: /opt/pg (postgres, share), /pgdata, /cell.manifest
services/postgresql/run.sh [secs]    # QEMU (tap) + psql probe; serial -> build/serial.log
```

Static `psql`: in the tree `build/src/b-st` the `submake-libpq` target is
redirected to `libpq.a` (shared libraries do not link with `-static`), then
`make -C src/bin/psql`. The initdb data is produced by the dynamic build in the
container (static musl without `dlopen` cannot load `plpgsql.so` during
initdb) — the data format of one version is the same.

`postgresql.conf` (in `build/pgdata`): `listen_addresses='*'`,
`unix_socket_directories=''` (there is no AF_UNIX), `shared_buffers=32MB`,
`max_connections=20`, `huge_pages=off`, `log_connections=on`; `pg_hba.conf`:
`host all all 0.0.0.0/0 trust`. In `pg_config.h` of the static build
`HAVE_SYS_SIGNALFD_H` is removed, so the latch uses a self-pipe.

## What it needed from the kernel

- **A writable file system:** ext2 (`ext2.rs`) with
  create/pread/pwrite/preadv/pwritev/ftruncate/fallocate/rename/unlink/mkdir/
  rmdir/getdents64/fsync; the image stays compatible with e2fsprogs.
- **A process model:** per-process fd tables with reference counts (the
  postmaster closes the listening socket in a child, and the parent keeps it),
  `dup2`, real `pipe`s (the postmaster-death pipe, the self-pipe latch), `fork`
  with COW and inherited registers/TLS/FPU state, `getpid/getppid`, `wait4`
  statuses, `SIGCHLD`/`SIGUSR1`/`SIGTERM` (`rt_sigaction`, `kill`,
  `rt_sigreturn`, SA_RESTART), `cwd` (PostgreSQL does `chdir(PGDATA)` and opens
  relative paths), `MAP_SHARED|MAP_ANONYMOUS` shared across fork
  (shared_buffers), SysV `shmget/shmat` (the interlock segment), POSIX shm
  (`/dev/shm/PostgreSQL.*`), `select` (`pg_usleep`), `/dev/urandom`, and
  `getuid()=1000` (PostgreSQL refuses to run as root).
- **Kernel bugs found on PostgreSQL:** SSE/x87 state saved across syscalls,
  interrupts and task switches; `CR0.WP` plus an eager copy of the user stack on
  fork (the kernel wrote to a COW page of the shared frame); COW reference
  counts on a repeated fork; the identity range of the image reserved from the
  allocator.

The postmaster plus 5 background processes plus one backend per connection;
~35 MB of ext2 for the data; about 1 s to `ready to accept connections` in
QEMU/KVM. PostgreSQL is one of the six workloads compared with Linux on EC2
(`docs/MEASUREMENTS.md` §2). `could not create IPv6 socket` in the log is expected:
there is no AF_INET6.
