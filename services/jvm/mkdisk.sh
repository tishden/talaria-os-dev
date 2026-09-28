#!/bin/sh
# The JVM disk (Eclipse Temurin 21, the musl build for Alpine) for Talaria:
#   /jdk           — a trimmed JDK (bin/lib/conf; no jmods/src.zip/ct.sym)
#   /lib/ld-musl-x86_64.so.1 + /lib/libc.musl-x86_64.so.1 — the musl dynamic loader
#   /app/*.class   — the benchmarks (compiled on the host in an Alpine container, see build-bench.sh)
#   /cell.manifest — what to run (JVM_ARGS/JVM_MAIN override it)
# mkdisk.sh [size]   env: JVM_MAIN=Bench JVM_ARGS="-Xmx256m ..." TAL_MUSL=0
set -eu
cd "$(dirname "$0")"
JDK=$(ls -d build/jdk-21* | head -1)
[ -d "$JDK" ] || { echo "no $JDK — unpack the JDK into build/"; exit 1; }
ROOT=$(mktemp -d)
mkdir -p "$ROOT/jdk" "$ROOT/lib" "$ROOT/tmp" "$ROOT/etc" "$ROOT/app" "$ROOT/proc" "$ROOT/sys"
cp -r "$JDK/bin" "$JDK/lib" "$JDK/conf" "$JDK/release" "$ROOT/jdk/"
# what the JVM never opens at runtime (compiler, sources, debugging)
rm -rf "$ROOT/jdk/lib/src.zip" "$ROOT/jdk/lib/ct.sym" "$ROOT/jdk/lib/jfr" "$ROOT/jdk/lib/security/public_suffix_list.dat"
rm -f "$ROOT/jdk/bin/jar" "$ROOT/jdk/bin/javac" "$ROOT/jdk/bin/jlink" "$ROOT/jdk/bin/jmod" "$ROOT/jdk/bin/jpackage"

# musl: the Alpine JDK build is linked against libc.musl-x86_64.so.1 (the same ld-musl).
# By default the PATCHED ld-musl 1.2.5 is used (PLAN-SYNC tier 1: futex paths via
# umwait where the hardware can). TAL_MUSL=0 takes the libc from the Alpine image.
MUSL=../../kernel/user/dyn/ld-musl-x86_64.so.1
if [ "${TAL_MUSL:-1}" = 1 ] && [ -f "$MUSL" ]; then
  cp "$MUSL" "$ROOT/lib/ld-musl-x86_64.so.1"
  echo "mkdisk: ld-musl — patched (PLAN-SYNC; TAL_MUSL=0 takes the one from Alpine)"
else
  docker run --rm alpine:3.20 cat /lib/ld-musl-x86_64.so.1 > "$ROOT/lib/ld-musl-x86_64.so.1"
fi
cp "$ROOT/lib/ld-musl-x86_64.so.1" "$ROOT/lib/libc.musl-x86_64.so.1"  # no symlinks in the image — a copy

[ -d build/app ] && cp -r build/app/. "$ROOT/app/" || true

printf 'root:x:0:0:root:/:/bin/sh\njava:x:1000:1000:java:/:/bin/sh\n' > "$ROOT/etc/passwd"
printf 'root:x:0:\njava:x:1000:\n' > "$ROOT/etc/group"
printf '127.0.0.1 localhost talaria\n' > "$ROOT/etc/hosts"
printf 'hosts: files dns\n' > "$ROOT/etc/nsswitch.conf"

MAIN=${JVM_MAIN:-"-version"}
# Defaults chosen for Talaria: SerialGC (fewer service threads), small
# reservations, no hsperfdata (an mmap of a /tmp file).
ARGS=${JVM_ARGS:-"-XX:+UseSerialGC -Xms32m -Xmx256m -XX:ReservedCodeCacheSize=48m -XX:CompressedClassSpaceSize=32m -XX:MaxMetaspaceSize=128m -XX:-UsePerfData -Djava.io.tmpdir=/tmp -Djava.class.path=/app"}
cat > "$ROOT/cell.manifest" <<M
# JVM (Temurin 21, musl) on Talaria
binary=/jdk/bin/java
args=java $ARGS $MAIN
env=JAVA_HOME=/jdk
env=LD_LIBRARY_PATH=/jdk/lib:/jdk/lib/server:/lib
env=PATH=/jdk/bin
env=HOME=/
env=TZ=UTC
M
../../kernel/mkext2.sh "$ROOT" disk-jvm.img ${1:-1024M}
rm -rf "$ROOT"
ls -la disk-jvm.img
e2fsck -fn disk-jvm.img >/dev/null 2>&1 && echo "disk-jvm.img: ext2 ok"
