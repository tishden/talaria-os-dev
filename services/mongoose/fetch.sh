#!/bin/sh
# Download the mongoose 7.23 sources (Cesanta, GPLv2/commercial — NOT vendored
# into this Apache-2.0 repository; fetched from upstream like the rest of services/).
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
V=7.23
for f in mongoose.c mongoose.h; do
  [ -f "$HERE/$f" ] && continue
  echo "fetch: $f (mongoose $V)"
  curl -fsSL "https://raw.githubusercontent.com/cesanta/mongoose/$V/$f" -o "$HERE/$f"
done
echo "OK: mongoose $V"
