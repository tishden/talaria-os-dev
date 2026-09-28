#!/bin/sh
# Static build of MariaDB 10.11 against musl (Alpine in docker); the build tree stays in build/b.
# Targets: mariadbd (+ the install-db scripts), no client. Result: build/b/sql/mariadbd, build/datadir.
set -eu
cd "$(dirname "$0")"; mkdir -p build
docker run --rm -v "$PWD/build":/out -w /out alpine:3.20 sh -ec '
apk add --no-cache build-base cmake linux-headers bison ncurses-dev ncurses-static zlib-dev zlib-static wget tar libaio-dev file >/dev/null
V=10.11.9
[ -f /out/mariadb-$V.tar.gz ] || wget -q -O /out/mariadb-$V.tar.gz https://archive.mariadb.org/mariadb-$V/source/mariadb-$V.tar.gz
[ -d /out/mariadb-$V ] || tar xzf /out/mariadb-$V.tar.gz -C /out
mkdir -p /out/b && cd /out/b
[ -f CMakeCache.txt ] || cmake ../mariadb-$V -DCMAKE_BUILD_TYPE=RelWithDebInfo -DCMAKE_INSTALL_PREFIX=/opt/mariadb \
  -DCMAKE_EXE_LINKER_FLAGS="-static -no-pie -Wl,-Ttext-segment=0x10000000" \
  -DWITHOUT_DYNAMIC_PLUGINS=ON -DWITH_EMBEDDED_SERVER=OFF -DWITH_UNIT_TESTS=OFF -DWITH_MARIABACKUP=OFF \
  -DWITH_SSL=bundled -DWITH_ZLIB=bundled -DWITH_PCRE=bundled -DWITH_SYSTEMD=no -DWITH_JEMALLOC=no \
  -DWITH_LIBWRAP=OFF -DWITH_URING=OFF -DWITH_LIBAIO=ON -DWITH_PAM=OFF \
  -DPLUGIN_ROCKSDB=NO -DPLUGIN_MROONGA=NO -DPLUGIN_SPIDER=NO -DPLUGIN_SPHINX=NO -DPLUGIN_CONNECT=NO \
  -DPLUGIN_OQGRAPH=NO -DPLUGIN_S3=NO -DPLUGIN_COLUMNSTORE=NO -DPLUGIN_FEDERATED=NO -DPLUGIN_FEDERATEDX=NO \
  -DPLUGIN_AUTH_PAM=NO -DPLUGIN_AUTH_GSSAPI=NO -DPLUGIN_AUTH_GSSAPI_CLIENT=OFF -DPLUGIN_PERFSCHEMA=NO \
  -DPLUGIN_TOKUDB=NO -DPLUGIN_CRACKLIB_PASSWORD_CHECK=NO -DPLUGIN_EXAMPLE=NO -DPLUGIN_TEST_SQL_DISCOVERY=NO \
  -DWITH_WSREP=OFF -DSKIP_TESTS=ON -DINSTALL_MYSQLTESTDIR= 2>&1 | tail -3
make -j8 mariadbd 2>&1 | grep -E "error|Error|Linking.*mariadbd" | tail -20 || true
ls -la sql/mariadbd && file sql/mariadbd
adduser -D mdb 2>/dev/null || true; rm -rf /out/datadir; mkdir -p /out/datadir; chown -R mdb /out/datadir /out/b
su mdb -c "cd /out/b && scripts/mariadb-install-db --srcdir=/out/mariadb-$V --builddir=/out/b --datadir=/out/datadir --auth-root-authentication-method=normal --skip-test-db" >/out/install-db.log 2>&1 || { tail -30 /out/install-db.log; }
du -sh /out/datadir; find /out/datadir | wc -l
chown -R 1000:1000 /out
echo BUILD-OK
'
