#!/bin/sh
# The small services for arm64 (docs/ARM64-PLAN.md phase 7, its first step): Lua, SQLite,
# QuickJS, BusyBox, mongoose and Redis, cross-built with the aarch64 musl toolchain from the sources each
# service's README downloads, and packed into the same TFS1 disks as on x86.
#
#   services/build-aarch64.sh            -> kernel/target/a64-services/disk-{lua,sqlite,quickjs,busybox,mongoose}.img,
#                                           ext2-redis.img (mkfs.ext2 needed)
#
# The compiler is `aarch64-linux-musl-gcc` from the PATH (musl.cc's aarch64-linux-musl-cross;
# CCA64= names another). The link address is 0x5000_0000, as for kernel/user/aarch64: the
# loader keeps a static binary at its link address, and x86's 0x1000_0000 is a PCIe window
# on `virt` and on Graviton. A service whose sources are not here is skipped with the
# command that fetches them.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT:-$HERE/../kernel/target/a64-services}
CC=${CCA64:-aarch64-linux-musl-gcc}
command -v "$CC" >/dev/null 2>&1 || { echo "build-aarch64: no $CC on the PATH (musl.cc aarch64-linux-musl-cross)"; exit 1; }
LD="-static -no-pie -Wl,-Ttext-segment=0x50000000"
STRIP=$(dirname "$(command -v "$CC")")/aarch64-linux-musl-strip
mkdir -p "$OUT"
ok=0; skipped=""

# pack NAME FILE...: run the service's own mkdisk.py over copies under the names it expects.
pack() {
  name=$1; shift
  d=$(mktemp -d)
  for f in "$@"; do cp "$f" "$d/"; done
  (cd "$d" && python3 "$HERE/$name/mkdisk.py" "$OUT/disk-$name.img") || return 1
  rm -rf "$d"
}

if [ -d "$HERE/lua/lua-5.4.7/src" ]; then
  $CC -O2 $LD -DLUA_USE_POSIX -I "$HERE/lua/lua-5.4.7/src" -o "$OUT/lua" \
    $(ls "$HERE"/lua/lua-5.4.7/src/*.c | grep -v luac.c) -lm \
    && "$STRIP" "$OUT/lua" && cp "$HERE/lua/script.lua" "$OUT/" && pack lua "$OUT/lua" "$OUT/script.lua" && ok=$((ok+1))
else skipped="$skipped lua(services/lua/README.md)"; fi

SQ=$(ls -d "$HERE"/sqlite/sqlite-amalgamation-* 2>/dev/null | head -1)
if [ -n "$SQ" ]; then
  $CC -O2 $LD -o "$OUT/sqlite3" "$SQ/shell.c" "$SQ/sqlite3.c" -I"$SQ" \
    -DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_LOAD_EXTENSION=1 -lm \
    && "$STRIP" "$OUT/sqlite3" && pack sqlite "$OUT/sqlite3" && ok=$((ok+1))
else skipped="$skipped sqlite(services/sqlite/README.md)"; fi

if [ -f "$HERE/quickjs/src/quickjs.c" ]; then
  Q=$HERE/quickjs
  $CC -O2 $LD -D_GNU_SOURCE -I"$Q/src" -o "$OUT/qjs" \
    "$Q/src/qjs.c" "$Q/src/quickjs.c" "$Q/src/libregexp.c" "$Q/src/libunicode.c" "$Q/src/dtoa.c" \
    "$Q/src/quickjs-libc.c" "$Q/stub.c" -lm \
    && "$STRIP" "$OUT/qjs" && cp "$OUT/qjs" "$OUT/qjs_bin" && cp "$Q/demo.js" "$OUT/" \
    && pack quickjs "$OUT/qjs_bin" "$OUT/demo.js" && ok=$((ok+1))
else skipped="$skipped quickjs(services/quickjs/README.md)"; fi

if [ -f "$HERE/busybox/bb.tar.bz2" ]; then
  B=$(mktemp -d)
  tar -C "$B" -xjf "$HERE/busybox/bb.tar.bz2"
  (cd "$B"/busybox-1.36.1 && make -s allnoconfig >/dev/null 2>&1 \
    && for a in ECHO PRINTF CAT AWK SORT SHA256SUM WC SEQ EXPR HEAD TAIL GREP TR CUT UNAME; do
         sed -i "s/# CONFIG_$a is not set/CONFIG_$a=y/" .config; done \
    && sed -i 's/# CONFIG_STATIC is not set/CONFIG_STATIC=y/' .config \
    && sed -i 's|CONFIG_EXTRA_LDFLAGS=""|CONFIG_EXTRA_LDFLAGS="-Wl,-Ttext-segment=0x50000000"|' .config \
    && (yes "" | make -s oldconfig >/dev/null 2>&1) \
    && make -s -j4 CC="$CC" HOSTCC=gcc STRIP="$STRIP" >/dev/null 2>&1) \
    && cp "$B"/busybox-1.36.1/busybox "$OUT/busybox_srv" && pack busybox "$OUT/busybox_srv" && ok=$((ok+1))
  rm -rf "$B"
else skipped="$skipped busybox(services/busybox/README.md)"; fi

if [ -f "$HERE/mongoose/mongoose.c" ]; then
  # MG_ENABLE_LOG=0 as on x86: with the log every request prints two lines to the console.
  $CC -O2 $LD -o "$OUT/mongoose_srv" "$HERE/mongoose/server.c" "$HERE/mongoose/mongoose.c" \
    -DMG_ENABLE_EPOLL=1 -DMG_ENABLE_POLL=0 -DMG_ENABLE_LOG=0 \
    && "$STRIP" "$OUT/mongoose_srv" && pack mongoose "$OUT/mongoose_srv" "$HERE/mongoose/index.html" && ok=$((ok+1))
else skipped="$skipped mongoose(services/mongoose/fetch.sh)"; fi

# Redis on arm64 runs as a PROCESS (`talaria.mode=proc`, an ext2 volume with a manifest): at
# start-up it forks to test the kernel for the arm64 dirty-bit-across-fork bug (Linux's
# ff1712f953e2) and reads the child's /proc/self/smaps; `talaria.mode=redis` is a single
# image with no process model, where that fork is ENOSYS and Redis refuses to run.
redis_ext2() {
  d=$(mktemp -d)
  cp "$OUT/redis-server" "$HERE/redis/redis.conf" "$d/"
  printf 'binary=/redis-server\nargs=redis-server /redis.conf\n' > "$d/cell.manifest"
  "$HERE/../kernel/mkext2.sh" "$d" "$OUT/ext2-redis.img" 64M >/dev/null 2>&1; rc=$?
  rm -rf "$d"; return $rc
}
if [ -f "$HERE/redis/src/Makefile" ]; then
  # A copy of the tree: x86's objects stay where they are. The deps are archived by the
  # cross ar (the host's may not index an aarch64 object); USE_SYSTEMD=no, or the host's
  # pkg-config finds ITS libsystemd and the build wants sd-daemon.h.
  R=$(mktemp -d); cp -a "$HERE/redis/src/." "$R/"
  (cd "$R" && make -s distclean >/dev/null 2>&1; make -s -j4 redis-server USE_SYSTEMD=no MALLOC=libc CC="$CC" \
      AR="${CC%gcc}ar" RANLIB="${CC%gcc}ranlib" CFLAGS="-O2 -w" LDFLAGS="$LD" >/dev/null 2>&1) \
    && cp "$R/src/redis-server" "$OUT/redis-server" && "$STRIP" "$OUT/redis-server" \
    && redis_ext2 && ok=$((ok+1)) || echo "build-aarch64: redis did not build or pack (the build tree: $R)"
  [ -f "$OUT/redis-server" ] && rm -rf "$R"
else skipped="$skipped redis(services/redis/README.md)"; fi

echo "build-aarch64: $ok service(s) built into $OUT${skipped:+; skipped (no sources):$skipped}"
