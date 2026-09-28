#!/bin/sh
# Static build of PostgreSQL 16 against musl for Talaria (Alpine in docker); the tree stays in build/src.
set -eu
cd "$(dirname "$0")"; mkdir -p build
docker run --rm -v "$PWD/build":/out -w /out alpine:3.20 sh -ec '
apk add --no-cache build-base linux-headers bison flex perl tar file >/dev/null
[ -d /out/src ] || { mkdir -p /out/src && tar xzf /out/postgresql-16.4.tar.gz -C /out/src --strip-components=1; }
cd /out/src
rm -rf b-st && mkdir b-st && cd b-st
../configure --prefix=/opt/pg --without-readline --without-icu --without-zlib CFLAGS="-O2" LDFLAGS="-static -no-pie -Wl,-Ttext-segment=0x10000000" 2>&1 | tail -3
sed -i "s|^#define HAVE_SYS_SIGNALFD_H 1|/* #undef HAVE_SYS_SIGNALFD_H */|" src/include/pg_config.h
grep -n SIGNALFD src/include/pg_config.h
make -j8 2>&1 | grep -E "error|Error|warning: Using|undefined" | head -40 || true
ls -la src/backend/postgres && file src/backend/postgres
rm -rf /out/pg-static; make install DESTDIR=/out/pg-static 2>&1 | tail -2
chown -R 1000:1000 /out
du -sh /out/pg-static
echo STATIC-OK
'
