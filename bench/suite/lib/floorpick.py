#!/usr/bin/env python3
"""Pick the WORST of several noise-floor measurements.

    floorpick.py <floor file> <how many measurements>

One floor measurement is not enough: between runs it varied tenfold
(p99.9 of 119 µs against 1125), i.e. the floor itself has a wide spread.
A threshold taken from a lucky measurement would let into the report as a "result" what
the stand in fact cannot distinguish. We take the worst — so the gate errs on the side
of caution.
"""
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import report            # noqa: E402  (which latency is judged is decided by report.py)


def main(out, n):
    best, bestv, seen = None, -1, []
    for i in range(1, n + 1):
        p = "%s.%d" % (out, i)
        if not os.path.exists(p):
            continue
        d = json.load(open(p))
        v = report.lat(d)["p99_9"]
        seen.append(v)
        if v > bestv:
            best, bestv = d, v
        os.unlink(p)
    if best is None:
        print("   the floor was not measured")
        return 1
    json.dump(best, open(out, "w"))
    print("   floor: took the WORST of %d measurements (p99.9 per measurement: %s µs)"
          % (len(seen), ", ".join("%.0f" % (x / 1e3) for x in sorted(seen))))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1], int(sys.argv[2])))
