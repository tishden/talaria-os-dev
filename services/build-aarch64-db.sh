#!/bin/sh
# The large services for arm64 (docs/ARM64-PLAN.md phase 7, its second step): PostgreSQL 16,
# MariaDB 10.11, MongoDB 4.0.5, OpenJDK 21 and ClickHouse — the distributions' OWN aarch64
# builds, unmodified, as on x86 where MongoDB, the JVM and ClickHouse are ready-made binaries
# too. Nothing is compiled and nothing is executed on the host: an x86 Alpine container's
# `apk --arch aarch64 --root ... --no-scripts` unpacks the aarch64 packages with their
# dependency closure into a directory, the JDK and ClickHouse are their vendors' arm64
# tarballs. The plan's "arm64 builder" (a c7g with docker) is not needed for any of them.
#
#   services/build-aarch64-db.sh [pg] [mdb] [mongo] [jvm] [ch] [dyn]   (none = all)
#     -> kernel/target/a64-services/ext2-{pg,mdb,mongo,jvm,jvm-short}.img, kernel/target/disk-a64-dyn.img
#   (ch is not built yet — see its case below)
#
# The data directories. PostgreSQL's and MariaDB's are initialised by a program the host
# cannot run (an aarch64 initdb / mariadb-install-db); they are the x86 builds' ones
# (services/postgresql/build/pgdata, services/mariadb/build/datadir): both machines are
# little-endian with an 8-byte MAXALIGN and the same type sizes, so the files are the same
# bytes — PostgreSQL's pg_control checks exactly that at start-up and would refuse otherwise.
# MongoDB and ClickHouse create theirs. The downloads are cached in kernel/target/a64-services/dl.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/.." && pwd)
OUT=${OUT:-$REPO/kernel/target/a64-services}
DL=$OUT/dl
mkdir -p "$OUT" "$DL"
WHAT=${*:-dyn pg mdb mongo jvm}
ok=0; skipped=""

# apkroot DIR RELEASE PKG...: the aarch64 packages with their closure unpacked into DIR, the
# symbolic links materialised (the kernel's ext2 does not follow them), the apk database dropped.
apkroot() {
  dir=$1; rel=$2; shift 2
  rm -rf "$dir.raw" "$dir"; mkdir -p "$dir.raw" "$dir" "$DL/apk-$rel"
  docker run --rm -v "$dir.raw":/r -v "$DL":/dl alpine:3.20 sh -c "
    apk --arch aarch64 -X https://dl-cdn.alpinelinux.org/alpine/$rel/main -X https://dl-cdn.alpinelinux.org/alpine/$rel/community \
      --allow-untrusted --root /r --initdb --no-scripts --cache-dir /dl/apk-$rel add $* >/r/apk.log 2>&1; rc=\$?
    rm -rf /r/var/cache/apk /r/lib/apk /r/etc/apk; chown -R $(id -u):$(id -g) /r /dl/apk-$rel; exit \$rc" \
    || { echo "build-aarch64-db: apk failed for $* (a CDN hiccup is the usual cause — run it again):"; tail -3 "$dir.raw/apk.log"; return 1; }
  # -h: a link's target is copied under the link's name; a link to a directory, the tree
  (cd "$dir.raw" && tar -chf - --exclude=./dev --exclude=./proc --exclude=./sys . 2>/dev/null) | (cd "$dir" && tar -xf -)
  rm -rf "$dir.raw"
  [ -f "$dir/lib/ld-musl-aarch64.so.1" ]
}
# need ROOT FILE...: every file the image's manifest names is there (an unpack that went
# half-way must not pack into a volume whose binary is missing — "the binary was not found").
need() { r=$1; shift; for f in "$@"; do [ -f "$r$f" ] || { echo "build-aarch64-db: $f is not in the unpacked tree"; return 1; }; done; }
common_etc() { # common_etc ROOT USER: passwd/group/hosts for getpwuid and localhost
  mkdir -p "$1/etc" "$1/tmp" "$1/proc" "$1/sys"
  printf 'root:x:0:0:root:/:/bin/sh\n%s:x:1000:1000:%s:/:/bin/sh\n' "$2" "$2" > "$1/etc/passwd"
  printf 'root:x:0:\n%s:x:1000:\n' "$2" > "$1/etc/group"
  printf '127.0.0.1 localhost talaria\n' > "$1/etc/hosts"
  printf 'hosts: files dns\n' > "$1/etc/nsswitch.conf"
}
# trim ROOT: what a server never opens (documentation, headers, static archives, locales).
trim() { rm -rf "$1/usr/share/doc" "$1/usr/share/man" "$1/usr/include" "$1/usr/share/locale" "$1/usr/share/i18n"; find "$1" -name '*.a' -delete; }
mk() { "$REPO/kernel/mkext2.sh" "$1" "$2" "$3" >/dev/null 2>&1 && e2fsck -fn "$2" >/dev/null 2>&1; }

for w in $WHAT; do case $w in
dyn) # the gate's dynamic ELF: a PIE + ld-musl + libz through dlopen (talaria.test=dyn)
  CC=${CCA64:-aarch64-linux-musl-gcc}
  R=$OUT/root-dyn
  if command -v "$CC" >/dev/null 2>&1 && apkroot "$R" v3.20 zlib; then
    D=$(mktemp -d); mkdir -p "$D/lib"
    $CC -O2 -fPIE -pie -o "$D/hello" "$REPO/kernel/user/dyn/hello.c" \
      && cp "$R/lib/ld-musl-aarch64.so.1" "$R/lib/libz.so.1" "$D/lib/" \
      && mk "$D" "$REPO/kernel/target/disk-a64-dyn.img" 32M && ok=$((ok+1)) || echo "build-aarch64-db: dyn failed"
    rm -rf "$D" "$R"
  else skipped="$skipped dyn"; fi ;;

pg) # PostgreSQL 16: the postmaster forks a backend per connection (mode=proc, ext2 RW)
  PGD=$HERE/postgresql/build/pgdata
  if [ -d "$PGD" ]; then
    R=$OUT/root-pg
    if apkroot "$R" v3.20 postgresql16 && need "$R" /usr/libexec/postgresql16/postgres; then
      trim "$R"; common_etc "$R" postgres
      rm -rf "$R/usr/share/postgresql16/extension" "$R/usr/share/postgresql16/tsearch_data" "$R/usr/lib/postgresql16/llvmjit"*
      cp -r "$PGD" "$R/pgdata"; chmod 700 "$R/pgdata"; rm -f "$R/pgdata/postmaster.pid"
      cat > "$R/cell.manifest" <<M
# PostgreSQL 16 (Alpine aarch64, dynamic) on Talaria arm64
binary=/usr/libexec/postgresql16/postgres
args=postgres -D /pgdata
env=PGDATA=/pgdata
env=HOME=/
env=PATH=/usr/libexec/postgresql16
M
      mk "$R" "$OUT/ext2-pg.img" 384M && ok=$((ok+1)) || echo "build-aarch64-db: pg did not pack"
      rm -rf "$R"
    else echo "build-aarch64-db: the postgresql16 packages did not unpack"; fi
  else skipped="$skipped pg(services/postgresql/build.sh: the data directory)"; fi ;;

mdb) # MariaDB 10.11: threads in one process, InnoDB's redo log and buffer pool on ext2
  MDD=$HERE/mariadb/build/datadir
  if [ -d "$MDD" ]; then
    R=$OUT/root-mdb
    if apkroot "$R" v3.20 mariadb && need "$R" /usr/bin/mariadbd /usr/share/mariadb/english/errmsg.sys; then
      trim "$R"; common_etc "$R" mysql; mkdir -p "$R/run"
      cp -r "$MDD" "$R/data"
      cat > "$R/etc/my.cnf" <<M
[mysqld]
basedir=/usr
datadir=/data
tmpdir=/tmp
lc-messages-dir=/usr/share/mariadb
socket=
pid-file=/run/mariadbd.pid
bind-address=0.0.0.0
port=3306
skip-name-resolve
innodb-use-native-aio=0
innodb-flush-method=fsync
innodb-buffer-pool-size=32M
innodb-log-file-size=8M
thread-handling=one-thread-per-connection
max-connections=20
M
      cat > "$R/cell.manifest" <<M
# MariaDB 10.11 (Alpine aarch64, dynamic) on Talaria arm64
binary=/usr/bin/mariadbd
args=mariadbd --defaults-file=/etc/my.cnf
env=HOME=/
env=PATH=/usr/bin
M
      mk "$R" "$OUT/ext2-mdb.img" 512M && ok=$((ok+1)) || echo "build-aarch64-db: mdb did not pack"
      rm -rf "$R"
    else echo "build-aarch64-db: the mariadb packages did not unpack"; fi
  else skipped="$skipped mdb(services/mariadb/build.sh: the data directory)"; fi ;;

mongo) # MongoDB 4.0.5 from Alpine 3.9 — the same release as x86's: a dynamic ELF, WiredTiger
  R=$OUT/root-mongo
  if apkroot "$R" v3.9 mongodb busybox && need "$R" /usr/bin/mongod; then
    trim "$R"; common_etc "$R" mongodb; mkdir -p "$R/data/db"
    cat > "$R/cell.manifest" <<M
# MongoDB 4.0.5 (Alpine 3.9 aarch64, dynamic) on Talaria arm64
binary=/usr/bin/mongod
args=mongod --dbpath /data/db --bind_ip_all --port 27017 --wiredTigerCacheSizeGB 0.25 --setParameter diagnosticDataCollectionEnabled=false --setParameter disableLogicalSessionCacheRefresh=true --nounixsocket
env=HOME=/
env=LD_LIBRARY_PATH=/usr/lib:/lib
env=PATH=/usr/bin:/bin
M
    mk "$R" "$OUT/ext2-mongo.img" 512M && ok=$((ok+1)) || echo "build-aarch64-db: mongo did not pack"
    rm -rf "$R"
  else echo "build-aarch64-db: the mongodb packages (Alpine 3.9 aarch64) did not unpack"; fi ;;

jvm) # Temurin 21 for alpine-linux aarch64 (x86 runs the x64 one of the same build) + the bench classes
  T=$DL/jdk21-musl-aarch64.tar.gz
  [ -s "$T" ] || curl -sfL -o "$T" "https://api.adoptium.net/v3/binary/latest/21/ga/alpine-linux/aarch64/jdk/hotspot/normal/eclipse" || rm -f "$T"
  if [ -s "$T" ] && [ -d "$HERE/jvm/build/app" ]; then
    R=$OUT/root-jvm; rm -rf "$R"; mkdir -p "$R/jdk" "$R/lib" "$R/app"
    J=$(mktemp -d); tar -C "$J" -xzf "$T"; JDK=$(ls -d "$J"/jdk-21* | head -1)
    cp -r "$JDK/bin" "$JDK/lib" "$JDK/conf" "$JDK/release" "$R/jdk/"; rm -rf "$J"
    rm -rf "$R/jdk/lib/src.zip" "$R/jdk/lib/ct.sym" "$R/jdk/lib/jfr" "$R/jdk/lib/security/public_suffix_list.dat"
    rm -f "$R/jdk/bin/jar" "$R/jdk/bin/javac" "$R/jdk/bin/jlink" "$R/jdk/bin/jmod" "$R/jdk/bin/jpackage"
    M=$OUT/root-musl; apkroot "$M" v3.20 musl zlib && cp "$M/lib/ld-musl-aarch64.so.1" "$M/lib/libz.so.1" "$R/lib/" \
      && cp "$R/lib/ld-musl-aarch64.so.1" "$R/lib/libc.musl-aarch64.so.1"; rm -rf "$M"
    # the class files are the bytecode x86 runs (services/jvm/build-bench.sh)
    cp -r "$HERE/jvm/build/app/." "$R/app/"
    common_etc "$R" java
    ARGS=${JVM_ARGS:-"-XX:+UseSerialGC -Xms32m -Xmx256m -XX:ReservedCodeCacheSize=48m -XX:CompressedClassSpaceSize=32m -XX:MaxMetaspaceSize=128m -XX:-UsePerfData -Djava.io.tmpdir=/tmp -Djava.class.path=/app"}
    # Two volumes, one JDK: the whole set (validate.sh B11's, for the KVM stand) and a short one
    # TCG finishes in minutes (the start, the interpreter and the JIT, a cross-thread park).
    jvm_img() { # jvm_img MAIN IMAGE
      cat > "$R/cell.manifest" <<M
# JVM (Temurin 21, musl, aarch64) on Talaria arm64
binary=/jdk/bin/java
args=java $ARGS $1
env=JAVA_HOME=/jdk
env=LD_LIBRARY_PATH=/jdk/lib:/jdk/lib/server:/lib
env=PATH=/jdk/bin
env=HOME=/
env=TZ=UTC
M
      mk "$R" "$OUT/$2" 900M
    }
    jvm_img "${JVM_MAIN:-Bench all}" ext2-jvm.img && jvm_img "Bench info sieve pingpong" ext2-jvm-short.img \
      && ok=$((ok+1)) || echo "build-aarch64-db: jvm did not pack"
    rm -rf "$R"
  else skipped="$skipped jvm(the Temurin download, services/jvm/build-bench.sh)"; fi ;;

ch) # ClickHouse's official arm64 binary is an ET_EXEC at 0x200000..0x1CA9_7310 (458 MB): there
  # every process of this kernel has the identity map — the devices on `virt` (the GIC, the
  # UART, virtio-mmio, the 32-bit PCIe window), RAM on a Graviton. x86 moved its kernel above
  # the binary (services/clickhouse/build-kernel.sh); arm64 needs the kernel's view of
  # devices and RAM off the low addresses (docs/ARM64-PLAN.md §10.13, "the direct map").
  skipped="$skipped ch(the address-space layout, ARM64-PLAN §10.13)" ;;
*) echo "build-aarch64-db: unknown '$w' (dyn pg mdb mongo jvm ch)"; exit 1 ;;
esac; done

echo "build-aarch64-db: $ok image(s) in $OUT${skipped:+; skipped:$skipped}"
