#!/usr/bin/env python3
"""THE WORST ANSWER PER RUN — do the "someone else's 8 ms" exist at all.

    worstscan.py <phase directory> [another directory ...]

The "someone else's 8 ms" class is visible NOT as a percentile but as a shape: up to p99.9 we
are better than Linux, and then there is a cliff into single-digit milliseconds with a maximum right
around 8 ms — and not in every repeat, but in two of three. So the instrument prints
each run separately, for inspection (rather than the median over repeats, as compare.py does),
and marks those whose maximum lies in the 6-12 ms window.
"""
import glob
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import report            # noqa: E402  (which histogram is judged is decided by report.py)


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    print("%-46s %10s %10s %10s" % ("run", "p99.9 µs", "p99.99", "MAX"))
    for d in sys.argv[1:]:
        for fn in sorted(glob.glob(os.path.join(d, "*.json"))):
            b = os.path.basename(fn)
            if b.startswith(("probe-", ".")) or b in ("report.json", "floor.json"):
                continue
            try:
                r = json.load(open(fn))
            except Exception:
                continue
            if r.get("schema") != 1 or int(r["counts"].get("measured", 0)) == 0:
                continue
            l = report.lat(r)
            m = l.get("max_ns", 0) / 1000.0
            print("%-46s %10.1f %10.1f %10.1f%s"
                  % (b, l.get("p99_9", 0) / 1000.0, l.get("p99_99", 0) / 1000.0, m,
                     "   <- around 8 ms" if 6000 <= m <= 12000 else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
