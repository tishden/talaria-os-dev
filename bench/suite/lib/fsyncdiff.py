#!/usr/bin/env python3
"""The breakdown of fsync's phases BY THE DIFFERENCE of two console snapshots.

    fsyncdiff.py <console file> [--first N] [--last M]

WHY A DIFFERENCE. The kernel's counters are cumulative since the boot, and the workload's
START-UP is mixed into them: for mongod that is 31 ms per fsync call against 2.4 ms in the steady
state — a thirteenfold difference (the rule of session 37). One may judge only by the difference of
neighbouring `stat:` snapshots, and there are many snapshots in the console, while subtracting them by
hand is an occupation that has already cost a session.

WHAT IS COUNTED. For every phase: how many calls and how many us were added
between the first and the last snapshot of the window, the price per call and the share of the `fsync`
phase. Separately — the instrument's line "the journal's batch waited for two" (see `fs::ABSW_*`) and
THE VERDICT on the change "a commit without a second round": it takes THE COPIES off the sequential
chain, but not THE DATA, so its gain is min(commit, the copies - the data).
"""
import re
import sys

# The EC2 console inserts its own timestamp RIGHT INTO THE MIDDLE of a line
# ("... 203 us ea[2026-09-18T13:44:51.982390]ch ..."), and without removing it
# the parsing loses a whole phase — silently, which is unacceptable for an instrument.
STAMP = re.compile(r"\[\d{4}-\d\d-\d\dT[\d:.]+\]")
SNAP = re.compile(r"stat: --- snapshot at (\d+) ms")
PHASE_LINE = re.compile(r"stat: fsync by phases:(.*)")
PHASE = re.compile(r"\s*(.+?) (\d+) \((\d+) us each, the worst (\d+)\)")
ABSW = re.compile(r"the journal's batch waited for two (\d+) times: the deferred DATA (\d+) us "
                  r".*?its own COPIES (\d+) us")


def snapshots(path):
    """[(ms since the boot, {phase: (calls, total us)}, (n, the data, the copies) | None)]"""
    out, cur_ms, phases, absw = [], None, None, None
    for raw in open(path, "rb").read().decode("utf-8", "replace").splitlines():
        raw = STAMP.sub("", raw)
        m = SNAP.search(raw)
        if m:
            if phases and cur_ms is not None:
                out.append((cur_ms, phases, absw))
            cur_ms, phases, absw = int(m.group(1)), None, None
            continue
        m = PHASE_LINE.search(raw)
        if m:
            phases = {}
            for part in m.group(1).split(";"):
                p = PHASE.match(part)
                if p:
                    name, n, per = p.group(1).strip(), int(p.group(2)), int(p.group(3))
                    phases[name] = (n, n * per)
            continue
        m = ABSW.search(raw)
        if m and cur_ms is not None:
            absw = (int(m.group(1)), int(m.group(2)), int(m.group(3)))
    if phases and cur_ms is not None:
        out.append((cur_ms, phases, absw))
    # The first snapshot in the console's window is often TRUNCATED (a 64 KB window starts from
    # the middle of a block) — it may have no phases at all; such ones are already filtered out.
    return out


def main():
    # SEVERAL FILES ARE NOT A CONVENIENCE BUT A CLOSED DEFECT OF THE INSTRUMENT. The A/B phase script
    # called the instrument as `fsyncdiff.py console-P0.txt console-P1.txt`, while ONLY the first
    # file was parsed: the second A/B phase was silently not printed at all. The same class
    # as "ONLY_RATES silently skips a phase" (session 40) — an instrument must speak
    # about everything it was given.
    first, last = 0, -1
    if "--first" in sys.argv:
        first = int(sys.argv[sys.argv.index("--first") + 1])
    if "--last" in sys.argv:
        last = int(sys.argv[sys.argv.index("--last") + 1])
    skip = set()
    for flag in ("--first", "--last"):
        if flag in sys.argv:
            i = sys.argv.index(flag)
            skip |= {i, i + 1}
    paths = [a for i, a in enumerate(sys.argv[1:], 1) if i not in skip]
    if not paths:
        print(__doc__)
        return 2
    bad = 0
    for path in paths:
        if len(paths) > 1:
            print("==", path)
        bad |= one(path, first, last)
        if len(paths) > 1:
            print()
    return bad


def one(path, first, last):
    snaps = snapshots(path)
    if len(snaps) < 2:
        print("there are fewer than two snapshots with phases (%d) — there is nothing to make a difference from" % len(snaps))
        print("check that the phase ran with talaria.stat= and the console was captured DURING the run")
        return 2
    a, b = snaps[first], snaps[last]
    print("the window: the snapshot at %d ms -> %d ms (%.0f s), snapshots in the file %d"
          % (a[0], b[0], (b[0] - a[0]) / 1000.0, len(snaps)))
    calls = b[1].get("fsync", (0, 0))[0] - a[1].get("fsync", (0, 0))[0]
    whole = b[1].get("fsync", (0, 0))[1] - a[1].get("fsync", (0, 0))[1]
    if calls <= 0:
        print("there was not a single fsync over the window — take another window")
        return 2
    print("fsync: %d calls, %d us per call\n" % (calls, whole // calls))
    print("| phase | times | us per fsync call | share |")
    print("|---|---:|---:|---:|")
    rows = []
    for name in b[1]:
        n0, t0 = a[1].get(name, (0, 0))
        n1, t1 = b[1][name]
        dn, dt = n1 - n0, t1 - t0
        if dn <= 0 and dt <= 0:
            continue
        rows.append((dt, name, dn))
    for dt, name, dn in sorted(rows, key=lambda r: -r[0]):
        if name == "fsync":
            continue
        print("| %s | %d | %d | %.1f %% |" % (name, dn, dt // calls, 100.0 * dt / whole))
    # The instrument "the data or the copies" and the price of step 2's change.
    if a[2] and b[2]:
        (n0, d0, o0), (n1, d1, o1) = a[2], b[2]
        dn = n1 - n0
        if dn > 0:
            dd = (n1 * d1 - n0 * d0) // dn
            do = (n1 * o1 - n0 * o0) // dn
            commit = 0
            for name in ("commit",):
                c0, c1 = a[1].get(name, (0, 0)), b[1].get(name, (0, 0))
                cn = c1[0] - c0[0]
                if cn > 0:
                    commit = (c1[1] - c0[1]) // cn
            win = min(commit, max(0, do - dd))
            print("\nthe journal's batch waited for two %d times: THE DATA %d us, THE COPIES %d us" % (dn, dd, do))
            print("the critical path: %s" % ("THE DATA" if dd >= do else "the copies"))
            print("the ceiling of the change \"a commit without a second round\" = min(commit %d, copies-data %d) = %d us per call"
                  % (commit, do - dd, win))
    return 0


if __name__ == "__main__":
    sys.exit(main())
