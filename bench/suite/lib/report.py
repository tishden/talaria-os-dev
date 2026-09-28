#!/usr/bin/env python3
"""Reducing a run's results to a "stand x rate x percentiles" table.

Input is a directory with tlgen JSON reports (one file per run), output is
report.md and report.json next to them.

Three rules that make the table look the way it does:

* a run with overload (`overload` or a rate shortfall) is marked and is NOT
  compared with a clean one: its percentiles measure the generator's queue;
* repeats are reduced BY MEDIAN, with the spread printed next to it — a mean over three
  runs, one of which is three times worse, hides exactly what is interesting;
* rows where Talaria loses look no different from rows where it wins.
"""
import json
import os
import statistics
import sys

PCTS = [("p50", "p50"), ("p99", "p99"), ("p99_9", "p99.9"), ("p99_99", "p99.99"), ("max", "max")]

# The share of samples lying BEYOND a percentile: p99.99 on a run of 80 thousand requests
# rests on eight of them.
PCT_LEVEL = {"p50": 50.0, "p99": 99.0, "p99_9": 99.9, "p99_99": 99.99, "max": 100.0}

# A POINT WITH ONLY A FEW REQUESTS BEYOND ITS PERCENTILE IS JUDGED NEITHER FOR NOR
# AGAINST. A percentile always returns a number, even when it is a single
# measurement: at 80 thousand requests p99.99 is the eighth from the top, and the ratio of the sides
# on it speaks of who was unlucky with one answer, not of the systems.
# The threshold is chosen so that a percentile rests on at least ten requests.
TAIL_MIN = 10

# The levels at which multi-millisecond answers are counted. 1 ms because
# it is two orders of magnitude above the median of any of our workloads (tens of microseconds),
# and below it the scale is occupied by ordinary work; 4 ms because the quantum with which
# the EC2 host takes a vCPU away from us is about 8 ms, and half
# a quantum separates it from everything else.
EV_LEVELS = (1_000_000, 4_000_000)

# THE SERVICE LATENCY IS JUDGED, and that is written down here rather than kept in the operator's head.
#
# tlgen measures two latencies: `open_loop` — from the SCHEDULED deadline, `service` —
# from the actual send to the answer. The first honestly includes the queue on the server,
# which is why the open loop exists — but together with it the debt of the GENERATOR
# ITSELF: all the time by which it was late for the deadline. On the EC2 stand the generator
# against Talaria was late by thousands of requests (`backlog_max` up to 84 thousand at 2.8 thousand
# req/s), and in the open loop the Talaria side gave not a single judgeable point:
# every number spoke about the generator, not about the server. The service latency does not
# depend on the generator's debt — and by it both sides are compared with one
# instrument. The debt is NOT hidden: it is printed in its own column (that is a
# direct measure of whether the instrument keeps the schedule), and a rate shortfall still
# rejects the point entirely (`bad`).
#
# `TLJUDGE=open_loop` restores the old judging — for cross-checking against old
# tables; there the per-percentile rejection by debt applies again.
JUDGE = os.environ.get("TLJUDGE", "service")
if JUDGE not in ("service", "open_loop"):
    raise SystemExit("TLJUDGE: service or open_loop, not %r" % JUDGE)


def lat(r):
    """The histogram by which a run is judged. Reports without `service` (old ones,
    or assembled by `mkresult.py` from foreign numbers, where both histograms are the same
    thing) are judged by `open_loop` — there is no difference there."""
    l = r["latency"]
    if JUDGE == "service" and l.get("service") and l["service"].get("n", 1) > 0 \
            and l["service"].get("p50", 0) > 0:
        return l["service"]
    return l["open_loop"]


def debt_ns(r, k):
    """The generator's debt at a percentile: the open loop minus the service latency."""
    l = r["latency"]
    sv = l.get("service") or {}
    kk = "max_ns" if k == "max" else k
    return max(0, l["open_loop"].get(kk, 0) - sv.get(kk, 0)) if sv.get(kk, 0) > 0 else 0


def load(d):
    """Reports from a directory and from nested ones: a combined run puts each workload
    into its own subdirectory, while one table is needed for all of them."""
    runs = []
    BROKEN.clear()
    for root, _dirs, files in os.walk(d):
        for fn in sorted(files):
            # `probe-*.json` are ceiling probes (run-remote.sh keeps them for
            # analysing "why 0"); they are not points of the ladder.
            if not fn.endswith(".json") or fn in ("report.json", "floor.json") or fn.startswith(".") \
                    or fn.startswith("probe-"):
                continue
            path = os.path.join(root, fn)
            try:
                r = parse_file(path)
            except Exception as e:
                # NOT "skipped". A file that does not parse is a point that is
                # MISSING, and the summary must say so loudly: on EC2
                # all ten points of the Talaria side disappeared like that (an old tlgen
                # appended `{"error": 1}` after the report), and the table looked
                # like "Linux was measured, Talaria was not run".
                BROKEN.append((os.path.relpath(path, d), str(e)))
                continue
            if r.get("schema") != 1:
                continue
            r["_file"] = os.path.relpath(path, d)
            runs.append(r)
    return runs


BROKEN = []     # (file, reason) — reports that did not parse


def parse_file(path):
    """A tlgen report from a file. A tail after the first JSON object (an old
    `--serve` appended `{"error": N}` on a separate line) does not lose the report,
    but lands in its `error`."""
    text = open(path, encoding="utf-8", errors="replace").read()
    r, end = json.JSONDecoder().raw_decode(text.lstrip())
    tail = text.lstrip()[end:].strip()
    if tail:
        r.setdefault("error", "tail after the report: %.80s" % tail)
    if not isinstance(r, dict) or "counts" not in r or "latency" not in r:
        raise ValueError("not a tlgen report (no counts/latency)")
    return r


def tags(r):
    out = {}
    for kv in (r.get("tags") or "").split(","):
        if "=" in kv:
            k, v = kv.split("=", 1)
            out[k] = v
    return out


def key(r):
    t = tags(r)
    return (t.get("workload", "?"), t.get("case", "?"), r.get("stand", "?"), int(r["cfg"]["rate"]))


def empty(r):
    """The run did not happen at all (the server did not come up, the connection did not open).
    This is NOT overload: overload is when there are numbers, but they measure a queue."""
    return int(r["counts"].get("measured", 0)) == 0


def queued_at(r, k):
    """At THIS percentile the generator's queue was measured, not the server.

    The open loop honestly includes the queue ON THE SERVER — that is what it exists
    for. But tlgen measures TWO latencies: `open_loop` (from the scheduled
    deadline) and `service` (from the send to the answer), and the difference between them is
    exactly the generator's own debt, nothing more. While it is small, the number
    speaks about the server; when it is hundreds of times larger than the service latency, the number
    speaks about the generator.

    Measured on this very suite: redis/mixed on the top step of the ladder —
    open_loop p99 158 ms at a service p99 of 0.22 ms, `overload=false` and an achieved
    rate of 99.5 %. A threshold on the debt COUNTER (`--max-backlog`, half a second) did not
    reach that far, because half a second of debt is not even a spoiled
    number any more but a different measurement.

    The rejection is PER PERCENTILE, and that came out of a measurement: if a whole run is
    discarded, then one hiccup of the generator at p99.99 (a gap of one and a half
    milliseconds with matching p50) carries away healthy p50 and p99 with it — on
    this suite more than half the points were lost that way. Debt in the tail spoils the tail
    and nothing else."""
    ol = r["latency"]["open_loop"]
    sv = r["latency"].get("service") or {}
    s, o = sv.get(k, 0), ol.get(k, 0)
    return bool(s > 0 and (o - s) > max(1_000_000, s))


def excluded_by_debt(a, p):
    """Whether the generator's debt excludes a percentile from the comparison. When judging by
    the service latency — no: debt does not spoil it, it is printed alongside."""
    return JUDGE == "open_loop" and p in a["queued"]


# The share of errors (timeouts, disconnects, application errors) at which a point is still
# judged. One hung connection per half a million requests is a defect, and it is
# printed next to the point, but it does not move the p99 tail; discarding the whole point
# because of it would mean hiding a latency comparison behind a single connection.
ERR_SHARE_MAX = 0.001


def errors(r):
    c = r["counts"]
    return int(c.get("timeouts", 0)) + int(c.get("conn_errors", 0)) + int(c.get("app_errors", 0))


def why_bad(r):
    """The reason a point is rejected — as a string, so that a `*` in the table is not mute."""
    c = r["counts"]
    if c.get("overload"):
        return "overload"
    if r.get("rate_achieved_pct", 100) < 98.0:
        return "rate %.1f%%" % r.get("rate_achieved_pct", 0)
    n, sent = errors(r), int(c.get("sent", 0))
    if n and n > ERR_SHARE_MAX * max(sent, 1):
        return "errors %d of %d" % (n, sent)
    return ""


def bad(r):
    return bool(why_bad(r))


def agg(group):
    """The median over repeats + the spread (max/min by p99)."""
    out = {}
    for k, _ in PCTS:
        vals = [lat(g)[k if k != "max" else "max_ns"] for g in group]
        out[k] = statistics.median(vals)
    p99s = [lat(g)["p99"] for g in group]
    out["debt_p99"] = statistics.median([debt_ns(g, "p99") for g in group])
    out["judge"] = "service" if all(lat(g) is g["latency"].get("service") for g in group) else "open_loop"
    out["spread"] = (max(p99s) / min(p99s)) if min(p99s) > 0 else 0.0
    out["thr"] = statistics.median([g["throughput_rps"] for g in group])
    out["n"] = len(group)
    out["bad"] = any(bad(g) for g in group)
    out["why"] = "; ".join(sorted({why_bad(g) for g in group if why_bad(g)}))
    out["errors"] = sum(errors(g) for g in group)
    out["empty"] = all(empty(g) for g in group)
    out["sys"] = statistics.median([g["syscalls"]["useful_per_req"] for g in group])
    out["queued"] = sorted(k for k, _ in PCTS if any(queued_at(g, k) for g in group))
    out["error"] = "; ".join(sorted({g["error"] for g in group if g.get("error")}))
    # The count of multi-millisecond answers is a SUM over repeats (the events are rare),
    # and next to it a mark of whether it is confirmed by the service latency of at least one of them.
    evs = [ev_counts(g) for g in group]
    out["ev"] = [sum(e[i] for e in evs) for i in range(len(EV_LEVELS))]
    out["ev_ok"] = [any(ev_trusted(g, t) for g in group) for t in EV_LEVELS]
    out["ev_cap"] = [sum(ev_service_cap(g, t) for g in group) for t in EV_LEVELS]
    out["n_req"] = min(lat(g).get("n", 0) for g in group)
    return out


def ev_counts(r):
    """How many answers in a run took longer than 1 and 4 ms — from `hist_ns`.

    Why the column. A percentile answers the question "how bad does it get", but
    it does not answer "how many times". A p99.99 of 5 ms may be ONE
    outlier per run, or it may be nine hundred events in a row; they need different
    cures, while they stand identically in the table. The count is taken from already collected
    points and needs no stand.

    WHAT EXACTLY IS COUNTED, AND WHY IT IS NOT EXACTLY THE NUMBER OF SERVER STALLS.
    `hist_ns` is a histogram of the OPEN LOOP (from the scheduled deadline); tlgen
    writes no other. In the open loop one server stall drags along
    the whole block of requests accumulated during it: an 8 ms stall at a rate of
    6.7 thousand req/s is one slowly served request and fifty
    overdue ones. So the number here is HOW MANY REQUESTS WERE AFFECTED, an upper
    bound on the number of stalls, not the stalls themselves. For comparing the sides that is enough
    (it is monotone in the number of stalls), and that is exactly how it should be used.

    Counted over ALL repeats of a point: the events are rare, and a median of (0, 1, 0)
    would say "zero".
    """
    out = [0] * len(EV_LEVELS)
    for lo, c in (r.get("hist_ns") or []):
        for i, t in enumerate(EV_LEVELS):
            if lo >= t:
                out[i] += c
    return out


# The grid of service percentiles by which the number of requests HONESTLY served slowly
# is bounded from above. tlgen does not write a full histogram of the service latency
# (hist_ns is the open loop), but the percentiles are there, and they suffice for a bound.
SVC_GRID = (("p50", 50.0), ("p75", 75.0), ("p90", 90.0), ("p99", 99.0),
            ("p99_9", 99.9), ("p99_99", 99.99), ("p99_999", 99.999))


def ev_service_cap(r, level):
    """How many requests COULD have been served longer than `level` — an upper bound.

    If the service p99.99 equals 724 µs, then no more than a hundredth of a percent of requests
    were served longer than 4 ms, however many the open loop counted.
    The bound answers the question "are those 907 the server or a schedule overrun".
    """
    l = lat(r)
    n = l.get("n", 0)
    if l.get("max_ns", 0) < level:
        return 0.0
    cap = float(n)
    for k, q in SVC_GRID:
        v = l.get(k, 0)
        if v and v < level:
            cap = min(cap, n * (1.0 - q / 100.0))
    return cap


def ev_trusted(r, level):
    """Whether the count at this level can be trusted, or whether it is all the generator's debt.

    The check that the column's own code must make (otherwise it lies silently).
    The open loop grows for two reasons: the server answered slowly and the generator
    was late for the deadline. The second is easy to tell apart: if NOT A SINGLE request was
    served longer than the level (the service maximum is below it), then everything the
    histogram counted above the level is a schedule overrun, and the server has
    nothing to do with it.

    This is not a precaution invented just in case: on the top steps of the ladder of one
    scoreboard the open-loop count gave 1.9 million answers "longer than 4 ms" (redis/mixed on Linux)
    with a generator debt of 190 ms — that is not the server, that is the instrument.
    """
    return lat(r).get("max_ns", 0) >= level


def ev_cell(a):
    """A cell of the column: "112 / 37".

    "debt" instead of a number — at this level NOT A SINGLE request was served longer,
    the whole open-loop count there is a schedule overrun. A `~` sign after a number means the
    count is LARGER than the upper bound from the service percentiles: that many requests
    were affected, but the server served a smaller part of them that long (the rest stood
    in the queue behind them). Without a mark the number agrees with the service latency and
    reads as is.
    """
    out = []
    for i, _level in enumerate(EV_LEVELS):
        if not a["ev_ok"][i]:
            out.append("debt" if a["ev"][i] else "0")
        else:
            out.append("%d%s" % (a["ev"][i], "~" if a["ev"][i] > a["ev_cap"][i] else ""))
    return " / ".join(out)


def tail_reqs(a, p):
    """How many measurements stand BEYOND percentile p at this point (over repeats —
    the minimum: a point is judged no better than its worst repeat)."""
    return a["n_req"] * (1.0 - PCT_LEVEL[p] / 100.0)


def thin(t, l, p):
    """A percentile beyond which either side has only a few requests."""
    return min(tail_reqs(t, p), tail_reqs(l, p)) < TAIL_MIN


def us(v):
    return "%.1f" % (v / 1000.0)


def _floor_read(path):
    try:
        return lat(json.load(open(path)))
    except Exception:
        return None


def floor_of(d):
    """The STAND's noise floor (lib/noisefloor.sh -> floor.json), if measured.

    A percentile at which the stand is no better than the host's own jitter
    cannot be compared: it speaks about the stand, not about the systems."""
    return _floor_read(os.path.join(d, "floor.json"))


def floor_jitter(floor):
    """The stand's jitter: how far its own tail goes from its own median.

    WHY NOT THE ABSOLUTE VALUE OF THE FLOOR. The floor is measured by the generator against nginx
    ON THE HOST, and its percentiles include the time of that nginx itself. Comparing guests'
    absolute numbers with it substitutes the question: the median of the host's nginx
    (38 µs on our stand) says nothing about whether the stand can tell
    25 µs from 37 in two guests. That question is answered by the SPREAD of the floor, not by its
    level: everything the stand adds of its own is the departure of the tail from the median.

    So a point is declared unresolvable where THE DIFFERENCE BETWEEN THE SIDES is no
    larger than that jitter. At p50 the jitter is zero — the host's jitter does not move the
    median — and the rule simply does not fire there, without exceptions.
    """
    base = floor.get("p50", 0)
    return {k: max(0, v - base) for k, v in floor.items() if isinstance(v, (int, float))}


def floor_for(d, r):
    """The floor that belongs TO THIS run: we look for `floor.json` next to its file and
    upwards through the tree to the report's root.

    A combined run (`run-all.sh`) measures the floor SEPARATELY for each workload and
    puts it into that workload's subdirectory — searching only in the root silently found no
    floor at all, and the summary compared points the stand cannot distinguish.
    Taking the worst floor over ALL workloads is wrong too: the floors were measured at
    different times, and jitter that happened during mongodb says nothing about
    what the stand could distinguish during redis. The "worst of three" rule
    is about three measurements of ONE floor in a row, not about someone else's."""
    root = os.path.abspath(d)
    cur = os.path.dirname(os.path.abspath(os.path.join(d, r.get("_file", ""))))
    while True:
        f = _floor_read(os.path.join(cur, "floor.json"))
        if f:
            return f
        if os.path.normpath(cur) == os.path.normpath(root) or len(cur) <= len(root):
            return None
        cur = os.path.dirname(cur)


def main(d):
    runs = load(d)
    if not runs:
        print("no tlgen reports in %s" % d)
        return 1

    groups = {}
    for r in runs:
        groups.setdefault(key(r), []).append(r)
    table = {k: agg(v) for k, v in groups.items()}

    floor_default = floor_of(d)
    floors = {}
    for k, v in groups.items():
        floors[k] = floor_for(d, v[0]) or floor_default
    workloads = sorted({(k[0], k[1]) for k in table})
    lines = ["# Run results", ""]
    lines.append("Directory: `%s`" % os.path.basename(os.path.abspath(d)))
    lines.append("")
    if JUDGE == "service":
        lines.append("The latency is the SERVICE one (from sending a request to the answer), microseconds; "
                     "the open loop on this stand measures the generator's own debt. "
                     "The \"debt p99\" column is by how much the open loop at p99 diverged from the "
                     "service one: that is a measure of the instrument, not of the systems. "
                     "A row with an asterisk is overload: the server did not hold the requested rate. "
                     "Dashes mean the run did not happen (the server did not come up or the connection "
                     "did not open); that is not a measurement result but a broken stand.")
    else:
        lines.append("The latency is from the SCHEDULED moment (the open loop), microseconds. "
                     "A row with an asterisk is overload: the server did not hold the requested rate, "
                     "and its percentiles measure the generator's queue, not the server. "
                     "Dashes mean the run did not happen (the server did not come up or the connection "
                     "did not open); that is not a measurement result but a broken stand.")
    lines.append("")
    lines.append("The \">1/>4 ms\" column is HOW MANY REQUESTS over all repeats of a point took "
                 "longer than a millisecond and longer than four (from `hist_ns`, i.e. by the OPEN "
                 "loop: one server stall drags the whole block of overdue requests with it, "
                 "so the number is an upper bound on the number of stalls, not the stalls themselves). The word "
                 "\"debt\" instead of a number means that at this level NOT A SINGLE request was served "
                 "longer: everything counted is a schedule overrun in the generator; "
                 "a `~` sign means the count is larger than the upper bound from the service percentiles, i.e. "
                 "that many requests were affected while only one of them was served that slowly. "
                 "A percentile answers \"how bad does it get\", this column answers \"how many times\".")
    lines.append("")
    if BROKEN:
        lines.append("**NOT PARSED: %d reports** — these are points that are missing, not \"skipped\": %s"
                     % (len(BROKEN), "; ".join("`%s` (%s)" % b for b in BROKEN[:6])
                        + (" …" if len(BROKEN) > 6 else "")))
        lines.append("")
    floor = floor_default or next((f for f in floors.values() if f), None)
    if floor:
        lines.append("**The stand's noise floor** (the same generator and the same server on the host, "
                     "without virtualisation): p99 %.1f, p99.9 %.1f, p99.99 %.1f, max %.1f µs. "
                     "A percentile comparable with that number speaks about the stand's jitter, "
                     "not about the systems being compared, and does NOT enter the comparison."
                     % (floor["p99"] / 1e3, floor["p99_9"] / 1e3,
                        floor["p99_99"] / 1e3, floor["max_ns"] / 1e3))
        lines.append("")

    summary = []
    unresolved = []
    thin_pts = []          # percentiles with only a few requests beyond them
    for wl, case in workloads:
        lines.append("## %s / %s" % (wl, case))
        lines.append("")
        lines.append("| stand | rate, req/s | actual | " + " | ".join(n for _, n in PCTS)
                     + " | >1/>4 ms | debt p99 | p99 spread | syscalls/req |")
        lines.append("|---|---:|---:|" + "---:|" * (len(PCTS) + 4))
        rates = sorted({k[3] for k in table if k[0] == wl and k[1] == case})
        for rate in rates:
            for stand in ("talaria", "linux"):
                k = (wl, case, stand, rate)
                if k not in table:
                    continue
                a = table[k]
                if a["empty"]:
                    lines.append("| %s | %d | — | %s | — | — | — | %s |"
                                 % (stand, rate, " | ".join("—" for _ in PCTS),
                                    a["error"] or "— (no reason recorded)"))
                    continue
                mark = " \\* (%s)" % a["why"] if a["bad"] else ""
                if not a["bad"] and a["errors"]:
                    mark = " (errors %d)" % a["errors"]
                lines.append("| %s%s | %d | %.0f | %s | %s | %s | %s |"
                             % (stand, mark, rate, a["thr"],
                                " | ".join(us(a[p]) for p, _ in PCTS), ev_cell(a),
                                us(a["debt_p99"]),
                                "%.2fx | %.1f" % (a["spread"], a["sys"])))
        lines.append("")

        # Comparison at equal rates — only where both sides are clean.
        for rate in rates:
            kt, kl = (wl, case, "talaria", rate), (wl, case, "linux", rate)
            if kt in table and kl in table and not table[kt]["bad"] and not table[kl]["bad"] \
                    and not table[kt]["empty"] and not table[kl]["empty"]:
                t, l = table[kt], table[kl]
                for p, nm in (("p50", "p50"), ("p99", "p99"), ("p99_9", "p99.9"), ("p99_99", "p99.99")):
                    if l[p] <= 0:
                        continue
                    # The generator's debt at this percentile on either side is
                    # a number about the instrument, not about the systems (only in the open loop).
                    if excluded_by_debt(t, p) or excluded_by_debt(l, p):
                        continue
                    # There are only a few requests beyond the percentile — there is nothing to judge.
                    if thin(t, l, p):
                        thin_pts.append((wl, case, rate, nm, min(tail_reqs(t, p), tail_reqs(l, p))))
                        continue
                    # The difference is smaller than the stand's own jitter — it cannot
                    # tell it apart, and the number speaks about the host, not about the systems.
                    fl = floors.get(kt) or floors.get(kl)
                    if fl and abs(t[p] - l[p]) <= floor_jitter(fl).get(p, 0):
                        unresolved.append((wl, case, rate, nm))
                        continue
                    summary.append((wl, case, rate, nm, t[p] / l[p]))

    if summary:
        lines.append("## Talaria against Linux (below one — Talaria is faster; judged by %s)"
                     % ("the service latency" if JUDGE == "service" else "the open loop"))
        lines.append("")
        lines.append("| workload | case | rate | percentile | ratio |")
        lines.append("|---|---|---:|---|---:|")
        for wl, case, rate, nm, ratio in summary:
            lines.append("| %s | %s | %d | %s | %.2f |" % (wl, case, rate, nm, ratio))
        lines.append("")
        wins = sum(1 for *_x, r in summary if r < 0.95)
        ties = sum(1 for *_x, r in summary if 0.95 <= r <= 1.05)
        loss = sum(1 for *_x, r in summary if r > 1.05)
        lines.append("Comparable points in total: %d — Talaria is faster in %d, parity in %d, "
                     "slower in %d." % (len(summary), wins, ties, loss))
        lines.append("")
    else:
        lines.append("There are no comparable points: at equal rates there were no clean runs "
                     "on both sides.")
        lines.append("")

    if thin_pts:
        lines.append("Not judged (only a few requests beyond the percentile, fewer than %d): %d points — "
                     "%s. The ratio there speaks of who was unlucky with one answer."
                     % (TAIL_MIN, len(thin_pts),
                        ", ".join(sorted({"%s/%s %s" % (u[0], u[1], u[3]) for u in thin_pts}))))
        lines.append("")

    if unresolved:
        lines.append("Not resolvable by the stand (both sides inside the noise floor): %d points — "
                     "%s." % (len(unresolved),
                              ", ".join(sorted({"%s/%s %s" % (u[0], u[1], u[3]) for u in unresolved}))))
        lines.append("")

    open(os.path.join(d, "report.md"), "w").write("\n".join(lines) + "\n")
    with open(os.path.join(d, "report.json"), "w") as f:
        json.dump({"%s|%s|%s|%d" % k: v for k, v in table.items()}, f, indent=1, ensure_ascii=False)
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else "."))
