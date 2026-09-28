#!/usr/bin/env python3
"""The instruments of steps 2 and 3 of session 39 — BY THE DIFFERENCE of console snapshots.

    statdiff.py <console file> [<another file> ...]

WHY SEPARATELY FROM fsyncdiff.py. That one counts the phases of `fsync`; here are three
`stat:` lines that appeared in session 39, and each has its own arithmetic:

  * "the data per fsync(fd) call" — how many blocks of DATA there are per call and
    how many blocks the file has in total. The kernel prints AVERAGES (the sum/the calls), and
    subtracting one snapshot from another is possible only by restoring the sums;
  * "the age of the dirty data at the flush" — the same with an average per block, plus
    "the worst" (it is a cumulative maximum and is not subtracted). Since session 40 the kernel
    prints a line PER POPULATION (who carried the block away: `fsync(fd)` itself, the threshold from
    `write`, the eviction of a slot, sync/checkpoint) plus the final "ALL populations";
  * "we waited for a foreign transaction at its phase" — the breakdown of the wait by the phase of
    the journal's HOLDER, in episodes and microseconds.

WHY A DIFFERENCE (the rule of session 37, the same one here). The counters are cumulative since the
boot, and the workload's START-UP sits in them: in the first snapshot of P0 "dirty blocks
per call" was 2743 with 13730 blocks in the file (the data import), while in the steady state
it was 50 with 25281. Judging by the last snapshot would mean mixing the import with the
measurement; the difference between them is fiftyfold.

The EC2 console inserts its own timestamp RIGHT INTO THE MIDDLE of a line, so the
stamps are removed before parsing — as in fsyncdiff.py (without that the instrument would silently
lose whole lines).
"""
import re
import sys

STAMP = re.compile(r"\[\d{4}-\d\d-\d\dT[\d:.]+\]")
SNAP = re.compile(r"stat: --- snapshot at (\d+) ms")
FSD = re.compile(r"the data per fsync\(fd\) call: calls (\d+), dirty blocks (\d+) per call, "
                 r"the file has (\d+) blocks")
AGE = re.compile(r"the age of the dirty data at the flush, (?:the population (.+?)|(ALL) populations)"
                 r": portions (\d+), blocks (\d+), the average (\d+) us, the worst (\d+) us")
# STEP 1 OF SESSION 41: THE PRICE OF PRE-EMPTION IS JUDGED BY THE WORST FLUSH, NOT BY THE AVERAGE ONE.
# Session 40 paid with a batch (44 flushes of 136 blocks) and spoiled p99.9 fourfold;
# the average over the run did not show that — the price is paid by the thread that got UNDER
# THE KICK. Hence "THE WORST" (a cumulative maximum, it is not subtracted) and "kicks without
# a pass over the cache" (the clock `DN_OLD` instead of a walk) are parsed separately.
KICK = re.compile(r"the pre-emptive flush by age: (\d+) times, (\d+) blocks "
                  r"\((\d+) per time, THE WORST (\d+)\), kicks without a pass over the cache (\d+)")
WAIT = re.compile(r"we waited for a foreign transaction at its phase:(.*)")
WITEM = re.compile(r"\s*([^—;]+?) — (\d+) episodes?, (\d+) us \(the worst (\d+)\)")

ALL = "ALL populations"


def snapshots(path):
    """The snapshots that have BOTH instruments of step 2 (otherwise there is nothing to subtract)."""
    out, cur = [], None
    for raw in open(path, "rb").read().decode("utf-8", "replace").splitlines():
        raw = STAMP.sub("", raw)
        m = SNAP.search(raw)
        if m:
            if cur:
                out.append(cur)
            cur = {"ms": int(m.group(1))}
            continue
        if cur is None:
            continue
        m = FSD.search(raw)
        if m:
            calls, per_call, file_blk = (int(g) for g in m.groups())
            cur["fsd"] = (calls, per_call * calls, file_blk * calls)
        m = AGE.search(raw)
        if m:
            who = m.group(1) or ALL
            chunks, blocks, avg, worst = (int(g) for g in m.groups()[2:])
            cur.setdefault("age", {})[who] = (chunks, blocks, avg * blocks, worst)
        m = KICK.search(raw)
        if m:
            f, blk, _per, worst, skip = (int(g) for g in m.groups())
            cur["kick"] = (f, blk, worst, skip)
        m = WAIT.search(raw)
        if m:
            cur["wait"] = {k.strip(): (int(n), int(t), int(x))
                           for k, n, t, x in WITEM.findall(m.group(1))}
    if cur:
        out.append(cur)
    return [s for s in out if "fsd" in s and "age" in s]


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    bad = 0
    for path in sys.argv[1:]:
        ss = snapshots(path)
        print("==", path, "— snapshots with the instrument:", len(ss))
        if len(ss) < 2:
            print("   SKIPPED: fewer than two snapshots — there is nothing to subtract")
            bad = 1
            continue
        a, b = ss[0], ss[-1]
        print("   the window: %d -> %d ms (%d s)" % (a["ms"], b["ms"], (b["ms"] - a["ms"]) / 1000))
        dc = b["fsd"][0] - a["fsd"][0]
        dd = b["fsd"][1] - a["fsd"][1]
        df = b["fsd"][2] - a["fsd"][2]
        if dc:
            print("   fsync(fd) with data: %d calls, DIRTY blocks %.1f per call, "
                  "the file has %.0f blocks -> they arrived clean %.2f %%"
                  % (dc, dd / dc, df / dc, 100 - dd * 100.0 / max(df, 1)))
        # THE AGE BY POPULATION (step 1 of session 40). While the population was one,
        # the "2.7-3.2 s" had to be divided between those who carried the block away BY AN INFERENCE
        # over two independent counters. Now it is a measurement: every line has its own name.
        aa, ab = a["age"], b["age"]
        names = sorted(set(aa) | set(ab), key=lambda k: (k == ALL, k))
        for k in names:
            x, y = aa.get(k, (0, 0, 0, 0)), ab.get(k, (0, 0, 0, 0))
            dn, db, dsum = y[0] - x[0], y[1] - x[1], y[2] - x[2]
            if not db:
                continue
            print("   the age of the dirty data, the population \"%s\": portions %d, blocks %d, THE AVERAGE %.0f us; "
                  "the worst over the run %d us" % (k, dn, db, dsum / db, y[3]))
        tot = ab.get(ALL, (0, 0, 0, 0))[1] - aa.get(ALL, (0, 0, 0, 0))[1]
        fs = ab.get("fsync(fd)", (0, 0, 0, 0))[1] - aa.get("fsync(fd)", (0, 0, 0, 0))[1]
        if tot and fs:
            print("   the share of the fsync(fd) population in blocks: %.1f %%" % (fs * 100.0 / tot))
        ka, kb = a.get("kick"), b.get("kick")
        if kb:
            ka = ka or (0, 0, 0, 0)
            df_, dblk, dskip = kb[0] - ka[0], kb[1] - ka[1], kb[3] - ka[3]
            print("   the pre-emptive flush by age: %d times, %d blocks (%.1f per time), "
                  "THE WORST over the run %d blocks, kicks without a pass over the cache %d"
                  % (df_, dblk, dblk / max(df_, 1), kb[2], dskip))
        wa, wb = a.get("wait", {}), b.get("wait", {})
        keys = sorted(set(wa) | set(wb),
                      key=lambda k: -(wb.get(k, (0, 0, 0))[1] - wa.get(k, (0, 0, 0))[1]))
        for k in keys:
            n = wb.get(k, (0, 0, 0))[0] - wa.get(k, (0, 0, 0))[0]
            t = wb.get(k, (0, 0, 0))[1] - wa.get(k, (0, 0, 0))[1]
            if n or t:
                print("   we waited at the phase \"%s\": %d episodes, %d us (the worst over the run %d)"
                      % (k, n, t, wb.get(k, (0, 0, 0))[2]))
    return bad


if __name__ == "__main__":
    sys.exit(main())
