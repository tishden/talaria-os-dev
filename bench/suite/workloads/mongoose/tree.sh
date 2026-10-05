#!/bin/sh
# The guest tree for mongoose 7.23 (a lightweight HTTP server, a second implementation next to nginx).
set -eu
OUT=${1:?directory}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../../.." && pwd)
# IMPORTANT: the binary must be built with -DMG_ENABLE_LOG=0. With logging on,
# mongoose prints two lines PER REQUEST, and on Talaria that goes to serial:
# the speed of the console is measured instead of the server (on a local run the
# test case produced not a single point because of it).
. "$HERE/../../lib/arch.sh"
mkdir -p "$OUT/etc"
if [ "$ARCH" = arm64 ]; then
    a64_need "$A64/mongoose_srv" services/build-aarch64.sh   # MG_ENABLE_LOG=0 there too
    cp "$A64/mongoose_srv" "$OUT/mongoose_srv"
else
    [ -f "$REPO/services/mongoose/mongoose_srv" ] || { echo "no services/mongoose/mongoose_srv (see services/mongoose/README.md, build with -DMG_ENABLE_LOG=0)"; exit 1; }
    cp "$REPO/services/mongoose/mongoose_srv" "$OUT/mongoose_srv"
fi
# The same page as nginx's: the test cases must differ by server, not by
# response size.
python3 - "$OUT/index.html" <<'PY'
import sys
open(sys.argv[1], "w").write("<html><body>\n" + ("talaria " * 120) + "\n</body></html>\n")
PY
printf 'root:x:0:0:root:/root:/bin/sh\n' > "$OUT/etc/passwd"
echo "the mongoose tree is ready: $OUT"
