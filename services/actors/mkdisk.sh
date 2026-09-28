#!/bin/sh
# actor-bench disk for Talaria: /actor-bench + /cell.manifest.
# usage: mkdisk.sh [scen disp pin [N]]   (default — the whole matrix: all)
set -eu
cd "$(dirname "$0")"
ARGS=${*:-all}
ROOT=$(mktemp -d)
cp build/actor-bench "$ROOT/"
cat > "$ROOT/cell.manifest" <<M
# actor-bench (SObjectizer-5) on Talaria: talaria.mode=proc talaria.smp=4
binary=/actor-bench
args=actor-bench $ARGS
env=HOME=/
M
../../kernel/mkext2.sh "$ROOT" disk-actors.img 32M
rm -rf "$ROOT"
echo "disk-actors.img: args=$ARGS"
