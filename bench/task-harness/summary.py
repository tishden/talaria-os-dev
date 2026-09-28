#!/usr/bin/env python3
"""Harness matrix summary: per (stand, mode) — the median over repeats of p50/p99/p99.9/p99.99 (µs) and drop_rate.
Usage: summary.py OUTDIR [--md]"""
import sys, os, re, glob, statistics
d = sys.argv[1]
rows = {}
for f in sorted(glob.glob(os.path.join(d, '*.report'))):
    name = os.path.basename(f)[:-7]
    m = re.match(r'(\w+?)_(base|nack|fecnack|fec)_r(\d+)$', name)
    if not m: continue
    stand, mode = m.group(1), m.group(2)
    t = open(f).read()
    def g(k):
        mm = re.search(r'^\s*' + re.escape(k) + r'\s*:\s*([0-9.]+)', t, re.M); return float(mm.group(1)) if mm else None
    vals = dict(p50=g('p50'), p99=g('p99'), p999=g('p99.9'), p9999=g('p99.99'), drop=g('drop_rate'))
    if vals['p50'] is None: continue
    rows.setdefault((stand, mode), []).append(vals)
order = {'loopback': 0, 'linux': 1, 'talaria': 2}; morder = {'base': 0, 'nack': 1, 'fec': 2, 'fecnack': 3}
print("| stand | mode | n | p50 µs | p99 | p99.9 | p99.99 | drop % |")
print("|---|---|---|---:|---:|---:|---:|---:|")
for (stand, mode) in sorted(rows, key=lambda k: (order.get(k[0], 9), morder.get(k[1], 9))):
    rs = rows[(stand, mode)]
    med = lambda k: statistics.median([r[k] for r in rs if r[k] is not None])
    print(f"| {stand} | {mode} | {len(rs)} | {med('p50')/1000:.1f} | {med('p99')/1000:.1f} | {med('p999')/1000:.1f} | {med('p9999')/1000:.1f} | {med('drop'):.3f} |")
