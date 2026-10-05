#!/bin/sh
# The guest tree for MongoDB 4.0 (Alpine musl, a DYNAMIC ELF: ld-musl + .so).
set -eu
OUT=${1:?directory}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../../.." && pwd)
SRC="$REPO/services/mongodb/build/root"
. "$HERE/../../lib/arch.sh"
if [ "$ARCH" = arm64 ]; then
    # The same release from the same Alpine (3.9), its aarch64 build (services/build-aarch64-db.sh).
    # No patched ld-musl for aarch64 exists: both sides run Alpine's own.
    . "$REPO/services/apkroot.sh"
    apkroot "$OUT.apk" v3.9 mongodb busybox >&2 || exit 1
    [ -f "$OUT.apk/usr/bin/mongod" ] || { echo "no mongod in the unpacked tree"; exit 1; }
    cp -a "$OUT.apk/." "$OUT/"; rm -rf "$OUT.apk" "$OUT/usr/share/doc" "$OUT/usr/share/man" "$OUT/usr/include"
else
    [ -x "$SRC/usr/bin/mongod" ] || { echo "no $SRC/usr/bin/mongod — see services/mongodb/README.md"; exit 1; }
    cp -r "$SRC/." "$OUT/"
fi
# A patched ld-musl (a umwait tier in the futex paths): on Talaria a futex
# sleeps with mwait in the kernel. It is installed on BOTH sides — otherwise this
# would be a difference of runtimes rather than of systems.
if [ "$ARCH" = x86_64 ] && [ "${TAL_MUSL:-1}" = 1 ] && [ -f "$REPO/kernel/user/dyn/ld-musl-x86_64.so.1" ]; then
    cp "$REPO/kernel/user/dyn/ld-musl-x86_64.so.1" "$OUT/lib/ld-musl-x86_64.so.1"
fi
mkdir -p "$OUT/data/db" "$OUT/tmp" "$OUT/etc" "$OUT/proc" "$OUT/sys"
printf 'root:x:0:0:root:/:/bin/sh\nmongodb:x:1000:1000:mongodb:/data:/bin/sh\n' > "$OUT/etc/passwd"
printf 'root:x:0:\nmongodb:x:1000:\n' > "$OUT/etc/group"
printf 'hosts: files dns\n' > "$OUT/etc/nsswitch.conf"
printf '127.0.0.1 localhost talaria\n' > "$OUT/etc/hosts"
echo "the mongodb tree is ready: $OUT"
