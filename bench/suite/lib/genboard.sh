#!/bin/sh
# THE LINUX-GENERATOR VIEW OF ONE WORKLOAD (ARM64-PLAN §12 item 0).
#
#   genboard.sh <gen-<wl> directory>     (A -> the scoreboard's run, L -> ab-phase.sh GEN=linux)
#
# The scoreboard pairs each server with ITS OWN generator; §10.14 showed the Talaria
# generator's client path is a share of every network point. This view pairs the Talaria
# server measured by the LINUX generator (phase L, stand=talaria) with the Linux server
# measured by the same Linux generator (phase A's linux side) — the same rates, the same
# hour, the same judging (lib/summary.py: 0.95/1.05, the debt, the thin tails). The merged
# directory LA holds links only; A's stand.env gives the provenance (the same server).
# Then the ceilings of the three pairs from the runs' .caps files.
set -eu
D=${1:?the gen-<workload> directory}
HERE=$(cd "$(dirname "$0")" && pwd)
LA="$D/LA"
rm -rf "$LA"; mkdir -p "$LA"
n=0
for f in "$D"/L/talaria-*.json "$D"/L/metrics-talaria-*.txt "$D"/A/linux-*.json "$D"/A/metrics-linux-*.txt; do
    [ -e "$f" ] || continue
    ln -s "$(realpath "$f")" "$LA/$(basename "$f")"; n=$((n + 1))
done
[ -f "$D/A/stand.env" ] && cp "$D/A/stand.env" "$LA/stand.env"
echo "== genboard: $n files linked into $LA"
python3 "$HERE/summary.py" "$LA" --md "$D/summary-LA.md" > "$D/summary-LA.txt" 2>&1 || true
tail -8 "$D/summary-LA.txt"
echo "== ceilings, req/s (the median of the probes):"
python3 - "$D" <<'PY'
import sys, os
d = sys.argv[1]
def caps(p):
    out = {}
    try:
        for l in open(p):
            w = l.split()
            if len(w) == 3: out[(w[0], w[1])] = int(w[2])
    except OSError: pass
    return out
a, l = caps(os.path.join(d, "A", ".caps")), caps(os.path.join(d, "L", ".caps"))
print("| case | Talaria server, Linux generator | Linux server, Linux generator | Talaria server, Talaria generator |")
print("| --- | ---: | ---: | ---: |")
for case in sorted({k[0] for k in list(a) + list(l)}):
    tl, ll, tt = l.get((case, "talaria")), a.get((case, "linux")), a.get((case, "talaria"))
    f = lambda v: "%d" % v if v else "—"
    print("| %s | %s | %s | %s |" % (case, f(tl), f(ll), f(tt)))
PY
