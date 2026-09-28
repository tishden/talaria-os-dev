#!/usr/bin/env python3
"""A promo page from a run's results: what Talaria gives, where it does not and why.

    promo.py <results directory> [more directories...] -o promo.html

The page has one rule and it is strict: **every point is shown**. A workload where
Talaria loses stands in the same table and in the same font as one where it
wins. A promo page with the losses removed is not a promo page but an
advertising leaflet: its numbers cannot be checked, so the rest cannot be believed
either.

What goes on the page:
  * the stand's passport (machine type, cores, memory, measurement discipline);
  * per workload — a "rate -> p99/p99.9" curve for both systems;
  * a summary of wins/parities/losses AS NUMBERS, without cherry-picking;
  * caveats: where a run was overloaded and why its percentiles do not mean
    what they seem to.
"""
import json
import os
import statistics
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import report            # noqa: E402  (a single rejection rule for points)

PCTS = [("p50", "p50"), ("p99", "p99"), ("p99_9", "p99.9"), ("p99_99", "p99.99")]


def load(dirs):
    runs = []
    for d in dirs:
        for root, _dirs, files in os.walk(d):
            for fn in files:
                if not fn.endswith(".json") or fn in ("report.json",) or fn.startswith("."):
                    continue
                try:
                    r = json.load(open(os.path.join(root, fn)))
                except Exception:
                    continue
                if r.get("schema") == 1:
                    runs.append(r)
    return runs


def tags(r):
    return dict(kv.split("=", 1) for kv in (r.get("tags") or "").split(",") if "=" in kv)


def bad(r):
    """The rejection is THE SAME as in report.py: two implementations of "this point does not count"
    diverge silently, and they diverge precisely on the promo page."""
    return report.bad(r)


def build(runs):
    groups = {}
    for r in runs:
        t = tags(r)
        k = (t.get("workload", "?"), t.get("case", "?"), r.get("stand", "?"), int(r["cfg"]["rate"]))
        groups.setdefault(k, []).append(r)
    table = {}
    for k, g in groups.items():
        row = {p: statistics.median([report.lat(x)[p if p != "max" else "max_ns"] for x in g])
               for p, _ in PCTS}
        row["max"] = statistics.median([report.lat(x)["max_ns"] for x in g])
        row["thr"] = statistics.median([x["throughput_rps"] for x in g])
        # Syscalls per request are not decoration: they are the direct price of a round trip, and they
        # explain the difference in p50 better than any words (nginx had three against
        # two — an extra segment per answer).
        row["sys"] = statistics.median([x["syscalls"]["useful_per_req"] for x in g])
        row["bad"] = any(bad(x) for x in g)
        row["n"] = len(g)
        table[k] = row
    return table


def esc(s):
    return (str(s).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;"))


def main(argv):
    dirs, out = [], "promo.html"
    i = 1
    while i < len(argv):
        if argv[i] == "-o" and i + 1 < len(argv):
            out = argv[i + 1]; i += 2
        else:
            dirs.append(argv[i]); i += 1
    if not dirs:
        print(__doc__); return 2

    runs = load(dirs)
    if not runs:
        print("no reports found"); return 1
    table = build(runs)

    # Comparable points: both sides at the same rate and both clean.
    cmp_rows, wins, ties, loss = [], 0, 0, 0
    for (wl, case, stand, rate) in list(table):
        if stand != "talaria":
            continue
        kl = (wl, case, "linux", rate)
        kt = (wl, case, "talaria", rate)
        if kl not in table or table[kl]["bad"] or table[kt]["bad"]:
            continue
        for p, nm in PCTS:
            if table[kl][p] > 0:
                ratio = table[kt][p] / table[kl][p]
                cmp_rows.append((wl, case, rate, nm, table[kt][p], table[kl][p], ratio))
                if ratio < 0.95: wins += 1
                elif ratio > 1.05: loss += 1
                else: ties += 1
    cmp_rows.sort()

    stands = sorted({t.get("stand_type", "") for t in map(tags, runs) if t.get("stand_type")})
    workloads = sorted({(k[0], k[1]) for k in table})

    h = []
    h.append("<title>Talaria: the numbers</title>")
    h.append("""<style>
:root{--bg:#fbfbf9;--fg:#1a1a18;--mut:#6b6b63;--line:#e2e2dc;--win:#1f7a4d;--los:#a33a2a;--tie:#6b6b63;--card:#fff}
:root:not([data-theme="light"]) @media (prefers-color-scheme: dark){}
@media (prefers-color-scheme: dark){:root:not([data-theme="light"]){--bg:#16171a;--fg:#e8e8e4;--mut:#9a9a92;--line:#2c2e33;--card:#1d1f23;--win:#57c48a;--los:#e2775f}}
:root[data-theme="dark"]{--bg:#16171a;--fg:#e8e8e4;--mut:#9a9a92;--line:#2c2e33;--card:#1d1f23;--win:#57c48a;--los:#e2775f}
body{background:var(--bg);color:var(--fg);font:15px/1.55 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;margin:0}
.wrap{max-width:1080px;margin:0 auto;padding:48px 24px 96px}
h1{font-size:32px;margin:0 0 8px;letter-spacing:-.02em}
h2{font-size:21px;margin:44px 0 12px;letter-spacing:-.01em}
h3{font-size:16px;margin:26px 0 8px}
p{margin:10px 0;max-width:70ch}
.lead{font-size:17px;color:var(--mut);max-width:70ch}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(190px,1fr));gap:12px;margin:20px 0}
.card{background:var(--card);border:1px solid var(--line);border-radius:10px;padding:14px 16px}
.card .k{font-size:12px;color:var(--mut);text-transform:uppercase;letter-spacing:.06em}
.card .v{font-size:26px;font-variant-numeric:tabular-nums;margin-top:4px}
.scroll{overflow-x:auto;-webkit-overflow-scrolling:touch}
table{border-collapse:collapse;width:100%;font-size:14px;font-variant-numeric:tabular-nums}
th,td{text-align:right;padding:7px 10px;border-bottom:1px solid var(--line);white-space:nowrap}
th:first-child,td:first-child,th:nth-child(2),td:nth-child(2){text-align:left}
th{font-weight:600;color:var(--mut);font-size:12px;text-transform:uppercase;letter-spacing:.05em}
.win{color:var(--win);font-weight:600}
.los{color:var(--los);font-weight:600}
.tie{color:var(--tie)}
.note{color:var(--mut);font-size:13px}
.bad{opacity:.55}
footer{margin-top:64px;color:var(--mut);font-size:13px;border-top:1px solid var(--line);padding-top:16px}
</style>""")
    h.append("<div class=wrap>")
    h.append("<h1>Talaria: numbers, not promises</h1>")
    h.append("<p class=lead>The same program, the same measuring instrument, "
             "identical machines. On the left is Linux tuned for low latency; on the right is "
             "Talaria. Below are all the measured points, including those where Talaria loses.</p>")

    h.append('<div class=grid>')
    h.append('<div class=card><div class=k>comparable points</div><div class=v>%d</div></div>' % len(cmp_rows))
    h.append('<div class=card><div class=k>faster</div><div class="v win">%d</div></div>' % wins)
    h.append('<div class=card><div class=k>parity</div><div class="v tie">%d</div></div>' % ties)
    h.append('<div class=card><div class=k>slower</div><div class="v los">%d</div></div>' % loss)
    h.append('</div>')
    h.append("<p class=note>Parity is a divergence of less than 5 %. A point counts as comparable "
             "only if BOTH sides held the requested rate: an overloaded run measures "
             "the generator's queue, not the server, and does not enter the comparison.</p>")
    if stands:
        h.append("<p class=note>Stand: %s.</p>" % esc(", ".join(stands)))

    h.append("<h2>How this is measured</h2>")
    h.append("<p><b>An open loop.</b> The schedule of sends is fixed in advance by the target rate "
             "and does not depend on whether the server keeps up. Latency is counted from the "
             "<i>scheduled</i> moment, not from the moment the request could be put into the socket. "
             "Otherwise a slow server receives fewer requests and improves its own tail.</p>")
    h.append("<p><b>One instrument.</b> The load generator is the same static binary "
             "on both systems. A difference in the numbers cannot be a difference between clients.</p>")
    h.append("<p><b>A load ladder.</b> One point says nothing: a system with low "
             "latency at 10 % load falls apart at 80 %. Every workload is walked "
             "as a ladder of rates from a shared ceiling.</p>")

    for wl, case in workloads:
        rates = sorted({k[3] for k in table if k[0] == wl and k[1] == case})
        if not rates:
            continue
        h.append("<h2>%s <span class=note>/ %s</span></h2>" % (esc(wl), esc(case)))
        h.append("<div class=scroll><table><thead><tr><th>system</th><th>rate, req/s</th>"
                 "<th>actual</th>" + "".join("<th>%s, µs</th>" % n for _, n in PCTS)
                 + "<th>max, µs</th><th>syscalls/req</th></tr></thead><tbody>")
        for rate in rates:
            for stand in ("talaria", "linux"):
                k = (wl, case, stand, rate)
                if k not in table:
                    continue
                a = table[k]
                cls = " class=bad" if a["bad"] else ""
                h.append("<tr%s><td>%s%s</td><td>%d</td><td>%.0f</td>%s<td>%.1f</td><td>%.1f</td></tr>"
                         % (cls, esc(stand), " (overload)" if a["bad"] else "", rate, a["thr"],
                            "".join("<td>%.1f</td>" % (a[p] / 1e3) for p, _ in PCTS),
                            a["max"] / 1e3, a.get("sys", 0.0)))
        h.append("</tbody></table></div>")

    if cmp_rows:
        h.append("<h2>The ratio of Talaria to Linux</h2>")
        h.append("<p class=note>Below one means Talaria is faster. All comparable points, "
                 "with no selection.</p>")
        h.append("<div class=scroll><table><thead><tr><th>workload</th><th>case</th>"
                 "<th>rate</th><th>percentile</th><th>Talaria, µs</th><th>Linux, µs</th>"
                 "<th>ratio</th></tr></thead><tbody>")
        for wl, case, rate, nm, tv, lv, ratio in cmp_rows:
            cls = "win" if ratio < 0.95 else ("los" if ratio > 1.05 else "tie")
            h.append("<tr><td>%s</td><td>%s</td><td>%d</td><td>%s</td><td>%.1f</td>"
                     "<td>%.1f</td><td class=%s>%.2f</td></tr>"
                     % (esc(wl), esc(case), rate, nm, tv / 1e3, lv / 1e3, cls, ratio))
        h.append("</tbody></table></div>")

    h.append("<footer>The numbers are reproducible: <code>bench/suite/</code> holds the generator, the workload "
             "descriptions, and the stand's terraform and ansible. A run: <code>run-remote.sh</code>, "
             "tearing the stand down: <code>infra/stand-down.sh</code>.</footer>")
    h.append("</div>")

    open(out, "w").write("\n".join(h) + "\n")
    print("page: %s (points %d: faster %d, parity %d, slower %d)"
          % (out, len(cmp_rows), wins, ties, loss))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
