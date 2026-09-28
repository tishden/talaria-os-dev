#!/usr/bin/env python3
"""Talking to the Talaria console over a unix socket (the VM's serial).

The Talaria console works on top of a running workload — from the scheduler tick —
so the counters are taken from a LIVE system without stopping it
(docs/PERFORMANCE-OPTIMIZATION-GUIDE.md §10). This is only the transport: send a
command, wait for silence, return the output.

    talaria-console.py <socket> <command> [another command...]
"""
import socket
import sys
import time


# The "the command is done" pause and the waiting ceiling: a snapshot is taken between
# the ladder's runs, and half a minute per snapshot is half an hour of workload time.
def talk(sock_path, cmds, quiet=0.25, budget=4.0):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(2.0)
    s.connect(sock_path)
    out = []
    # An empty line wakes the prompt and also flushes any partial input.
    for cmd in ["", *cmds]:
        s.sendall((cmd + "\n").encode())
        buf = b""
        last = time.time()
        t0 = time.time()
        s.settimeout(0.2)
        while time.time() - last < quiet and time.time() - t0 < budget:
            try:
                chunk = s.recv(65536)
                if not chunk:
                    break
                buf += chunk
                last = time.time()
            except socket.timeout:
                pass
        if cmd:
            out.append((cmd, buf.decode("utf-8", "replace")))
    s.close()
    return out


if __name__ == "__main__":
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    for cmd, text in talk(sys.argv[1], sys.argv[2:]):
        print("=== %s ===" % cmd)
        print(text.replace("\r", ""))
