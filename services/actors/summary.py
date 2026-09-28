#!/usr/bin/env python3
# Linux vs Talaria summary from results-*.txt (the RESULT lines of actor-bench).
#
# THE JUDGING RULE IS NOT "WHOSE MEDIAN IS LOWER". The matrix varies between runs
# more than the difference we look for: one and the same build gave 38/17/14,
# 41/12/16 and 34/19/16. A median of disagreeing numbers picks the winner by
# lot, so a point is judged ONLY when the sides separated completely: every boot
# of one side is better than every boot of the other. Otherwise it is "not
# judged" — an honest answer, not a gap.
#
# NEXT TO THE VERDICT — WHERE THE NOISE LIVES. The spread between boots and the
# spread within a boot are printed separately: they are cured by different
# means (more boots vs more repeats), and while they were one number the
# choice could not be made.
#
# usage: summary.py [results-talaria.txt results-linux.txt] [--metric p50|p99|thr]
import sys, os, statistics

def load(path):
    """(scen,disp,pin) -> {boot: [kv, ...]}; the key "hung" collects hung runs."""
    rows = {}
    if not os.path.exists(path):
        return rows
    for line in open(path, encoding="utf-8", errors="replace"):
        if line.startswith("HUNG "):
            kv = dict(p.split("=", 1) for p in line.split()[1:])
            rows.setdefault((kv["scen"], kv["disp"], kv["pin"]), {}).setdefault("hung", []).append(kv)
            continue
        if not line.startswith("RESULT "):
            continue
        kv = dict(p.split("=", 1) for p in line.split()[1:])
        key = (kv["scen"], kv["disp"], kv["pin"])
        rows.setdefault(key, {}).setdefault(kv.get("boot", "1"), []).append(kv)
    return rows

def fmt_ns(v):
    v = float(v)
    return f"{v:.0f}" if v < 100000 else f"{v/1000:.0f}k"

def fmt_thr(v):
    v = float(v)
    return f"{v/1e6:.2f}M" if v >= 1e6 else f"{v/1e3:.0f}k"

def per_boot(entry, metric):
    """The metric per boot — the unit the verdicts are reached on."""
    out = {}
    for boot, rows in entry.items():
        if boot == "hung":
            continue
        vals = [float(r[metric]) for r in rows if metric in r]
        if vals:
            out[boot] = vals
    return out

def noise(bootvals):
    """(spread between boots, spread within a boot) in the units of the metric."""
    meds = [statistics.median(v) for v in bootvals.values()]
    between = max(meds) - min(meds) if len(meds) > 1 else 0.0
    inner = [max(v) - min(v) for v in bootvals.values() if len(v) > 1]
    within = statistics.median(inner) if inner else 0.0
    return between, within

# How many TIMES the repeats WITHIN one boot may differ before a point stops
# being judged. Not strictness for its own sake but a defect found: the `raw`
# scenarios on Linux are NOT IDEMPOTENT within a process — the first repeat
# gives 206 ns, the second and third 16,980 each (flood raw: 54 vs 1124). The
# median of three such numbers means nothing but looks convincing: it credited
# Talaria with an "82x win". On Talaria the same scenario is stable
# (183/187/185), so the remainder is in the benchmark on that side, not in the
# systems being compared.
REP_DISAGREE = 2.0

def rep_disagreement(bootvals):
    """The largest disagreement of repeats WITHIN a boot (a ratio)."""
    worst = 1.0
    for v in bootvals.values():
        lo, hi = min(v), max(v)
        if lo > 0 and hi / lo > worst: worst = hi / lo
    return worst

def judge(t, l, lower_is_better):
    """Did the sides separate COMPLETELY: every boot of one better than every boot of the other."""
    if not t or not l:
        return None, None
    if max(rep_disagreement(t), rep_disagreement(l)) >= REP_DISAGREE:
        return None, "repeats"         # not judged: the repeats disagree
    tm = [statistics.median(v) for v in t.values()]
    lm = [statistics.median(v) for v in l.values()]
    mt, ml = statistics.median(tm), statistics.median(lm)
    ratio = (mt / ml) if ml else float("nan")
    if len(tm) < 2 or len(lm) < 2:
        sep = None                      # one boot never gives a verdict
    elif max(tm) < min(lm):
        sep = "ours" if lower_is_better else "theirs"
    elif min(tm) > max(lm):
        sep = "theirs" if lower_is_better else "ours"
    else:
        sep = None
    return ratio, sep

def main():
    d = os.path.dirname(os.path.abspath(__file__))
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    tal = load(args[0] if len(args) > 0 else f"{d}/build/results-talaria.txt")
    lin = load(args[1] if len(args) > 1 else f"{d}/build/results-linux.txt")
    keys = sorted(set(tal) | set(lin))
    metrics = [("p50", True), ("p99", True), ("thr", False)]
    print("| scenario | disp | pin | " + " | ".join(
        f"Linux {m} | Talaria {m} | verdict {m}" for m, _ in metrics) + " |")
    print("|---" * (3 + 3 * len(metrics)) + "|")
    score = {m: [0, 0, 0] for m, _ in metrics}    # ours / not judged / theirs
    noises = []
    for k in keys:
        t, l = tal.get(k, {}), lin.get(k, {})
        cells = []
        for m, low in metrics:
            tb, lb = per_boot(t, m), per_boot(l, m)
            f = fmt_thr if m == "thr" else fmt_ns
            def show(bv, entry):
                if not bv:
                    return "hung" if "hung" in entry else "—"
                return f(statistics.median([statistics.median(v) for v in bv.values()]))
            ratio, sep = judge(tb, lb, low)
            if sep == "repeats":
                verdict = "REPEATS DISAGREE"
                score[m][1] += 1
            elif sep is None:
                verdict = "not judged"
                score[m][1] += 1
            else:
                verdict = f"{sep} {ratio:.2f}x" if ratio == ratio else sep
                score[m][0 if sep == "ours" else 2] += 1
            cells += [show(lb, l), show(tb, t), verdict]
            if m == "p99" and tb:
                b, w = noise(tb)
                med = statistics.median([statistics.median(v) for v in tb.values()])
                if med:
                    noises.append((k, b / med, w / med))
        print(f"| {k[0]} | {k[1]} | {k[2]} | " + " | ".join(cells) + " |")
    print()
    for m, _ in metrics:
        a, u, c = score[m]
        print(f"score {m}: ours {a} / not judged {u} / theirs {c}")
    if noises:
        print()
        print("NOISE (Talaria p99, share of the median): between boots / within a boot")
        b = statistics.median([x[1] for x in noises]); w = statistics.median([x[2] for x in noises])
        print(f"  median over the matrix: between boots {b*100:.0f} %, within a boot {w*100:.0f} %")
        worst = sorted(noises, key=lambda x: -max(x[1], x[2]))[:5]
        for k, bb, ww in worst:
            print(f"  {k[0]}/{k[1]}/{k[2]}: between boots {bb*100:.0f} %, within a boot {ww*100:.0f} %")

if __name__ == "__main__":
    main()
