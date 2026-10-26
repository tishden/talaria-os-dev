#!/usr/bin/env python3
"""Talaria's server PMU per request, from the stat blocks of a captured console
(ARM64-PLAN §12 item 3; the Linux side is pmureq.py).

    talpmu.py <console-P<n>.txt> [min req/s]

Every pair of neighbouring blocks (talaria.stat=<ms>) gives the cores' cycles / instructions /
the LLC-miss slot's event (talaria.pmuel0=1 — EL0 alone, talaria.pmuev=<event> — another event
in the slot) over the requests the server answered between them ("the server's turnaround on a
socket ... N"). A pair is printed when it held at least [min] req/s (default 1000); its rate
names the rung.
"""
import re, sys

def blocks(path):
    cur = None
    for line in open(path, errors="replace"):
        m = re.search(r"--- snapshot at (\d+) ms ---", line)
        if m:
            if cur and "req" in cur and cur["cyc"]:
                yield cur
            cur = {"ms": int(m.group(1)), "cyc": {}, "ins": {}, "ev": {}}
            continue
        if cur is None:
            continue
        m = re.search(r"stat: core (\d+): cycles (\d+) instructions (\d+) IPC \S+ LLC misses (\d+)", line)
        if m:
            c = int(m.group(1))
            cur["cyc"][c], cur["ins"][c], cur["ev"][c] = int(m.group(2)), int(m.group(3)), int(m.group(4))
            continue
        m = re.search(r"the server's turnaround on a socket \(.*?\) (\d+)", line)
        if m:
            cur["req"] = int(m.group(1))
    if cur and "req" in cur and cur["cyc"]:
        yield cur

def main():
    path = sys.argv[1]
    floor = float(sys.argv[2]) if len(sys.argv) > 2 else 1000
    seen, bs = set(), []
    for b in blocks(path):              # the console is captured with overlaps: a block comes twice
        if b["ms"] not in seen:
            seen.add(b["ms"]); bs.append(b)
    bs.sort(key=lambda b: b["ms"])
    print("%9s %8s %8s %9s %9s %7s %5s" % ("at, s", "req/s", "req", "cyc/req", "ins/req", "ev/req", "IPC"))
    for a, b in zip(bs, bs[1:]):
        dt = (b["ms"] - a["ms"]) / 1000.0
        dr = b["req"] - a["req"]
        if dt <= 0 or dr <= 0 or dr / dt < floor or set(a["cyc"]) != set(b["cyc"]):
            continue
        d = lambda k: sum(b[k][c] - a[k][c] for c in b[k])
        cy, ins, ev = d("cyc"), d("ins"), d("ev")
        if cy < 0:
            continue                    # a counter restarted between the blocks
        print("%9.0f %8.0f %8d %9.0f %9.0f %7.0f %5.2f" % (b["ms"] / 1000.0, dr / dt, dr, cy / dr, ins / dr, ev / dr, ins / cy if cy else 0))

if __name__ == "__main__":
    main()
