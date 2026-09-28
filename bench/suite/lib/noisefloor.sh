#!/bin/sh
# The stand's noise floor: which percentiles this stand can distinguish AT ALL.
#
#   noisefloor.sh <floor.json file>
#
# Why. On a working machine without core isolation, the host scheduler's own jitter
# produces outliers of milliseconds. If it is bigger than the effect under study,
# a number at p99.9 speaks about the host rather than about the systems being compared — and
# publishing that is not admissible in anyone's favour.
#
# How it is measured. The same generator, the same server (nginx from the suite), but WITHOUT
# any virtualisation: both the client and the server on the host over loopback. Whatever
# remains in the tail is the price of the stand itself: the host scheduler, migrations,
# neighbouring processes. A percentile at which the stand is no better than this, the report
# marks as unresolvable.
set -eu
OUT=${1:?file}
HERE=$(cd "$(dirname "$0")" && pwd)
SUITE=$(cd "$HERE/.." && pwd)
REPO=$(cd "$SUITE/../.." && pwd)
NG="$REPO/services/nginx/build/nginx"
ROOT="$REPO/services/nginx/build/root/nginx"
PORT=${FLOOR_PORT:-18080}
RATE=${FLOOR_RATE:-2000}
DUR=${FLOOR_DUR:-20}

[ -x "$NG" ] || { echo "no $NG — run services/nginx/build.sh"; exit 1; }
mkdir -p "$ROOT/html" "$ROOT/tmp/body"
[ -f "$ROOT/nginx.conf" ] || sed "s/listen 8080/listen $PORT/" "$REPO/services/nginx/nginx.conf" > "$ROOT/nginx.conf"
sed -i "s/listen [0-9]*/listen $PORT/" "$ROOT/nginx.conf"
[ -f "$ROOT/html/index.html" ] || echo "<html><body>floor</body></html>" > "$ROOT/html/index.html"
: > "$ROOT/error.log"

"$NG" -p "$ROOT/" -c nginx.conf >/dev/null 2>&1 &
NGPID=$!
trap 'kill $NGPID 2>/dev/null || true' EXIT INT TERM
i=0; while [ $i -lt 30 ]; do
    timeout 1 bash -c "exec 3<>/dev/tcp/127.0.0.1/$PORT" 2>/dev/null && break
    i=$((i + 1)); sleep 1
done

# The floor is measured SEVERAL times, and the WORST is taken (see lib/floorpick.py): one
# measurement is not enough — between runs it varied tenfold (p99.9 of 119 µs
# against 1125), i.e. the floor itself has a wide spread. A threshold from a
# lucky measurement would let into the report as a "result" what the stand cannot
# distinguish.
N=${FLOOR_RUNS:-3}
i=1
while [ "$i" -le "$N" ]; do
    "$SUITE/tlgen/tlgen" --proto http --host 127.0.0.1 --port "$PORT" --path /nop \
        --conns 4 --rate "$RATE" --duration "$DUR" --warmup 3 --spin \
        --name "floor" --stand floor --tags "workload=floor,case=nop" \
        --out "$OUT.$i" 2>&1 | tail -1 | sed "s|^|   floor $i/$N: |"
    i=$((i + 1))
done
python3 "$HERE/floorpick.py" "$OUT" "$N"
echo "the stand's noise floor recorded: $OUT"
