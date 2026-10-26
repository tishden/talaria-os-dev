#!/usr/bin/env python3
"""The Linux server's PMU per request (ARM64-PLAN §12 item 3).

    pmureq.py <run directory> [case]

Reads the `during` snapshot of every metrics-linux-<case>-r<rate>-<n>.txt (run-remote.sh with
DURING=1; statd's `perfall` block: perf stat -a, one second a set) and divides each count by the
rung's rate — the open loop holds the rate, so a second's count / rate is the price of one
request on the whole machine. Talaria's side of the same table is talaria.pmuel0=1 /
talaria.pmuev=<event> in the stat block.
"""
import glob, os, re, sys

COLS = [("cycles:u", "cyc u"), ("instructions:u", "ins u"), ("r2D:u", "l2tlb u"),
        ("r02:u", "l1itlb u"), ("r05:u", "l1dtlb u"), ("cycles:k", "cyc k"),
        ("instructions:k", "ins k"), ("r2D:k", "l2tlb k"), ("r02:k", "l1itlb k"),
        ("r05:k", "l1dtlb k"), ("raw_syscalls:sys_enter", "sysc"), ("context-switches", "csw")]


def during(path):
    """{event: count} of the file's `during` snapshot (perf's -x, lines)."""
    out, on = {}, False
    for line in open(path, errors="replace"):
        m = re.match(r"### snapshot \S+ (before|during|after) ", line)
        if m:            # metrics-remote.sh's header; statd's own "### snapshot linux <date>" is inside
            on = m.group(1) == "during"
        elif on and line.startswith("perfall "):
            f = line[8:].strip().split(",")
            if len(f) >= 3 and re.fullmatch(r"[0-9.]+", f[0]):
                out[f[2]] = float(f[0])
    return out


def main():
    run = sys.argv[1]
    case = sys.argv[2] if len(sys.argv) > 2 else "*"
    rows = []
    for p in glob.glob(os.path.join(run, "**", "metrics-linux-%s-r*-*.txt" % case), recursive=True):
        m = re.search(r"metrics-linux-(.+)-r(\d+)-(\d+)\.txt$", p)
        ev = during(p)
        if not m or not ev:
            continue
        rows.append((m.group(1), int(m.group(2)), int(m.group(3)), ev))
    if not rows:
        print("no `during` snapshots (run-remote.sh DURING=1)")
        return
    print("%-8s %6s %2s " % ("case", "rate", "n") + " ".join("%9s" % c[1] for c in COLS) + "  ipc u  ipc k")
    for c, rate, n, ev in sorted(rows):
        per = [ev.get(k) for k, _ in COLS]
        cell = ["%9s" % ("-" if v is None else "%.0f" % (v / rate)) for v in per]
        ipc = lambda i, c_: ("%.2f" % (ev[i] / ev[c_])) if ev.get(i) and ev.get(c_) else "-"
        print("%-8s %6d %2d " % (c, rate, n) + " ".join(cell)
              + "  %5s  %5s" % (ipc("instructions:u", "cycles:u"), ipc("instructions:k", "cycles:k")))


if __name__ == "__main__":
    main()
