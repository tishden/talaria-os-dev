#!/bin/sh
# The guest tree for nginx. THE SAME tree goes into Talaria's ext2 image and into the
# Linux guest's initramfs: if each side were built by its own script, sooner or later
# they would diverge in the config or in the page content, and the comparison would
# measure the difference between trees.
#   tree.sh <directory>
set -eu
OUT=${1:?directory}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../../.." && pwd)
. "$HERE/../../lib/arch.sh"
if [ "$ARCH" = arm64 ]; then
    # ALPINE'S OWN aarch64 nginx (dynamic: ld-musl + pcre2/ssl/zlib), not x86's static
    # build: nginx's configure RUNS its test programs (sizeof, the features), so it does
    # not cross-build, and there is no aarch64 machine to build on. Both sides of the
    # Graviton stand run this one binary; the config is the same file as on x86. Its
    # compiled-in paths (/var/lib/nginx/tmp/*, /var/log/nginx) are created at start-up
    # when their parents exist — they are made below; everything we use is set by the
    # config relative to `-p /nginx/`.
    . "$REPO/services/apkroot.sh"
    apkroot "$OUT.apk" v3.20 nginx >&2 || exit 1
    [ -f "$OUT.apk/usr/sbin/nginx" ] || { echo "no usr/sbin/nginx in the unpacked tree"; exit 1; }
    cp -a "$OUT.apk/." "$OUT/"; rm -rf "$OUT.apk" "$OUT/etc/nginx" "$OUT/usr/share/doc" "$OUT/usr/share/man"
    mkdir -p "$OUT/nginx" "$OUT/var/lib/nginx/tmp" "$OUT/var/log/nginx" "$OUT/run/nginx"
    mv "$OUT/usr/sbin/nginx" "$OUT/nginx/nginx"
else
    [ -f "$REPO/services/nginx/build/nginx" ] || { echo "no services/nginx/build/nginx — run services/nginx/build.sh"; exit 1; }
fi

mkdir -p "$OUT/nginx/html" "$OUT/nginx/tmp/body" "$OUT/nginx/logs" "$OUT/etc"
[ "$ARCH" = arm64 ] || cp "$REPO/services/nginx/build/nginx" "$OUT/nginx/nginx"
cp "$REPO/services/nginx/nginx.conf"  "$OUT/nginx/nginx.conf"
: > "$OUT/nginx/error.log"

# nginx under root calls getpwnam for the user directive — without /etc/passwd it
# refuses to start. The file goes into the SHARED tree so that both sides of the
# comparison see the same thing.
# Alpine's arm64 nginx is built `--user=nginx`: as root (the Linux side of the stand) it asks
# getpwnam("nginx") and refuses to start without it (`[emerg] getpwnam("nginx") failed` — the first
# Graviton scoreboard lost its nginx bring-up to it; Talaria's process is not root and never asks).
cat > "$OUT/etc/passwd" <<'P'
root:x:0:0:root:/root:/bin/sh
nobody:x:65534:65534:nobody:/:/bin/false
nginx:x:100:101:nginx:/var/lib/nginx:/bin/false
P
cat > "$OUT/etc/group" <<'G'
root:x:0:
nobody:x:65534:
nginx:x:101:
G

# A page of fixed size: the response must not depend on what happened to be
# in the tree on a particular machine.
python3 - "$OUT/nginx/html/index.html" <<'PY'
import sys
body = "<html><body>\n" + ("talaria " * 120) + "\n</body></html>\n"
open(sys.argv[1], "w").write(body)
PY
echo "the nginx tree is ready: $OUT"
