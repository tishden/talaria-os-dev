# SQLite on Talaria — an open-source database engine in ring 3

Unmodified [SQLite](https://sqlite.org) 3.46.1 (the amalgamation), built
statically against musl (0x10000000), runs **in ring 3 on the Talaria kernel**
and executes SQL on an in-memory database (`:memory:` — nothing is written to
disk). It shows the "database engine" class of workloads: the SQL parser,
B-trees, aggregates, sorting.

The SQL (passed as an argument): CREATE TABLE + INSERT + aggregates
(count/sum/avg) + WHERE/ORDER BY + `sqlite_version`. Output:

```
=rows=4 sum=12.0 avg=3.0
delta=4.5
gamma=3.5
beta=2.5
SQLite 3.46.1 on Talaria OS
```

## Build and run

```
curl -sSL -o s.zip https://www.sqlite.org/2024/sqlite-amalgamation-3460100.zip
unzip s.zip
docker run --rm -v "$PWD":/w -w /w talaria-musl sh -c \
  'musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x10000000 -o sqlite3 \
     sqlite-amalgamation-*/shell.c sqlite-amalgamation-*/sqlite3.c \
     -Isqlite-amalgamation-* -DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_LOAD_EXTENSION=1 -lm'
python3 mkdisk.py disk-sqlite.img
services/sqlite/run.sh
```

Kernel mode: `talaria.mode=sqlite`. This workload needed stubs for access(21) and
the getuid family. The disk here is TFS (read-only), hence `:memory:`; for a
persistent database use ext2 (see `services/postgresql`).
