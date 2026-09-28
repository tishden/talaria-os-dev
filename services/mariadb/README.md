# MariaDB 10.11 on Talaria — a multi-threaded database server (InnoDB) in ring 3

Unmodified [MariaDB 10.11.9](https://mariadb.org) (static musl,
`-Ttext-segment=0x10000000`, no dynamic plugins) runs on the Talaria kernel
(`talaria.mode=proc`): `mariadbd` brings up InnoDB (32 MB buffer pool, redo log,
`ibtmp1`), listens on `0.0.0.0:3306`, and a client speaking the MySQL protocol
from the host creates a database and an InnoDB table, writes and reads; the
data on the ext2 disk survives a VM restart. (MariaDB stands in for MySQL:
MySQL 8 as a static musl build is a separate patching project.)

Verified by `run.sh` (the `mysqlprobe.py` client from the qemu container):

```
mariadb: the server answers after 1 s
connected: server version 5.5.5-10.11.9-MariaDB
> select version()                      -> 10.11.9-MariaDB
> create database if not exists talaria -> OK
> create table t(... ) engine=innodb    -> OK
> insert into t(v) values ('talaria'),('bare metal') -> OK affected=2
> select * from t                       -> 1 talaria / 2 bare metal
```

A second run without `mkdisk.sh`: the database and the table are there,
`last_insert_id=3`; InnoDB in the serial log: `log sequence number ...;
transaction id ...`, `ready for connections`.

## Build and run

```
services/mariadb/build.sh        # Alpine docker (image talaria-alpine-build): cmake + make mariadbd
                                 #   -> build/b/sql/mariadbd; mariadb-install-db -> build/datadir
services/mariadb/mkdisk.sh 512M  # ext2: /opt/mariadb/bin/mariadbd, share/english, /data, /etc/my.cnf
services/mariadb/run.sh [secs]   # QEMU (tap) + a MySQL-protocol probe; serial -> build/serial.log
```

`my.cnf`: `socket=` (there is no AF_UNIX), `innodb-use-native-aio=0`,
`innodb-flush-method=fsync`, `innodb-buffer-pool-size=32M`,
`innodb-log-file-size=8M`, `thread-handling=one-thread-per-connection`,
`max-connections=20`. `root@'%'` without a password is created with
`mariadbd --bootstrap` (`FLUSH PRIVILEGES` first, otherwise ERROR 1290), so the
probe authenticates for real. The client is a static `mariadb` from a
client-only build (`-DWITHOUT_SERVER=ON -DWITH_SSL=system` +
`openssl-libs-static`; the in-tree build with the server pulled GnuTLS, which
Alpine has no static library for); `mysqlprobe.py` in plain Python is the
fallback.

## What it needed from the kernel (beyond PostgreSQL)

- **Threads in process mode:** `clone(CLONE_VM)` inside a private address
  space; a thread unmapping its own stack before exiting (musl's
  `__unmapself`) — done later by the scheduler, because the kernel runs the
  syscall on that stack; `rt_sigtimedwait` (the signal-handling thread),
  `FUTEX_WAKE_OP`, `/dev/urandom`, `fallocate`/`pwritev` (InnoDB writes whole
  files); new threads inherit the creator's signal mask.
- **Relative paths from a directory fd:** Aria opens table files with
  `openat(dirfd, "servers.MAI")`; ext2 remembers the path of an open directory.
- **Per-thread FPU/SSE state** — without it C++ threads corrupt each other's
  registers.
- **File mmap:** when MariaDB was first brought up, file mappings returned
  ENODEV and InnoDB fell back to pwrite/fdatasync. File `MAP_SHARED` is real
  now (pages of the ext2 block cache, write-back by the dirty bit), but it has
  NOT been re-checked with InnoDB; start with the memory-mapped redo log if you do.

MariaDB is one of the six workloads compared with Linux on EC2
(`docs/TABLO-41.md`, as `mysql`). `feedback plugin: failed to retrieve the MAC
address` in the log is expected (the plugin is off).
