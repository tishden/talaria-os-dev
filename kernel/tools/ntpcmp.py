#!/usr/bin/env python3
"""The gates' judge of talaria.ntp=: how far the guest's wall clock is from the SNTP server
it follows, measured from the outside.

    ntpcmp.py <guest ip> <kecho port> <server ip> [rounds]

The guest runs the kernel echo with talaria.kts=1, which answers with its wall clock in the
first 8 bytes; the server (minintp.py) answers SNTP. Each round asks both and refers each
answer to the host's clock at its round trip's midpoint; the difference of the two is the
guest's error against the reference, free of the host's own offset. Prints the median in us
and the shortest round trips seen; exits 1 if a side never answered.
"""
import socket, struct, sys, time

guest, port, server = sys.argv[1], int(sys.argv[2]), sys.argv[3]
rounds = int(sys.argv[4]) if len(sys.argv) > 4 else 9
NTP_DELTA = 2208988800


def ask(sock, addr, payload, parse):
    a = time.time()
    sock.sendto(payload, addr)
    d, _ = sock.recvfrom(128)
    b = time.time()
    return parse(d) - (a + b) / 2, b - a


def kecho(d):
    return struct.unpack("<Q", d[:8])[0] / 1e9


def sntp(d):
    ts = struct.unpack("!Q", d[40:48])[0]
    return (ts >> 32) - NTP_DELTA + (ts & 0xFFFFFFFF) / 2**32


s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.settimeout(3)
diffs, rk, rs = [], [], []
for _ in range(rounds):
    try:
        k, tk = ask(s, (guest, port), b"\0" * 16, kecho)
        n, tn = ask(s, (server, 123), bytes([0x23]) + b"\0" * 47, sntp)
    except socket.timeout:
        continue
    diffs.append(k - n); rk.append(tk); rs.append(tn)
    time.sleep(0.05)
if not diffs:
    print("ntpcmp: no answer"); sys.exit(1)
diffs.sort()
med = diffs[len(diffs) // 2]
print("ntpcmp: the guest - the reference = %+d us (median of %d), round trips %d/%d us"
      % (round(med * 1e6), len(diffs), round(min(rk) * 1e6), round(min(rs) * 1e6)))
