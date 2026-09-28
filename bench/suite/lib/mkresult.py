#!/usr/bin/env python3
"""A report in the common format from ready latency measurements.

Workloads that are not measured by the tlgen generator (ClickHouse — query time,
actors — message delivery, task — transport with losses) bring their numbers
to the same format as everything else. Otherwise the summary table is assembled
by hand, and by hand it is assembled differently every time.

    mkresult.py <file.json> --name NAME --stand talaria|linux --tags k=v,k=v
                [--op OP] [--unit ns|us|ms] [--throughput N] [--wall SECONDS]
                [--counts key=value,...]
    samples on stdin, one number per line.

If there are no raw samples and only percentiles are available (that is how
actor-bench reports its numbers), they can be passed directly:

    mkresult.py ... --pcts "p50=1997,p90=2419,p99=4111,p99_9=11374,max=46562"
"""
import json
import math
import sys

PCTS = [("p50", 50), ("p75", 75), ("p90", 90), ("p99", 99),
        ("p99_9", 99.9), ("p99_99", 99.99), ("p99_999", 99.999)]


def pct(sorted_vals, p):
    if not sorted_vals:
        return 0
    k = min(len(sorted_vals) - 1, max(0, int(math.ceil(p / 100.0 * len(sorted_vals))) - 1))
    return int(sorted_vals[k])


def main(argv):
    out = argv[1]
    opt = {"name": "run", "stand": "unknown", "tags": "", "op": "-",
           "unit": "ns", "throughput": 0.0, "wall": 0.0, "counts": "", "pcts": ""}
    i = 2
    while i < len(argv):
        k = argv[i].lstrip("-")
        if k in opt and i + 1 < len(argv):
            opt[k] = argv[i + 1]
            i += 2
        else:
            i += 1

    mul = {"ns": 1.0, "us": 1e3, "ms": 1e6}[opt["unit"]]
    vals = []
    given = {}
    for kv in opt["pcts"].split(","):
        if "=" in kv:
            k, v = kv.split("=", 1)
            try:
                given[k.strip()] = float(v) * mul
            except ValueError:
                pass
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            vals.append(float(line) * mul)
        except ValueError:
            continue
    vals.sort()

    counts = {"sent": len(vals), "done": len(vals), "measured": len(vals),
              "lost": 0, "timeouts": 0, "conn_errors": 0, "app_errors": 0,
              "backlog_max": 0, "overload": False}
    for kv in opt["counts"].split(","):
        if "=" in kv:
            k, v = kv.split("=", 1)
            counts[k] = (v == "true") if v in ("true", "false") else int(float(v))

    if given:
        # The percentiles arrived ready: we fill them in and leave the missing ones
        # at zero — lying through interpolation over someone else's numbers is worse than not printing.
        n = int(given.get("n", counts.get("done", 0)))
        hist = {"n": n, "min_ns": int(given.get("min", 0)),
                "mean_ns": given.get("mean", 0.0), "max_ns": int(given.get("max", 0))}
        for name, _ in PCTS:
            hist[name] = int(given.get(name, 0))
        # There are no raw samples in this mode, so `measured` cannot be taken from
        # their count — and the report uses it to decide whether the run happened at all. Without
        # this line every point that arrived as ready percentiles (the actors)
        # looked like "the run did not happen" and silently dropped out of the
        # comparison.
        counts["measured"] = n
    else:
        hist = {"n": len(vals),
                "min_ns": int(vals[0]) if vals else 0,
                "mean_ns": (sum(vals) / len(vals)) if vals else 0.0,
                "max_ns": int(vals[-1]) if vals else 0}
        for name, p in PCTS:
            hist[name] = pct(vals, p)

    wall = float(opt["wall"]) or (len(vals) / float(opt["throughput"]) if float(opt["throughput"]) else 0.0)
    doc = {
        "schema": 1,
        "name": opt["name"],
        "stand": opt["stand"],
        "proto": "external",
        "op": opt["op"],
        "target": "-",
        "tags": opt["tags"],
        "wall_s": wall,
        "tsc_hz": 0,
        "cfg": {"conns": 0, "depth": 0, "threads": 0, "rate": 0, "duration": wall,
                "warmup": 0, "count": len(vals), "poll": "-", "dist": "-",
                "keyspace": 0, "valsize": 0, "read_pct": 0, "seed": 0},
        "counts": counts,
        "throughput_rps": float(opt["throughput"]) or (len(vals) / wall if wall else 0.0),
        "rate_achieved_pct": 100.0,
        "syscalls": {"send": 0, "recv_data": 0, "recv_empty": 0, "epoll_wait": 0,
                     "useful_per_req": 0.0, "all_per_req": 0.0},
        "latency": {"open_loop": hist, "service": hist},
        "timeline": [],
        "hist_ns": [],
    }
    with open(out, "w") as f:
        json.dump(doc, f, ensure_ascii=False, indent=1)
    print("%s: n=%d p50=%.1f µs p99=%.1f µs p99.9=%.1f µs max=%.1f µs"
          % (opt["name"], len(vals), hist["p50"] / 1e3, hist["p99"] / 1e3,
             hist["p99_9"] / 1e3, hist["max_ns"] / 1e3))


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(2)
    main(sys.argv)
