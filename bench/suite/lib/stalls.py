#!/usr/bin/env python3
"""STALLS from tlgen's per-second timeline: when the server stopped completely.

    stalls.py <results directory> [case] [spike threshold ms = 60]

Percentiles over a run hide the main thing: is a p99.9 of 13 ms "a little worse every
second" or "a one-second stall once a minute"? The `timeline` (second ->
p50/p99/p99.9/max) answers that. Printed per run:
  stalls  — seconds where p50 >= 100 ms (the server answered NOBODY);
  spikes  — seconds where max >= the threshold while p50 is normal.
The sides must be compared at one rate: a stall in both at the ceiling is overload,
a stall in one at a third of the ceiling is a defect.
"""
import glob
import json
import os
import re
import sys


def scan(path, thr_ms):
    d = json.load(open(path))
    tl = d.get("timeline") or []
    stops, spikes = [], []
    for e in tl:
        p50 = e.get("p50", 0) / 1e6
        mx = e.get("max", 0) / 1e6
        if p50 >= 100.0:
            stops.append((e["s"], p50, mx))
        elif mx >= thr_ms:
            spikes.append((e["s"], mx))
    return len(tl), stops, spikes


def main(root, case=None, thr_ms=60.0):
    files = sorted(glob.glob(os.path.join(root, "*.json")))
    rows = []
    for f in files:
        m = re.match(r"(talaria|linux)-(.+)-r(\d+)-(\d+)\.json$", os.path.basename(f))
        if not m:
            continue
        side, cs, rate, rep = m.group(1), m.group(2), int(m.group(3)), int(m.group(4))
        if case and cs != case:
            continue
        n, stops, spikes = scan(f, thr_ms)
        rows.append((cs, rate, side, rep, n, stops, spikes))
    rows.sort()
    print("%-8s %-8s %7s %3s %4s %5s %6s  %s" % ("case", "stand", "rate", "#", "s", "stall", "spike", "stalls (s: p50 ms / max ms), the worst spikes (s: max ms)"))
    for cs, rate, side, rep, n, stops, spikes in rows:
        st = " ".join("%d:%.0f/%.0f" % x for x in stops[:6]) + (" …" if len(stops) > 6 else "")
        sp = " ".join("%d:%.0f" % x for x in sorted(spikes, key=lambda x: -x[1])[:4])
        print("%-8s %-8s %7d %3d %4d %5d %6d  %s | %s" % (cs, side, rate, rep, n, len(stops), len(spikes), st, sp))


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(2)
    case = sys.argv[2] if len(sys.argv) > 2 and not sys.argv[2].replace(".", "").isdigit() else None
    thr = float(sys.argv[-1]) if len(sys.argv) > 2 and sys.argv[-1].replace(".", "").isdigit() else 60.0
    main(sys.argv[1], case, thr)
