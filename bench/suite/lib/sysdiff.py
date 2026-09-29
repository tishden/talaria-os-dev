#!/usr/bin/env python3
"""THE PRICE OF A SYSCALL BY THE DIFFERENCE of console snapshots (`talaria.sysprof=1`).

    sysdiff.py <console file> [another file ...] [--first N] [--last M] [--nr 1,18,74]

WHAT FOR. The knob `talaria.dirtyage=` pays ITS OWN price inside `write`: the pre-emptive
flush is called from there next to the §8.7 threshold (`ext2::dirty_age_flush`). By construction
that price does not enter the price of an `fsync` call, and the knob can therefore be judged
only by a second scoreboard (nine hours of the stand) — see `docs/ENGINEERING-NOTES.md`,
the rule that only points from ONE bring-up may be compared. But the price of a
`write` call is measured in exactly the same way as the price of an `fsync` call —
by the difference of two snapshots of the `stat:` block — and on the stand that instrument turned out to be
accurate to 0.2 %, unlike `p99.9`.

WHAT IS COUNTED. The kernel prints the ten most expensive syscalls: the number, the calls,
the total ms, the ns per call. Cumulatively since the boot, so the workload's START-UP is mixed
into the numbers (the rule of session 37) — only THE DIFFERENCE of the first and the last
snapshot of the window is judged.

THE KERNEL'S "NS PER CALL" COLUMN OVERFLOWS AND MUST NOT BE USED.
`metrics::top_syscalls` computed it as `cycles * 1_000_000_000 / hz / c`, and the
multiplier overflows u64 already at 188 seconds of accumulated time (5.4e14
cycles * 1e9 = 5.4e23 against a ceiling of 1.8e19): in session 41's snapshot there stands "#202: 448
times, 188137 ms, 8190622 ns", where 448 * 8.19 ms = 3669 ms rather than 188137. The instrument
takes ONLY "calls" and "total ms" and divides by itself; with a window of a minute the resolution
of a millisecond gives an accuracy better than 0.1 %.

THE INSTRUMENT'S PITFALL. There are only TEN syscalls in the table, and the composition of the ten changes
between snapshots. A syscall that disappeared from one of the window's two snapshots is honestly
called "not in the top ten" rather than counted as a zero.
"""
import re
import sys

STAMP = re.compile(r"\[\d{4}-\d\d-\d\dT[\d:.]+\]")
SNAP = re.compile(r"stat: --- snapshot at (\d+) ms")
# Two lines of one format: the top ten by time and THE LINE ON REQUEST
# (`talaria.sysrow=`). The second one is the main one: `write`/`pwrite64` do not get into the
# top ten under mongodb until the knob `dirtyage` makes them pay.
ROW = re.compile(r"stat:\s+(?:on request )?#(\d+): (\d+) times, (\d+) ms, (\d+) ns")
NAMES = {0: "read", 1: "write", 3: "close", 7: "poll", 8: "lseek", 9: "mmap", 11: "munmap",
         17: "pread64", 18: "pwrite64", 23: "select", 24: "sched_yield", 35: "nanosleep",
         44: "sendto", 45: "recvfrom", 46: "sendmsg", 47: "recvmsg", 74: "fsync",
         75: "fdatasync", 202: "futex", 232: "epoll_wait", 281: "epoll_pwait"}


def snapshots(path):
    """[(ms since the boot, {number: (calls, total ms)})]"""
    out, cur, rows = [], None, None
    for raw in open(path, "rb").read().decode("utf-8", "replace").splitlines():
        raw = STAMP.sub("", raw)
        m = SNAP.search(raw)
        if m:
            if rows and cur is not None:
                out.append((cur, rows))
            cur, rows = int(m.group(1)), None
            continue
        m = ROW.search(raw)
        if m and cur is not None:
            if rows is None:
                rows = {}
            rows[int(m.group(1))] = (int(m.group(2)), int(m.group(3)))
    if rows and cur is not None:
        out.append((cur, rows))
    return out


def one(path, first, last, want):
    snaps = snapshots(path)
    if len(snaps) < 2:
        print("there are fewer than two snapshots with the syscall table (%d) — there is nothing to make a difference from" % len(snaps))
        print("check that the phase ran with talaria.stat= and talaria.sysprof=1")
        return 2
    a, b = snaps[first], snaps[last]
    print("the window: the snapshot at %d ms -> %d ms (%.0f s), snapshots in the file %d"
          % (a[0], b[0], (b[0] - a[0]) / 1000.0, len(snaps)))
    print("| syscall | calls over the window | us per call | total ms | share of the window |")
    print("|---|---:|---:|---:|---:|")
    win_ms = (b[0] - a[0]) or 1
    rows = []
    for nr in sorted(set(a[1]) | set(b[1])):
        if want and nr not in want:
            continue
        if nr not in a[1] or nr not in b[1]:
            rows.append((0, nr, None))
            continue
        (c0, t0), (c1, t1) = a[1][nr], b[1][nr]
        dc, dt = c1 - c0, t1 - t0       # the calls and the milliseconds over the window
        if dc <= 0:
            continue
        rows.append((dt, nr, (dc, dt * 1000.0 / dc, float(dt))))
    for dt, nr, r in sorted(rows, key=lambda x: -x[0]):
        name = NAMES.get(nr, "?")
        if r is None:
            print("| #%d %s | — | — | — | not in the top ten of one of the snapshots |" % (nr, name))
            continue
        dc, per, ms = r
        print("| #%d %s | %d | %.1f | %.1f | %.1f %% |" % (nr, name, dc, per, ms, 100.0 * ms / win_ms))
    return 0


def main():
    first, last, want = 0, -1, set()
    skip = set()
    for flag in ("--first", "--last", "--nr"):
        if flag in sys.argv:
            i = sys.argv.index(flag)
            skip |= {i, i + 1}
            v = sys.argv[i + 1]
            if flag == "--first":
                first = int(v)
            elif flag == "--last":
                last = int(v)
            else:
                want = {int(x) for x in v.split(",")}
    paths = [a for i, a in enumerate(sys.argv[1:], 1) if i not in skip]
    if not paths:
        print(__doc__)
        return 2
    bad = 0
    for p in paths:
        if len(paths) > 1:
            print("==", p)
        bad |= one(p, first, last, want)
        if len(paths) > 1:
            print()
    return bad


if __name__ == "__main__":
    sys.exit(main())
