#!/usr/bin/env python3
"""A toy RESP server for the harness self-check (`selfcheck.sh`).

It answers `+PONG`/`+OK`/`$-1` to PING/SET/GET — exactly enough for tlgen
to make a full run against it on the host, without Redis and without a guest. This is
not a measurement: its numbers mean nothing, but the generator's report is real,
and it can be checked for parseability, for `counts.sent`, for the agreement of
`open_loop` with `service` and for behaviour on a disconnect.

  fakeresp.py --port P [--die-after N]   after N answers, close ALL
                                          connections and exit (a connection drop)
"""
import argparse
import selectors
import socket
import sys


def parse_one(buf, pos):
    """One RESP2 value -> (end, arguments), or None if incomplete."""
    if pos >= len(buf):
        return None
    c = buf[pos:pos + 1]
    nl = buf.find(b"\r\n", pos)
    if nl < 0:
        return None
    if c in (b"+", b"-", b":"):
        return nl + 2, [buf[pos + 1:nl]]
    if c == b"$":
        n = int(buf[pos + 1:nl])
        if n < 0:
            return nl + 2, [None]
        end = nl + 2 + n + 2
        if len(buf) < end:
            return None
        return end, [buf[nl + 2:nl + 2 + n]]
    if c == b"*":
        n = int(buf[pos + 1:nl])
        p, args = nl + 2, []
        for _ in range(n):
            r = parse_one(buf, p)
            if r is None:
                return None
            p, a = r
            args += a
        return p, args
    raise ValueError("garbage in RESP: %r" % c)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--die-after", type=int, default=0)
    a = ap.parse_args()
    ls = socket.socket()
    ls.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    ls.bind(("127.0.0.1", a.port))
    ls.listen(64)
    ls.setblocking(False)
    sel = selectors.DefaultSelector()
    sel.register(ls, selectors.EVENT_READ)
    bufs, answered = {}, 0
    print("fakeresp: listening on 127.0.0.1:%d" % a.port, flush=True)
    while True:
        for key, _ in sel.select():
            s = key.fileobj
            if s is ls:
                c, _addr = ls.accept()
                c.setblocking(False)
                c.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                sel.register(c, selectors.EVENT_READ)
                bufs[c] = b""
                continue
            try:
                d = s.recv(65536)
            except OSError:
                d = b""
            if not d:
                sel.unregister(s); s.close(); bufs.pop(s, None)
                continue
            buf = bufs[s] + d
            out = b""
            pos = 0
            while True:
                r = parse_one(buf, pos)
                if r is None:
                    break
                pos, args = r
                cmd = (args[0] or b"").upper()
                if cmd == b"PING":
                    out += b"+PONG\r\n"
                elif cmd == b"GET":
                    out += b"$-1\r\n"
                elif cmd == b"INCR":
                    out += b":1\r\n"
                else:
                    out += b"+OK\r\n"
                answered += 1
            bufs[s] = buf[pos:]
            if out:
                try:
                    s.sendall(out)
                except OSError:
                    pass
            if a.die_after and answered >= a.die_after:
                print("fakeresp: %d answers — dropping all connections" % answered, flush=True)
                for c in list(bufs):
                    try:
                        c.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, b"\x01\x00\x00\x00\x00\x00\x00\x00")
                        c.close()
                    except OSError:
                        pass
                ls.close()
                return 0


if __name__ == "__main__":
    sys.exit(main())
