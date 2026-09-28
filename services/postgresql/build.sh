#!/bin/sh
# Build PostgreSQL 16 against musl (Alpine in docker): a dynamic build (for initdb in the
# container) and a static one (for Talaria, linked at 0x10000000). Result: build/pg-static/
# (the /opt/pg install tree) and build/pgdata/ (initdb).
set -eu
cd "$(dirname "$0")"
mkdir -p build
docker run --rm -v "$PWD/build":/out -w /tmp alpine:3.20 sh -ec '
apk add --no-cache build-base linux-headers bison flex perl wget tar >/dev/null
[ -f /out/postgresql-16.4.tar.gz ] || wget -q -O /out/postgresql-16.4.tar.gz https://ftp.postgresql.org/pub/source/v16.4/postgresql-16.4.tar.gz
tar xzf /out/postgresql-16.4.tar.gz
cd postgresql-16.4
# 1) dynamic build — initdb in the container
mkdir b-dyn && cd b-dyn
../configure --prefix=/opt/pg --without-readline --without-icu --without-zlib >/dev/null
make -j8 >/dev/null 2>&1 && make install >/dev/null
cd ..
# 2) static build for Talaria
mkdir b-st && cd b-st
../configure --prefix=/opt/pg --without-readline --without-icu --without-zlib CFLAGS="-O2" LDFLAGS="-static -no-pie -Wl,-Ttext-segment=0x10000000" >/dev/null
# latch: without signalfd -> self-pipe (the latch is built on signals + a pipe)
sed -i "s|^#define HAVE_SYS_SIGNALFD_H 1|/* #undef HAVE_SYS_SIGNALFD_H */|" src/include/pg_config.h
make -j8 >/dev/null 2>&1 && make install DESTDIR=/out/pg-static >/dev/null
file /out/pg-static/opt/pg/bin/postgres
# 3) initdb (not as root)
adduser -D pg
rm -rf /out/pgdata; mkdir -p /out/pgdata; chown pg /out/pgdata
su pg -c "/opt/pg/bin/initdb -D /out/pgdata -U postgres --auth=trust --locale=C -E UTF8" >/out/initdb.log 2>&1 || { tail -20 /out/initdb.log; exit 1; }
cat >> /out/pgdata/postgresql.conf <<CONF
listen_addresses = '"'"'*'"'"'
unix_socket_directories = '"'"''"'"'
shared_buffers = 32MB
max_connections = 20
dynamic_shared_memory_type = mmap
timezone = '"'"'UTC'"'"'
log_timezone = '"'"'UTC'"'"'
logging_collector = off
autovacuum = off
huge_pages = off
wal_level = minimal
max_wal_senders = 0
CONF
echo "host all all 0.0.0.0/0 trust" >> /out/pgdata/pg_hba.conf
chown -R 1000:1000 /out; chmod 0700 /out/pgdata
du -sh /out/pg-static /out/pgdata; find /out/pgdata -type f | wc -l
echo BUILD-OK
'
