#!/usr/bin/env python3
"""A minimal SNTP server for the gates: talaria.ntp= against a clock whose offset and rate
are KNOWN, so the kernel's servo can be judged by numbers instead of by "it printed a line".

    minintp.py <bind ip> [offset s] [skew ppm]

The served clock is the host's time.time() moved by `offset` seconds and running `skew` ppm
fast from the server's start: the kernel must step once onto the offset and then settle its
correction at -skew (its nanoseconds lengthen when the reference runs fast). Each reply is
logged as one line on stderr.
"""
import socket, struct, sys, time

NTP_DELTA = 2208988800
bind = sys.argv[1] if len(sys.argv) > 1 else "0.0.0.0"
offset = float(sys.argv[2]) if len(sys.argv) > 2 else 0.0
skew = float(sys.argv[3]) if len(sys.argv) > 3 else 0.0
t0 = time.time()


def served():
    now = time.time()
    return now + offset + (now - t0) * skew * 1e-6


def ntp(ts):
    sec = int(ts) + NTP_DELTA
    return (sec << 32) | int((ts - int(ts)) * (1 << 32))


s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind((bind, 123))
print(f"minintp: {bind}:123 offset {offset:+} s skew {skew:+} ppm", file=sys.stderr, flush=True)
while True:
    p, peer = s.recvfrom(512)
    t2 = served()
    if len(p) < 48 or p[0] & 7 != 3:
        continue
    origin = p[40:48]
    t3 = served()
    reply = struct.pack("!BBbbII4s", (0 << 6) | (4 << 3) | 4, 2, 4, -20, 0, 0, b"GATE")
    reply += struct.pack("!Q", ntp(t3)) + origin + struct.pack("!QQ", ntp(t2), ntp(t3))
    s.sendto(reply, peer)
    print(f"minintp: {peer[0]} t2={t2:.6f}", file=sys.stderr, flush=True)
