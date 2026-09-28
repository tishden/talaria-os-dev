#!/usr/bin/env python3
"""A summary over ALL workloads at once: one table on which a decision is made.

    summary.py <results directory> [more directories...] [--md file]

Why this is separate from report.py. That one prints EVERY point — the right format
when working through a single workload, and unreadable when there are ten workloads and
the question is "are we ready to show these numbers at all". Here the answer
is given in one table: per workload — how many points are for us, how many are parity,
how many are against, separately for each percentile.

The three rules are the same as in report.py, and they matter more here, because a summary
tempts one to round things off:

* a point where AT LEAST ONE side was overloaded is not compared at all —
  its percentiles measure the generator's queue;
* a point where BOTH sides are inside the stand's noise floor is not compared at all —
  it speaks about the host's jitter, not about the systems. Such points are counted in
  a separate column rather than dissolving into "parity";
* losses are printed in the same font as wins, and the worst of them are listed
  at the bottom. A summary that hides where we fall behind is useless for work.

Workloads with SCALAR metrics (JVM: gigaflops, pass time, speedup) do not fit
the percentile table: for half the metrics "more" means "better".
For them a separate line is printed alongside — computed by `scalars.py`, where the direction
is given explicitly per metric name.
"""
import json
import os
import statistics
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import report            # noqa: E402  (load/agg/key/tags/bad/empty/floor_of)
import scalars           # noqa: E402  (direction)

PCTS = [("p50", "p50"), ("p99", "p99"), ("p99_9", "p99.9"), ("p99_99", "p99.99")]
WIN, TIE = 0.95, 1.05    # the same bounds as in report.py


def verdict(ratio):
    return "faster" if ratio < WIN else ("slower" if ratio > TIE else "parity")


def collect(dirs):
    """(workload -> {percentile -> [ratios]}, the list of worst points, rejection counters).

    REPEATS ARE REDUCED BY MEDIAN, NOT ADDED UP. There may be several input directories
    for one workload: `run-everything.sh 3` runs the actors, ClickHouse
    and the JVM three times each, because they are noisier than the difference being looked for. If the
    points of all directories were simply added up, those three harnesses would weigh three
    times as much in the final row as the tlgen workloads — and the "total" would mostly
    speak about whatever is noisiest. So one and the same point
    (workload, case, rate, percentile) met in several directories
    gives ONE ratio — the median over the repeats. That is exactly what the measurement
    rule prescribes: judge by concrete points and by no fewer than three
    runs.
    """
    per_point = {}      # (workload, case, rate, percentile) -> [(ratio, t, l, ev)]
    skipped_bad = skipped_floor = skipped_queue = skipped_thin = 0
    for d in dirs:
        runs = report.load(d)
        if not runs:
            continue
        groups = {}
        for r in runs:
            groups.setdefault(report.key(r), []).append(r)
        table = {k: report.agg(v) for k, v in groups.items()}
        # The floor is EACH workload's OWN: a combined run measures it separately before
        # each one (see report.floor_for).
        floors = {k: (report.floor_for(d, v[0]) or report.floor_of(d)) for k, v in groups.items()}
        for (wl, case, stand, rate) in list(table):
            if stand != "talaria":
                continue
            kt, kl = (wl, case, "talaria", rate), (wl, case, "linux", rate)
            if kl not in table:
                continue
            t, l = table[kt], table[kl]
            if t["empty"] or l["empty"]:
                continue
            if t["bad"] or l["bad"]:
                skipped_bad += 1
                continue
            for p, nm in PCTS:
                if l[p] <= 0:
                    continue
                if report.excluded_by_debt(t, p) or report.excluded_by_debt(l, p):
                    skipped_queue += 1
                    continue
                # Only a few requests stand beyond the percentile — the ratio there speaks
                # of who was unlucky with one answer (report.TAIL_MIN).
                if report.thin(t, l, p):
                    skipped_thin += 1
                    continue
                fl = floors.get(kt) or floors.get(kl)
                if fl and abs(t[p] - l[p]) <= report.floor_jitter(fl).get(p, 0):
                    skipped_floor += 1
                    continue
                ratio = t[p] / l[p]
                per_point.setdefault((wl, case, rate, nm), []).append(
                    (ratio, t[p], l[p], report.ev_cell(t), report.ev_cell(l)))
    # The median over repeats: a REAL run standing in the middle is taken, so that in
    # the list of worst points the sides' actual numbers stand next to the ratio rather than
    # averaged ones (averaged runs do not exist, and there is nothing to check them against).
    per_wl = {}
    worst = []
    unstable = []
    for (wl, case, rate, nm), v in per_point.items():
        v.sort(key=lambda x: x[0])
        ratio, tp, lp, evt, evl = v[len(v) // 2]
        # A POINT WHOSE REPEATS DISAGREE WITH ONE ANOTHER IS NOT JUDGED.
        #
        # The median always gives an answer — even when there is nothing to answer. For
        # `clickhouse ddl` three runs of ONE side gave 104 / 172 / 100 ms:
        # the case's own spread (73 ms) is larger than the difference being looked for (72), and
        # the median of such repeats is not "what the operation costs" but
        # "which run happened to be in the middle". The same holds for rates, where the ceiling wanders
        # between runs. Such a point goes into a separate column: it honestly
        # says "the measurement does not distinguish", instead of appointing a winner by lot.
        #
        # The criterion comes not from the spread of values but from THE DIVERGENCE OF CONCLUSIONS:
        # if the extreme repeats give different verdicts (faster / parity /
        # slower), then this point has no conclusion.
        if len(v) >= 3 and verdict(v[0][0]) != verdict(v[-1][0]):
            unstable.append((wl, case, rate, nm, v[0][0], v[-1][0], len(v)))
            continue
        per_wl.setdefault(wl, {}).setdefault(nm, []).append(ratio)
        worst.append((ratio, wl, case, rate, nm, tp / 1e3, lp / 1e3, len(v), evt, evl))
    return per_wl, worst, unstable, skipped_bad, skipped_floor, skipped_queue, skipped_thin


def scalar_pairs(dirs):
    """Pairs `<name>-talaria.txt` / `<name>-linux.txt` — workloads with scalars."""
    out = []
    for d in dirs:
        for root, _dirs, files in os.walk(d):
            for fn in sorted(files):
                if not fn.endswith("-talaria.txt"):
                    continue
                lin = os.path.join(root, fn.replace("-talaria.txt", "-linux.txt"))
                if os.path.exists(lin):
                    # The directory name goes into the label: with repeats there are several
                    # pairs with ONE name, and without it three "jvm" rows are indistinguishable — while
                    # the point of repeats is precisely to see the spread.
                    out.append((fn[:-len("-talaria.txt")].lstrip(".") + " (" +
                                os.path.basename(os.path.abspath(d)) + ")",
                                os.path.join(root, fn), lin))
    return out


def scalar_counts(tal, lin):
    """(better, parity, worse, no direction) — by `scalars.py`'s own count."""
    _rows, counts = scalars.compare(tal, lin)
    return counts


def stand_env(d):
    """The stand's stamp from <directory>/stand.env (run-remote.sh copies it at the end)."""
    env = {}
    try:
        for line in open(os.path.join(d, "stand.env")):
            if "=" in line and not line.startswith("#"):
                k, v = line.rstrip("\n").split("=", 1)
                env[k] = v
    except OSError:
        pass
    return env


def provenance(dirs):
    """A HEADER ABOUT WHERE THE NUMBERS CAME FROM — before the table, because without it the table
    lies silently. One scoreboard (`ec2all-20260911-213108`) was taken with a kernel from a
    DIRTY tree, while the A/B of the same day used two other binaries, and the conclusion
    "the same build moved by 20 %" rested on a false premise; that came out
    a day later from mk-ami.log. Each directory is its own stand bring-up (its own
    server instance), and the rule says points from DIFFERENT bring-ups
    are not comparable even by median. So this prints: which kernel, which
    tree, which Linux anchor, and LOUDLY — if the directories have different kernels, if the
    tree is dirty, if there is no stamp at all."""
    L = []
    envs = {os.path.basename(os.path.abspath(d)): stand_env(d) for d in sorted(dirs)}
    kern = {e.get("STAND_KERNEL_SHA", "?") for e in envs.values()}
    boots = {e.get("talaria_server_id", "?") for e in envs.values()}
    L.append("| directory | Talaria kernel (sha256) | tree | Talaria server | Linux AMI |")
    L.append("|---|---|---|---|---|")
    for name, e in envs.items():
        L.append("| `%s` | %s | %s | %s | %s |" % (
            name, e.get("STAND_KERNEL_SHA", "?"), e.get("STAND_KERNEL_TREE", "?"),
            e.get("talaria_server_id", "?"), e.get("STAND_LINUX_AMI", "?")))
    L.append("")
    warn = []
    if "?" in kern:
        warn.append("some directories have NO KERNEL STAMP (stand.env without STAND_KERNEL_SHA) — "
                    "which kernel produced these points cannot be established")
    if len(kern - {"?"}) > 1:
        warn.append("the directories were taken with DIFFERENT KERNELS (%s) — this is not one scoreboard but several"
                    % ", ".join(sorted(kern - {"?"})))
    if any(e.get("STAND_KERNEL_TREE", "").endswith("-dirty") for e in envs.values()):
        warn.append("the kernel was built from a DIRTY tree — the point cannot be tied to a commit")
    if len(boots) > 1:
        warn.append("each workload was taken on ITS OWN stand bring-up: the table's rows "
                    "are comparable with the Linux of their own hour, but NOT with one another and NOT with another scoreboard "
                    "(between bring-ups even the median moves)")
    for w in warn:
        L.append("**WARNING:** " + w + ".")
        L.append("")
    return L


def main(argv):
    dirs, md = [], None
    i = 0
    while i < len(argv):
        if argv[i] == "--md":
            i += 1
            md = argv[i]
        else:
            dirs.append(argv[i])
        i += 1
    if not dirs:
        print(__doc__)
        return 2

    per_wl, worst, unstable, skipped_bad, skipped_floor, skipped_queue, skipped_thin = collect(dirs)
    L = ["# Summary: Talaria against Linux", ""]
    # By name rather than in argument order: a summary of the same directories
    # must be the same text every time (selfcheck.sh checks that).
    L.append("Directories: " + ", ".join("`%s`" % os.path.basename(os.path.abspath(d)) for d in sorted(dirs)))
    L.append("")
    L += provenance(dirs)
    L.append("The ratio is Talaria to Linux at the same rate: **below one "
             "means Talaria is faster**. A point counts for us at a ratio "
             "< %.2f, against at > %.2f, in between it is parity." % (WIN, TIE))
    L.append("")
    L.append("In the tables of lagging and won points, next to the ratio stands \">1/>4 ms\" — "
             "how many requests over all repeats took longer than a millisecond and longer than "
             "four, on each side. That separates a tail of TENS AND HUNDREDS of events "
             "(a property of the system) from a tail of one or two outliers, and shows at once whether "
             "Linux has the same thing. The count comes from `hist_ns`, i.e. by the open loop: one "
             "stall drags a block of overdue requests with it, so the number is an upper bound on "
             "the number of stalls; \"debt\" instead of a number means that no request was served "
             "that long and the whole count is a schedule overrun in the generator, while a `~` sign means "
             "the count is above the upper bound from the service percentiles (that many were affected, "
             "while only one of them was served that slowly).")
    L.append("")

    if per_wl:
        L.append("| workload | " + " | ".join(nm for _, nm in PCTS) + " | points in total |")
        L.append("|---|" + "---|" * (len(PCTS) + 1))
        tot = {nm: [0, 0, 0] for _, nm in PCTS}
        for wl in sorted(per_wl):
            cells, n = [], 0
            for _, nm in PCTS:
                rs = per_wl[wl].get(nm, [])
                w = sum(1 for r in rs if r < WIN)
                t = sum(1 for r in rs if WIN <= r <= TIE)
                s = sum(1 for r in rs if r > TIE)
                tot[nm][0] += w; tot[nm][1] += t; tot[nm][2] += s
                n += len(rs)
                cells.append("%d / %d / %d" % (w, t, s) if rs else "—")
            L.append("| %s | %s | %d |" % (wl, " | ".join(cells), n))
        L.append("| **total** | %s | **%d** |"
                 % (" | ".join("**%d / %d / %d**" % tuple(tot[nm]) for _, nm in PCTS),
                    sum(sum(tot[nm]) for _, nm in PCTS)))
        L.append("")
        L.append("In a cell: faster / parity / slower.")
        L.append("")
    else:
        L.append("No comparable points with percentiles were found.")
        L.append("")

    sp = scalar_pairs(dirs)
    if sp:
        L.append("## Workloads with scalar metrics")
        L.append("")
        L.append("Here the metrics have DIFFERENT directions (gigaflops — more is better, pass "
                 "time — less), so no ratio as such is printed: "
                 "the direction is given per metric name (`lib/scalars.py`).")
        L.append("")
        L.append("| workload | better | parity | worse | no direction |")
        L.append("|---|---:|---:|---:|---:|")
        for name, tal, lin in sp:
            b, t, w, u = scalar_counts(tal, lin)
            # A pair without common metrics is not "zeros" but the wrong file: next to the actors
            # lies the raw output of the matrix (`RESULT scen=... disp=...`), which has nothing
            # to do with the "group metric value" scheme.
            if b + t + w + u == 0:
                continue
            L.append("| %s | %d | %d | %d | %d |" % (name, b, t, w, u))
        L.append("")

    if skipped_bad or skipped_floor or skipped_queue or skipped_thin:
        L.append("Not compared: %d runs with overload or errors on at least one "
                 "side, %d percentiles where the generator could not keep the schedule "
                 "(the difference between the open-loop and the service latency is its debt, not the server), "
                 "%d percentiles where the difference between the sides is smaller than the stand's own "
                 "jitter (that much it cannot distinguish), and %d percentiles with only "
                 "a few requests beyond them (fewer than %d — there the ratio speaks of "
                 "who was unlucky with one answer, not of the systems)."
                 % (skipped_bad, skipped_queue, skipped_floor, skipped_thin, report.TAIL_MIN))
        L.append("")

    if unstable:
        L.append("## Points whose repeats disagree with one another")
        L.append("")
        L.append("Neither for us nor against: the extreme repeats give DIFFERENT verdicts, i.e. "
                 "the point's own spread is larger than the difference being looked for. They do not enter the table "
                 "above at all — otherwise the winner would be appointed by a median of "
                 "disagreeing numbers.")
        L.append("")
        L.append("| workload | case | rate | percentile | min. ratio | max. ratio | repeats |")
        L.append("|---|---|---:|---|---:|---:|---:|")
        for wl, case, rate, nm, lo, hi, nrep in sorted(unstable):
            L.append("| %s | %s | %d | %s | %.2f | %.2f | %d |" % (wl, case, rate, nm, lo, hi, nrep))
        L.append("")

    if worst:
        worst.sort(reverse=True)
        L.append("## Where Talaria lags the most")
        L.append("")
        L.append("| workload | case | rate | percentile | Talaria, µs | Linux, µs | "
                 "ratio | >1/>4 ms ours | >1/>4 ms Linux | repeats |")
        L.append("|---|---|---:|---|---:|---:|---:|---:|---:|---:|")
        for ratio, wl, case, rate, nm, tv, lv, nrep, evt, evl in worst[:12]:
            if ratio <= TIE:
                break
            L.append("| %s | %s | %d | %s | %.1f | %.1f | %.2f | %s | %s | %d |"
                     % (wl, case, rate, nm, tv, lv, ratio, evt, evl, nrep))
        L.append("")
        best = [x for x in worst if x[0] < WIN]
        if best:
            L.append("## Where the win is largest")
            L.append("")
            L.append("| workload | case | rate | percentile | Talaria, µs | Linux, µs | "
                     "ratio | >1/>4 ms ours | >1/>4 ms Linux | repeats |")
            L.append("|---|---|---:|---|---:|---:|---:|---:|---:|---:|")
            for ratio, wl, case, rate, nm, tv, lv, nrep, evt, evl in best[-12:][::-1]:
                L.append("| %s | %s | %d | %s | %.1f | %.1f | %.2f | %s | %s | %d |"
                         % (wl, case, rate, nm, tv, lv, ratio, evt, evl, nrep))
            L.append("")

    text = "\n".join(L) + "\n"
    if md:
        open(md, "w").write(text)
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
