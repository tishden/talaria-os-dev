#!/usr/bin/env python3
"""Compare two suite runs: what changed after a fix.

    compare.py <BEFORE directory> <AFTER directory> [stand]

Prints, for every point (workload/case/rate/percentile), the BEFORE and AFTER values
and their ratio. It is needed so that a performance fix can be discussed in
numbers rather than feelings: "it got faster" without two runs is a feeling.

Points missing from either run are skipped silently: the ladder is built from
the measured ceiling and may shift between runs.
"""
import json
import os
import statistics
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import report            # noqa: E402  (which latency is judged is decided by report.py)

PCTS = [("p50", "p50"), ("p99", "p99"), ("p99_9", "p99.9"), ("p99_99", "p99.99")]


def load(d):
    out = {}
    for root, _dirs, files in os.walk(d):
        for fn in files:
            if not fn.endswith(".json") or fn in ("report.json", "floor.json") or fn.startswith(".") or fn.startswith("probe-"):
                continue
            try:
                r = json.load(open(os.path.join(root, fn)))
            except Exception:
                continue
            if r.get("schema") != 1 or int(r["counts"].get("measured", 0)) == 0:
                continue
            t = dict(kv.split("=", 1) for kv in (r.get("tags") or "").split(",") if "=" in kv)
            k = (t.get("workload", "?"), t.get("case", "?"), r.get("stand", "?"), int(r["cfg"]["rate"]))
            out.setdefault(k, []).append(report.lat(r))
    # REPEATS ARE TAKEN AS A MEDIAN PER PERCENTILE, as in report.py. Before, the last file
    # read used to win: one random repeat of three was compared,
    # and a "ratio of 1.3" between two runs could be the jitter of a single probe.
    return {k: {p: statistics.median(v[p] for v in reps if p in v)
                for p in reps[0] if all(isinstance(v.get(p), (int, float)) for v in reps)}
            for k, reps in out.items()}


def match(A, B, tol=0.08):
    """Pairs of BEFORE/AFTER points by the NEAREST rate.

    An exact rate match cannot be required: the ladder is built from the measured
    ceiling, and a performance fix moves that ceiling — which is why it was
    made. A tolerance of 8 % keeps the pairs meaningful without turning the comparison
    into a comparison of different load regimes."""
    pairs = []
    for ka, va in A.items():
        wl, case, stand, rate = ka
        best, bd = None, None
        for kb, vb in B.items():
            if kb[:3] != (wl, case, stand):
                continue
            d = abs(kb[3] - rate) / float(rate or 1)
            if d <= tol and (bd is None or d < bd):
                best, bd = (kb, vb), d
        if best:
            pairs.append((ka, va, best[0], best[1]))
    return pairs


def main(a, b, stand=None):
    A, B = load(a), load(b)
    if stand:
        A = {k: v for k, v in A.items() if k[2] == stand}
        B = {k: v for k, v in B.items() if k[2] == stand}
    pairs = sorted(match(A, B), key=lambda x: x[0])
    if not pairs:
        print("no comparable points even by the nearest rate (tolerance 8 %)")
        return 1
    print("%-8s %-6s %-8s %7s %-7s %9s %9s %6s" %
          ("workload", "case", "stand", "rate", "pct", "before", "after", "ratio"))
    better = worse = same = 0
    for k, va, kb, vb in pairs:
        for p, nm in PCTS:
            x, y = va[p], vb[p]
            if x <= 0 or y <= 0:
                continue
            r = y / x
            if r < 0.95:
                better += 1
            elif r > 1.05:
                worse += 1
            else:
                same += 1
            print("%-8s %-6s %-8s %7d %-7s %9.1f %9.1f %6.2f" %
                  (k[0], k[1], k[2], kb[3], nm, x / 1e3, y / 1e3, r))
    print("\ntotal: better %d, unchanged %d, worse %d" % (better, same, worse))
    return 0


if __name__ == "__main__":
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    sys.exit(main(sys.argv[1], sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else None))
