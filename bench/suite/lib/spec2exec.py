#!/usr/bin/env python3
"""From the guest description (workloads/<workload>/spec) — the start command for the Linux stand.

One description for both paths: the local Linux guest executes the spec with its own init,
and on EC2 a systemd unit is built from the very same file. Two separate start
descriptions diverge sooner or later, and the comparison starts measuring the difference between them.

Prints one line: <command>|<uid>|<gid>|<directories to chown, comma-separated>
"""
import sys

ex, args, uid, gid, chown = "", [], "0", "0", []
for line in open(sys.argv[1]):
    line = line.strip()
    if not line or line.startswith("#") or "=" not in line:
        continue
    k, v = line.split("=", 1)
    if k == "exec":
        ex = v
    elif k == "arg":
        args.append(v)
    elif k == "uid":
        uid = v
    elif k == "gid":
        gid = v
    elif k == "chown":
        chown.append(v.split(":")[0])

# argv[0] in the spec is the program name; systemd substitutes it from the path itself.
print("%s|%s|%s|%s" % (" ".join([ex] + args[1:]), uid, gid, ",".join(chown)))
