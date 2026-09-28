#!/usr/bin/env python3
"""Tail analysis: when exactly the slow requests happen.

    tailscan.py <tlgen samples file> [threshold µs]

The samples file (`tlgen --samples`) holds uint32 triples: time from the start of the
measurement (ms), open-loop latency (ns), service time (ns).

It prints: how many outliers there are, how they are spread over time and — most importantly —
THE DISTANCES BETWEEN THEM. Periodicity distinguishes "background kernel work every N ms"
from "random noise"; without it a tail gets explained by anything at all.
"""
import struct
import sys
from collections import Counter


def main(path, thr_us=500.0):
    raw = open(path, "rb").read()
    n = len(raw) // 12
    rec = struct.unpack("<%dI" % (n * 3), raw[: n * 12])
    t = rec[0::3]        # ms from the start of the measurement
    lat = rec[1::3]      # ns
    svc = rec[2::3]

    thr = thr_us * 1000.0
    slow = [(t[i], lat[i], svc[i]) for i in range(n) if lat[i] >= thr]
    print("samples %d, threshold %.0f µs, outliers %d (%.4f %%)"
          % (n, thr_us, len(slow), 100.0 * len(slow) / n if n else 0))
    if not slow:
        return

    worst = sorted(slow, key=lambda x: -x[1])[:8]
    print("the slowest:")
    for ms, l, s in worst:
        print("   t=%7.3f s  latency %8.1f µs  service %8.1f µs" % (ms / 1000.0, l / 1e3, s / 1e3))

    # The distances between outliers: periodicity is visible at once.
    times = sorted(x[0] for x in slow)
    gaps = [times[i + 1] - times[i] for i in range(len(times) - 1)]
    if gaps:
        gaps_nz = [g for g in gaps if g > 0]
        print("gaps between outliers, ms: min %d, median %d, max %d"
              % (min(gaps), sorted(gaps)[len(gaps) // 2], max(gaps)))
        if gaps_nz:
            c = Counter(gaps_nz).most_common(6)
            print("   the most frequent non-zero gaps: " +
                  ", ".join("%d ms x%d" % (g, k) for g, k in c))

    # The breakdown by second: evenly spread or in bursts.
    per_sec = Counter(x[0] // 1000 for x in slow)
    line = " ".join("%d:%d" % (s, per_sec[s]) for s in sorted(per_sec))
    print("outliers per second: %s" % line)

    # Latency minus service = waiting in the generator's queue. If it is
    # small, the server itself answered slowly; if it is large, there was a general jam.
    q = [(l - s) for _, l, s in slow]
    print("of which waiting in the generator's queue: median %.1f µs, max %.1f µs"
          % (sorted(q)[len(q) // 2] / 1e3, max(q) / 1e3))


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(2)
    main(sys.argv[1], float(sys.argv[2]) if len(sys.argv) > 2 else 500.0)
