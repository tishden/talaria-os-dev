#!/usr/bin/env python3
"""THE BAND: where exactly the distribution loses, and whether that reproduces.

    bandscan.py <results directory> [case] [rate] [--lo µs] [--hi µs]

WHY. The scoreboard judges by four percentiles, and when a point is lost exactly at
`p99.9` (mongodb insert, the bottom step), they say nothing: `p50` and `p99`
are won, we have FEWER events longer than a millisecond, and `p99.9` is one and a half times
worse. A percentile is one number at the end of a long tape; to understand the shape one must
look at WHICH share of requests is above each threshold, on both sides and in all
repeats.

WHAT IS COUNTED (from the full `hist_ns` histogram, not from the report's percentiles):
  * the share of requests above a series of thresholds — on both sides, each repeat separately;
  * THE BAND [--lo, --hi): how many requests fell into it, how many
    microseconds that is and what the shape inside looks like (buckets of 20 µs);
  * the spread of the share between repeats — an instrument is judged by ITS OWN reproducibility,
    and the share in the band turned out to be more precise than `p99.9` itself.

WHY THIS IS MORE HONEST THAN `p99.9`. At point r2168 of one scoreboard Linux's `p99.9` wandered
over 152-177 µs (16 %), while the share of requests above 150 µs was 0.127-0.176 % (the same thing,
but it is visible that these are TWO HUNDRED requests, not "a percentile"). Our spread of the share
in the band is 5 %, and the ratio of the sides holds in all three repeats.
"""
import glob
import json
import os
import sys

THRESHOLDS_US = (110, 120, 130, 140, 150, 160, 180, 200, 250, 300, 400, 600, 1000, 2000)


def runs(d, case, rate):
    """{side: [(repeat name, histogram, total)]}"""
    out = {}
    for side in ("talaria", "linux"):
        pat = os.path.join(d, f"{side}-{case}-r{rate}-*.json")
        for f in sorted(glob.glob(pat)):
            j = json.load(open(f))
            h = j.get("hist_ns") or []
            if not h:
                continue
            out.setdefault(side, []).append((os.path.basename(f), h, sum(c for _, c in h), j))
    return out


def share_above(h, tot, us):
    return 100.0 * sum(c for v, c in h if v >= us * 1000) / tot


def band(h, lo, hi):
    sel = [(v, c) for v, c in h if lo * 1000 <= v < hi * 1000]
    n = sum(c for _, c in sel)
    s = sum(v * c for v, c in sel)
    return n, s / 1e6, (s / n / 1000.0 if n else 0.0)


def pick(d, case, rate):
    """If the case/rate are not given — take the most populated one in the directory."""
    names = set()
    for f in glob.glob(os.path.join(d, "talaria-*.json")):
        b = os.path.basename(f)[len("talaria-"):-len(".json")]
        i = b.rfind("-r")
        if i < 0:
            continue
        c, r = b[:i], b[i + 2:].split("-")[0]
        names.add((c, r))
    if not names:
        return None, None
    if case and rate:
        return case, rate
    cand = [x for x in names if not case or x[0] == case]
    cand.sort(key=lambda x: int(x[1]))
    return cand[0] if cand else (None, None)


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    lo, hi = 140, 400
    if "--lo" in sys.argv:
        lo = int(sys.argv[sys.argv.index("--lo") + 1])
    if "--hi" in sys.argv:
        hi = int(sys.argv[sys.argv.index("--hi") + 1])
    args = [a for a in args if not a.isdigit() or True]
    if not args:
        print(__doc__)
        return 2
    d = args[0]
    case = args[1] if len(args) > 1 else None
    rate = args[2] if len(args) > 2 else None
    case, rate = pick(d, case, rate)
    if not case:
        print("the directory has no talaria-<case>-r<rate>-*.json files")
        return 2
    rr = runs(d, case, rate)
    if "talaria" not in rr:
        print(f"no talaria runs for {case} r{rate}")
        return 2
    print(f"== {d}: {case} r{rate}, band {lo}-{hi} µs")
    print("\n| side | repeat | requests | p50 | p99 | p99.9 | p99.99 | worst |")
    print("|---|---|---:|---:|---:|---:|---:|---:|")
    for side in ("talaria", "linux"):
        for name, h, tot, j in rr.get(side, []):
            s = j["latency"]["service"]
            print("| %s | %s | %d | %.1f | %.1f | %.1f | %.1f | %.2f ms |"
                  % (side, name.split("-")[-1].split(".")[0], tot, s["p50"] / 1e3,
                     s["p99"] / 1e3, s["p99_9"] / 1e3, s["p99_99"] / 1e3, s["max_ns"] / 1e6))
    print("\nshare of requests ABOVE the threshold, %:")
    cols = [(side, name, h, tot) for side in ("talaria", "linux") for name, h, tot, _ in rr.get(side, [])]
    print("|---:|" + "---:|" * (len(cols) + 1))
    def med(v):
        v = sorted(v)
        return v[len(v) // 2]
    print("| threshold, µs | " + " | ".join(("ours" if s == "talaria" else "linux") + " " + n.split("-")[-1].split(".")[0]
                                         for s, n, _, _ in cols) + " | ratio of medians |")
    for us in THRESHOLDS_US:
        by = {}
        vals = []
        for side, _, h, tot in cols:
            x = share_above(h, tot, us)
            vals.append(x)
            by.setdefault(side, []).append(x)
        r = ""
        if len(by) == 2 and med(by["linux"]) > 0:
            r = "%.2f" % (med(by["talaria"]) / med(by["linux"]))
        print(f"| {us} | " + " | ".join("%.4f" % x for x in vals) + f" | {r} |")
    print(f"\nTHE BAND {lo}-{hi} µs:")
    print("| side | repeat | requests | share, % | total, ms | mean, µs |")
    print("|---|---|---:|---:|---:|---:|")
    shares = {}
    for side, name, h, tot in cols:
        n, ms, avg = band(h, lo, hi)
        shares.setdefault(side, []).append(100.0 * n / tot)
        print("| %s | %s | %d | %.3f | %.1f | %.1f |"
              % (side, name.split("-")[-1].split(".")[0], n, 100.0 * n / tot, ms, avg))
    for side, v in shares.items():
        if len(v) > 1:
            print(f"   spread of the share on {side}: {min(v):.3f}-{max(v):.3f} % (×{max(v)/max(min(v),1e-9):.2f})")
    if len(shares) == 2:
        a = sum(shares["talaria"]) / len(shares["talaria"])
        b = sum(shares["linux"]) / len(shares["linux"])
        print(f"   RATIO of shares (ours/Linux): {a/max(b,1e-9):.2f}")
    print(f"\nthe shape INSIDE the band (buckets of 20 µs), requests:")
    edges = list(range(lo - 20, hi + 1, 20))
    print("| side/repeat | " + " | ".join(str(e) for e in edges) + " |")
    print("|---|" + "---:|" * len(edges))
    for side, name, h, tot in cols:
        row = [sum(c for v, c in h if e * 1000 <= v < (e + 20) * 1000) for e in edges]
        print("| %s %s | " % ("ours" if side == "talaria" else "linux", name.split("-")[-1].split(".")[0])
              + " | ".join(str(x) for x in row) + " |")
    return 0


if __name__ == "__main__":
    sys.exit(main())
