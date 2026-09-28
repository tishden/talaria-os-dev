#!/usr/bin/env python3
"""Splicing consecutive snapshots of the EC2 serial console into ONE growing file.

    console-splice.py <accumulator file>        # the snapshot is read from stdin

WHY. `aws ec2 get-console-output --latest` returns not the whole serial but a 64 KB
WINDOW that moves as printing goes on. The diagnostic phase prints a
`stat:` block every twenty seconds, and over a ten-minute run the window moves
several times: a single snapshot at the end (that is how it was taken once —
`console-PA.txt` exactly 64 KB) keeps only the last minutes, while the start of the run
is lost together with the first snapshot, from which the DIFFERENCE of the counters is computed.

So the snapshots are taken periodically and spliced here: neighbouring snapshots
have a common part, and only the tail after it needs appending.

THE SPLICE LOOKS FOR THE LONGEST overlap rather than the first one found: the console is full of
REPEATING blocks (idle `stat:` snapshots with identical counters — an
instrument defect from one session), and a short match here would mean a piece of the log
appended twice.

A TRADE-OFF MADE HERE DELIBERATELY. The overlap is verified in full
(the accumulator's tail must match the snapshot's beginning byte for byte), so the splice
cannot append a piece IN THE WRONG PLACE. But if the console prints
BYTE-IDENTICAL blocks in a row, the longest match may eat
an extra period — and the number of repeats in the accumulator comes out smaller than it was. Losing
a repeat of identical content is no loss (the counters are read as the DIFFERENCE of the first and the last
snapshot, and identical blocks by definition change nothing), while gluing something foreign
into the middle of the log would ruin the analysis. Hence this order, and the gate in
`selfcheck.sh` checks both sides of the trade-off.

If there is no overlap at all — the window moved ENTIRELY between two snapshots, and part
of the console is lost for good. That is not hidden: an explicit GAP mark goes into
the file, and a line for the phase log goes to stderr (the polling period must be reduced).
"""
import os
import sys

PROBE = 256  # the length of the hook by which the overlap is searched for


def overlap(acc: bytes, snap: bytes) -> int:
    """The length of the common end of `acc` and beginning of `snap` (0 — nothing in common)."""
    if not acc or not snap:
        return 0
    # There can be no overlap beyond the end of the window — we search only in the tail.
    tail = acc[-len(snap):]
    probe = snap[:PROBE]
    pos = tail.find(probe)
    while pos != -1:
        k = len(tail) - pos
        if k <= len(snap) and tail[pos:] == snap[:k]:
            return k          # the first match in the tail = THE LONGEST
        pos = tail.find(probe, pos + 1)
    # The hook is shorter than PROBE (a very small snapshot) — we take it head-on.
    if len(snap) < PROBE:
        for k in range(min(len(acc), len(snap)), 0, -1):
            if acc[-k:] == snap[:k]:
                return k
    return 0


def splice(path: str, snap: bytes) -> str:
    acc = b""
    if os.path.exists(path):
        with open(path, "rb") as f:
            acc = f.read()
    if not snap.strip():
        return "the snapshot is empty — nothing appended"
    if not acc:
        with open(path, "wb") as f:
            f.write(snap)
        return "the first snapshot: %d B" % len(snap)
    k = overlap(acc, snap)
    if k == 0:
        gap = "\n### CONSOLE GAP: the 64 KB window moved between snapshots, part of the log is lost\n".encode()
        with open(path, "ab") as f:
            f.write(gap + snap)
        return "GAP: no overlap, appended %d B (poll more often!)" % len(snap)
    add = snap[k:]
    if add:
        with open(path, "ab") as f:
            f.write(add)
    return "overlap %d B, appended %d B" % (k, len(add))


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(__doc__)
        sys.exit(2)
    print(splice(sys.argv[1], sys.stdin.buffer.read()), file=sys.stderr)
