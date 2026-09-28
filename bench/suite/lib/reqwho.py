#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""The parsing of the instrument of step 1 of session 43: TO WHOM THE CORE GOES FROM A THREAD WITH A REQUEST IN PROGRESS.

The instrument (`talaria.appprof=1`, `netprof::dump_sched`) prints ACCUMULATED counters into
every `stat:` block, so they cannot be read one after another: between the snapshots
there lie the warm-up, the database import and the idle time. The shares are computed BY THE DIFFERENCE of
neighbouring snapshots — the same rule as in `statdiff.py`/`sysdiff.py`, and for the same reason.

The answer for whose sake the instrument was written is one line: is the neighbour that took the core from a
thread in the middle of a request itself in a request or not. If it is "also in a request" —
the policy "the thread with a request in progress returns first" degenerates into a long quantum, and
that one has already been measured and is harmful (session 42, phase T2).

    reqwho.py <console-...txt> [more files]
"""
import re
import sys

CLASSES = ["also in a request", "ring 3 without a request", "a network waiter", "a kernel thread", "nobody"]
RE_ALL = re.compile(r"app rotations in the middle of a request: in total (\d+)")
RE_WHO = re.compile(r"the core went to: (.+?)\s\s+(\d+) \(")
RE_NQ = re.compile(r"threads in a request on this core: 0:(\d+) 1:(\d+) 2:(\d+) 3:(\d+) 4\+:(\d+)")
RE_RET = re.compile(r"the return of the removed thread: n=(\d+) avg=(\d+) p50=(\d+) p90=(\d+) p99=(\d+) p99\.9=(\d+) max=(\d+)")


def snapshots(path):
    """The instrument's snapshots from the console: [(total, {class: n}, [1,2,3,4+], the return|None)]."""
    out = []
    cur = None
    for line in open(path, encoding="utf-8", errors="replace"):
        m = RE_ALL.search(line)
        if m:
            if cur:
                out.append(cur)
            cur = {"all": int(m.group(1)), "who": {}, "nq": None, "ret": None}
            continue
        if cur is None:
            continue
        m = RE_WHO.search(line)
        if m:
            cur["who"][m.group(1).strip()] = int(m.group(2))
            continue
        m = RE_NQ.search(line)
        if m:
            cur["nq"] = [int(x) for x in m.groups()]
            continue
        m = RE_RET.search(line)
        if m:
            cur["ret"] = [int(x) for x in m.groups()]
            out.append(cur)
            cur = None
    if cur:
        out.append(cur)
    return out


def diff(a, b):
    """The difference of the snapshots b - a (the snapshots are cumulative)."""
    who = {c: b["who"].get(c, 0) - a["who"].get(c, 0) for c in CLASSES}
    tot = sum(who.values())
    nq = None
    if a["nq"] and b["nq"]:
        nq = [y - x for x, y in zip(a["nq"], b["nq"])]
    return tot, who, nq


def main(paths):
    rows = []
    for p in paths:
        snaps = snapshots(p)
        if len(snaps) < 2:
            print("== %s: %d snapshots — there is nothing to take differences from" % (p, len(snaps)))
            continue
        print("== %s: %d snapshots" % (p, len(snaps)))
        print("| window | rotations in the middle of a request | " + " | ".join(CLASSES) + " |")
        print("|---|---:|" + "---:|" * len(CLASSES))
        for i in range(1, len(snaps)):
            tot, who, nq = diff(snaps[i - 1], snaps[i])
            if tot <= 0:
                continue
            cells = ["%d (%.1f %%)" % (who[c], 100.0 * who[c] / tot) for c in CLASSES]
            print("| %d | %d | %s |" % (i, tot, " | ".join(cells)))
            rows.append((tot, who, nq))
        r = snaps[-1]["ret"]
        if r:
            print("the return of the removed thread (cumulative): n=%d avg=%.1f p50=%.1f p90=%.1f p99=%.1f p99.9=%.1f max=%.1f us"
                  % (r[0], r[1] / 1000.0, r[2] / 1000.0, r[3] / 1000.0, r[4] / 1000.0, r[5] / 1000.0, r[6] / 1000.0))
        if snaps[-1]["nq"]:
            nq = snaps[-1]["nq"]
            s = sum(nq) or 1
            print("threads in a request on the core at the moment of a rotation (cumulative): "
                  + " ".join("%s:%d (%.1f %%)" % (k, v, 100.0 * v / s)
                             for k, v in zip(["0", "1", "2", "3", "4+"], nq)))
    if not rows:
        return 1
    print()
    print("== THE ANSWER TO THE FORK (over the windows with rotations, %d windows)" % len(rows))
    for c in CLASSES:
        sh = sorted(100.0 * w[c] / t for t, w, _ in rows if t > 0)
        if not sh:
            continue
        spread = (sh[-1] / sh[0]) if sh[0] > 0 else float("inf")
        print("  %-26s %.1f-%.1f %% (the spread x%.2f)" % (c + ":", sh[0], sh[-1], spread))
    tot = sum(t for t, _, _ in rows)
    inreq = sum(w[CLASSES[0]] for _, w, _ in rows)
    print("  IN TOTAL: the core goes to a thread IN A REQUEST in %.1f %% of the rotations, NOT in a request — in %.1f %%"
          % (100.0 * inreq / tot, 100.0 * (tot - inreq) / tot))
    return 0


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(2)
    sys.exit(main(sys.argv[1:]))
