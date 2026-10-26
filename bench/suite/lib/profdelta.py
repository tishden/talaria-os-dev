#!/usr/bin/env python3
"""THE TICK'S PROFILE PER STAT BLOCK: where the cores were at each rung, not over the run.

    profdelta.py <console file> [--kernel <ELF>] [--nm <nm>] [--top N]

WHY. `talaria.prof=1` samples every tick into two tables (the kernel's code, the pages of
user code) and the stat block prints them CUMULATIVE since boot. Over a ladder that is the
average of the idle low rungs and the loaded top ones, and it reads backwards: PostgreSQL's
pk over a whole run was 27 % ring 3 and 46 % the network waiter's spin, while at the top
rung it was 76 % ring 3 and 6 % spin — with 13 % of the cores asleep, i.e. a ceiling bound
by the round trip, not by the processor (MEASUREMENTS §10.19). The difference between two
consecutive blocks is the profile of those ten seconds.

WHAT IS PRINTED, per block (its snapshot time, seconds): the share of ring 3, and of each
kernel address the table holds, named by the kernel's symbols when --kernel is given
(the nearest symbol at or below, demangled when `rustfilt` is on the PATH). Ring 3 is printed
WITHOUT the samples that landed on a syscall's return address — a tick held off by the kernel's
masked interrupts is taken right after the `eret`, and those are the kernel's time, shown as
`masked-syscall-tail` by syscall number (a kernel that prints that line; MEASUREMENTS §10.19).
"""
import re, shutil, subprocess, sys


def symbols(elf, nm):
    out = subprocess.run([nm, '-n', elf], capture_output=True, text=True, check=True).stdout
    syms = []
    for line in out.splitlines():
        p = line.split()
        if len(p) >= 3 and p[1] in 'tTwW':
            syms.append((int(p[0], 16), p[2]))
    return syms


def name_of(addr, syms, cache):
    if addr in cache:
        return cache[addr]
    lo, hi, best = 0, len(syms) - 1, None
    while lo <= hi:
        mid = (lo + hi) // 2
        if syms[mid][0] <= addr:
            best = syms[mid][1]; lo = mid + 1
        else:
            hi = mid - 1
    cache[addr] = best or hex(addr)
    return cache[addr]


def short_v0(n):
    """`_RNvNtCs..._7talaria6netsys12yield_window` -> `netsys::yield_window` (no rustfilt)."""
    if not n.startswith('_R'):
        return n
    ids, i = [], 2
    while i < len(n):
        m = re.match(r'(\d+)', n[i:])
        if not m:
            i += 1; continue
        k = int(m.group(1)); j = i + len(m.group(1))
        if k == 0 or j + k > len(n):
            i = j; continue
        ids.append(n[j:j + k]); i = j + k
    ids = [x for x in ids if re.match(r'[A-Za-z_]\w*$', x)]
    return '::'.join(ids[-2:]) if ids else n


def demangle(names):
    if not shutil.which('rustfilt'):
        return {n: short_v0(n) for n in names}
    out = subprocess.run(['rustfilt'], input='\n'.join(names), capture_output=True, text=True).stdout.split('\n')
    return dict(zip(names, out))


def main():
    a = sys.argv[1:]
    if not a or a[0].startswith('-'):
        print(__doc__); sys.exit(2)
    con = a[0]
    elf = a[a.index('--kernel') + 1] if '--kernel' in a else None
    nm = a[a.index('--nm') + 1] if '--nm' in a else 'nm'
    top = int(a[a.index('--top') + 1]) if '--top' in a else 4
    blocks, cur, mode = [], None, None
    for l in open(con, errors='replace'):
        m = re.search(r'--- snapshot at (\d+) ms', l)
        if m:
            cur = {'t': int(m.group(1)), 'k': {}, 'u': 0, 'kt': 0, 'st': 0, 'sn': {}, 'ph': {}}; blocks.append(cur); mode = None; continue
        if cur is None:
            continue
        m = re.search(r"profile by the KERNEL's code \((\d+) samples", l)
        if m:
            cur['kt'] = int(m.group(1)); mode = 'k'; continue
        m = re.search(r'profile by code \((\d+) samples', l)
        if m:
            cur['u'] = int(m.group(1)); mode = None; continue
        m = re.search(r'the waits by phase, us:(.*?) \(1 the entry', l)
        if m:
            cur['ph'] = {int(a): int(b) for a, b in re.findall(r'(\d+)=(\d+)', m.group(1))}
            continue
        m = re.search(r"on a syscall's return address (\d+) .*by syscall:(.*)", l)
        if m:
            cur['st'] = int(m.group(1))
            cur['sn'] = {int(a): int(b) for a, b in re.findall(r'#(\d+) (\d+)', m.group(2)) if int(b)}
            continue
        m = re.match(r'stat:\s+0x([0-9a-f]+): (\d+)', l)
        if m and mode == 'k':
            cur['k'][int(m.group(1), 16)] = int(m.group(2))
    syms = symbols(elf, nm) if elf else []
    cache = {}
    label = (lambda x: name_of(x, syms, cache)) if syms else hex
    rows, seen, prev = [], set(), None
    for b in blocks:
        # a snapshot spliced twice from the console windows appears twice: keep the first
        if b['kt'] == 0 or b['t'] in seen:
            continue
        seen.add(b['t'])
        if prev:
            du, dk = b['u'] - prev['u'], b['kt'] - prev['kt']
            tot = du + dk
            if tot > 0:
                agg = {}
                for x, v in b['k'].items():
                    n = label(x); agg[n] = agg.get(n, 0) + v - prev['k'].get(x, 0)
                ds = b['st'] - prev['st']
                sn = {k: v - prev['sn'].get(k, 0) for k, v in b['sn'].items()}
                dt = max(b['t'] - prev['t'], 1) * 1000  # us of wall time
                ph = {k: (v - prev['ph'].get(k, 0)) / dt for k, v in b['ph'].items()}
                rows.append((b['t'] // 1000, du, tot, agg, dk - sum(agg.values()), ds, sn, ph))
        prev = b
    dm = demangle(sorted({n for r in rows for n in r[3]}))
    PH = {1: 'entry', 2: 'top', 3: 'poll', 4: 'pass', 5: 'lock', 6: 'scan', 7: 'window', 8: 'spin', 9: 'yield',
          10: 'back', 11: 'nic', 12: 'parked', 13: 'exit', 14: 'read', 15: 'write'}
    for t, du, tot, agg, rest, ds, sn, ph in rows:
        parts = sorted(agg.items(), key=lambda kv: -kv[1])[:top]
        s = '  '.join('%s %.0f%%' % (dm.get(n, n), 100 * v / tot) for n, v in parts)
        # ring-3 samples on a syscall's return address are the kernel's masked time (sched::PROF_SYSTAIL)
        tail = ''
        if ds:
            by = ' '.join('#%d %.0f%%' % (k, 100 * v / tot) for k, v in sorted(sn.items(), key=lambda kv: -kv[1]) if v > 0)
            tail = '  masked-syscall-tail %.0f%% (%s)' % (100 * ds / tot, by)
        # the waits' time by phase in cores (summed over the tasks; "exit" runs on into ring 3)
        wp = '  waits(cores): ' + ' '.join('%s %.2f' % (PH.get(k, k), v) for k, v in sorted(ph.items(), key=lambda kv: -kv[1])[:6]) if ph else ''
        print('%5d s  samples %8d  ring3 %3.0f%%%s  %s  kernel-other %.0f%%%s' % (t, tot, 100 * (du - ds) / tot, tail, s, 100 * rest / tot, wp))


if __name__ == '__main__':
    main()
