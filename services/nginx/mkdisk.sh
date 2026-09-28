#!/bin/sh
# The nginx disk for Talaria (ext2): the /nginx tree + the cell manifest.
#   mkdisk.sh [size]        default 64M
set -eu
cd "$(dirname "$0")"
[ -f build/nginx ] || { echo "no build/nginx — run ./build.sh first"; exit 1; }
ROOT=$(mktemp -d)
mkdir -p "$ROOT/nginx/html" "$ROOT/nginx/tmp/body" "$ROOT/nginx/logs"
cp build/nginx "$ROOT/nginx/nginx"
cp nginx.conf "$ROOT/nginx/nginx.conf"
: > "$ROOT/nginx/error.log"
# A page of a fixed size: the response must not depend on what was put into the
# tree, otherwise runs on the two stands serve different amounts.
python3 - "$ROOT/nginx/html/index.html" <<'PY'
import sys
body = "<html><body>\n" + ("talaria " * 120) + "\n</body></html>\n"
open(sys.argv[1], "w").write(body)
print("index.html: %d bytes" % len(body))
PY
cat > "$ROOT/cell.manifest" <<'M'
# nginx 1.24 on Talaria: one process, one worker thread (master_process off).
binary=/nginx/nginx
args=nginx -p /nginx/ -c nginx.conf
env=HOME=/
env=PATH=/nginx
M
../../kernel/mkext2.sh "$ROOT" disk-nginx.img "${1:-64M}"
rm -rf "$ROOT"
ls -la disk-nginx.img
e2fsck -fn disk-nginx.img >/dev/null 2>&1 && echo "disk-nginx.img: ext2 ok"
