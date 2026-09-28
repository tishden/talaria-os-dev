# Redis on Talaria — a server database in ring 3

Unmodified [Redis](https://redis.io) 7.2.5, built statically against musl
(`MALLOC=libc`, linked at 0x10000000), runs **in ring 3 on the Talaria kernel** —
a real in-memory server database answering the RESP protocol on TCP port 6379
through its own event loop (ae/epoll → the kernel's network stack).

Verified with raw RESP over TCP from the network:

```
PING                         -> +PONG
SET talaria "on bare metal"  -> +OK
GET talaria                  -> "on bare metal"
INCR hits                    -> :1
HSET user name alice; HGETALL user -> [name, alice]
LPUSH q a b c; LRANGE q 0 -1 -> [c, b, a]
SADD/SCARD, DBSIZE           -> work
INFO server                  -> redis_version:7.2.5, mode:standalone, multiplexing_api:epoll
```

Strings, hashes, lists, sets, key expiry — a complete Redis.

## Build and run

```
curl -sSL https://download.redis.io/releases/redis-7.2.5.tar.gz | tar xz
mv redis-7.2.5 src
docker run --rm -v "$PWD/src":/w -w /w talaria-musl sh -c \
  'make -j4 redis-server MALLOC=libc CC=musl-gcc CFLAGS="-O2 -w" \
     LDFLAGS="-static -no-pie -Wl,-Ttext-segment=0x10000000"'
cp src/src/redis-server redis-server && python3 mkdisk.py disk-redis.img
services/redis/run.sh
```

`redis.conf` (on the disk): port 6379, bind 0.0.0.0, protected-mode no,
save "", appendonly no. Kernel mode: `talaria.mode=redis` (runs the binary
from disk). The comparison with Linux in `bench/suite` uses the same binary.

## What it needed from the kernel

Redis uses an epoll loop, socket/bind/listen/accept4, clock_gettime and
mmap/brk (musl malloc). Stubs were added for uname(63), umask(95),
setitimer(38) and sysinfo(99). Persistence (RDB/AOF, the BGSAVE fork) is not
used in this demo — the database is in memory on a read-only TFS disk. Zero
changes to the Redis sources.
