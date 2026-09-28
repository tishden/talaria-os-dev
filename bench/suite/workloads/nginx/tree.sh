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
[ -f "$REPO/services/nginx/build/nginx" ] || { echo "no services/nginx/build/nginx — run services/nginx/build.sh"; exit 1; }

mkdir -p "$OUT/nginx/html" "$OUT/nginx/tmp/body" "$OUT/nginx/logs" "$OUT/etc"
cp "$REPO/services/nginx/build/nginx" "$OUT/nginx/nginx"
cp "$REPO/services/nginx/nginx.conf"  "$OUT/nginx/nginx.conf"
: > "$OUT/nginx/error.log"

# nginx under root calls getpwnam for the user directive — without /etc/passwd it
# refuses to start. The file goes into the SHARED tree so that both sides of the
# comparison see the same thing.
cat > "$OUT/etc/passwd" <<'P'
root:x:0:0:root:/root:/bin/sh
nobody:x:65534:65534:nobody:/:/bin/false
P
cat > "$OUT/etc/group" <<'G'
root:x:0:
nobody:x:65534:
G

# A page of fixed size: the response must not depend on what happened to be
# in the tree on a particular machine.
python3 - "$OUT/nginx/html/index.html" <<'PY'
import sys
body = "<html><body>\n" + ("talaria " * 120) + "\n</body></html>\n"
open(sys.argv[1], "w").write(body)
PY
echo "the nginx tree is ready: $OUT"
