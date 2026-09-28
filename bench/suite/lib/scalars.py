#!/usr/bin/env python3
"""A summary of SCALAR metrics (lines `RESULT <group> <metric> <value> <unit>`).

    scalars.py <talaria file> <linux file> [--md]

Not every workload is measured by a latency distribution. For the JVM benchmark it is pass
time, gigaflops, allocation rate, speedup on four cores; for the actors it is
throughput. Such things cannot be forced into the percentile scheme: for half the
metrics "more" means "better", and the talaria/linux ratio would read backwards.

So the direction is given EXPLICITLY per metric name rather than guessed. A metric
whose direction is unknown is printed without a verdict — a silent guess
here is worse than an omission.
"""
import re
import sys

# "More is better": rates, throughput, speedup.
HIGHER = ("rate", "gflops", "throughput", "speedup", "bytes", "ops", "mops",
          "monitor", "reentrant")   # "million acquisitions/s" — also a rate
# "Less is better": times and latencies.
LOWER = ("time", "startup", "p50", "p99", "p999", "min", "max", "fullgc",
         "1thr", "2thr", "4thr", "8thr", "latency", "rtt",
         "nanotime",      # ns per call
         "park",          # sleep "overshoot": by how much the deadline was missed
         "wall")          # the total run time
# Informational: not compared.
INFO = ("cores", "maxheap")


def direction(metric):
    m = metric.lower()
    if m in INFO:
        return 0
    if any(m == h or m.startswith(h) for h in HIGHER):
        return +1
    if any(m == l or m.startswith(l) for l in LOWER):
        return -1
    return None


def load(path):
    out = {}
    for line in open(path, encoding="utf-8", errors="replace"):
        m = re.match(r"^RESULT\s+(\S+)\s+(\S+)\s+([0-9.]+)\s*(.*)$", line.strip())
        if m:
            out[(m.group(1), m.group(2))] = (float(m.group(3)), m.group(4).strip())
    return out


def compare(fa, fb):
    """(table rows, (better, parity, worse, no direction)) for two files.

    Moved out of `main` because the same count is used by the summary over all
    workloads (`lib/summary.py`): two implementations of one verdict diverge
    sooner or later, and they diverge silently."""
    A, B = load(fa), load(fb)
    keys = [k for k in A if k in B]
    better = worse = same = unknown = 0
    rows = []
    for k in sorted(keys):
        av, unit = A[k]
        bv, _ = B[k]
        d = direction(k[1])
        if d == 0 or bv == 0:
            verdict = "—"
        elif d is None:
            verdict = "direction unknown"
            unknown += 1
        else:
            ratio = av / bv
            gain = ratio if d < 0 else (1.0 / ratio if ratio else 0)
            if gain < 0.95:
                verdict = "Talaria better"; better += 1
            elif gain > 1.05:
                verdict = "Talaria worse"; worse += 1
            else:
                verdict = "parity"; same += 1
        rows.append((k[0], k[1], av, bv, unit, verdict))
    return rows, (better, same, worse, unknown)


def main(fa, fb, md=False):
    rows, (better, same, worse, unknown) = compare(fa, fb)
    if not rows:
        print("no common metrics")
        return 1

    if md:
        print("| group | metric | Talaria | Linux | unit | verdict |")
        print("|---|---|---:|---:|---|---|")
        for g, m, av, bv, unit, v in rows:
            print("| %s | %s | %.3f | %.3f | %s | %s |" % (g, m, av, bv, unit, v))
    else:
        print("%-10s %-12s %12s %12s  %-14s %s" % ("group", "metric", "Talaria", "Linux", "unit", "verdict"))
        for g, m, av, bv, unit, v in rows:
            print("%-10s %-12s %12.3f %12.3f  %-14s %s" % (g, m, av, bv, unit, v))
    print("\ntotal: Talaria better %d, parity %d, worse %d%s"
          % (better, same, worse, (", direction unknown for %d" % unknown) if unknown else ""))
    return 0


if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if a != "--md"]
    if len(args) < 2:
        print(__doc__)
        sys.exit(2)
    sys.exit(main(args[0], args[1], "--md" in sys.argv))
