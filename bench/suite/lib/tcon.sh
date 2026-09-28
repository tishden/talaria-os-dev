#!/bin/sh
# A wrapper over talaria-console.py: the console socket is created by QEMU inside the
# container (i.e. as root, with mode srwxr-xr-x), and it cannot be connected to as an
# ordinary user — connecting to a unix socket requires write permission. So the reader
# runs from the container too.
#   tcon.sh <command> [another command...]
set -eu
REPO=${REPO:-$(cd "$(dirname "$0")/../../.." && pwd)}
SOCK=${CONSOLE_SOCK:-bench/suite/results/console.sock}
[ -S "$REPO/$SOCK" ] || { echo "no console socket $SOCK — is the VM up?"; exit 1; }
exec docker run --rm -v "$REPO":/w -w /w talaria-qemu2 \
    python3 /w/bench/suite/lib/talaria-console.py "/w/$SOCK" "$@"
