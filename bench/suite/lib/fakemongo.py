#!/usr/bin/env python3
"""A toy MongoDB (OP_MSG) for the harness self-check (`selfcheck.sh`).

It understands exactly as much as tlgen sends: OP_MSG (opcode 2013) with one
kind-0 section. To `ping`/`find` it answers `{ok: 1.0}`; on `insert` it looks at
the document's `_id` (int64) and, if that key HAS ALREADY BEEN INSERTED, answers like
a real server — `{ok: 1.0, writeErrors: [...]}` (a write refusal arrives
with ok: 1.0, see proto_mongo.c). This is the gate on the uniqueness of insert keys
BETWEEN the jobs of one `--serve` receiver: a "once per process" nonce gave
every next job the same `_id`s, and both sides of the suite measured the round trip of
a duplicate key (65-75 % errors at every point of ec2all-20260905-205134).

  fakemongo.py --port P        on SIGTERM prints "dups=N" to stderr
"""
import argparse
import selectors
import signal
import socket
import struct
import sys

SEEN = set()
DUPS = 0


def bson_ok():
    # { ok: 1.0 }
    body = b"\x01ok\x00" + struct.pack("<d", 1.0)
    return struct.pack("<i", 4 + len(body) + 1) + body + b"\x00"


def bson_dup():
    # { ok: 1.0, writeErrors: [ { index: 0, code: 11000 } ] }
    inner = b"\x10index\x00" + struct.pack("<i", 0) + b"\x10code\x00" + struct.pack("<i", 11000)
    inner = struct.pack("<i", 4 + len(inner) + 1) + inner + b"\x00"
    arr = b"\x030\x00" + inner
    arr = struct.pack("<i", 4 + len(arr) + 1) + arr + b"\x00"
    body = b"\x01ok\x00" + struct.pack("<d", 1.0) + b"\x04writeErrors\x00" + arr
    return struct.pack("<i", 4 + len(body) + 1) + body + b"\x00"


def reply(req_id, doc):
    total = 16 + 4 + 1 + len(doc)
    return struct.pack("<iiii", total, 1, req_id, 2013) + struct.pack("<i", 0) + b"\x00" + doc


def handle(msg):
    global DUPS
    length, req_id, _resp, opcode = struct.unpack_from("<iiii", msg, 0)
    if opcode != 2013:
        return reply(req_id, bson_ok())
    body = msg[21:]
    if body.find(b"\x02insert\x00") >= 0:
        i = body.find(b"\x12_id\x00")
        if i >= 0:
            key = struct.unpack_from("<q", body, i + 5)[0]
            if key in SEEN:
                DUPS += 1
                return reply(req_id, bson_dup())
            SEEN.add(key)
    return reply(req_id, bson_ok())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    a = ap.parse_args()

    def bye(*_):
        print("dups=%d seen=%d" % (DUPS, len(SEEN)), file=sys.stderr, flush=True)
        sys.exit(0)
    signal.signal(signal.SIGTERM, bye)
    signal.signal(signal.SIGINT, bye)

    ls = socket.socket()
    ls.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    ls.bind(("127.0.0.1", a.port)); ls.listen(64); ls.setblocking(False)
    sel = selectors.DefaultSelector()
    sel.register(ls, selectors.EVENT_READ)
    bufs = {}
    while True:
        for k, _ in sel.select():
            if k.fileobj is ls:
                c, _ = ls.accept(); c.setblocking(False); c.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                sel.register(c, selectors.EVENT_READ); bufs[c] = b""
                continue
            c = k.fileobj
            try:
                d = c.recv(65536)
            except OSError:
                d = b""
            if not d:
                sel.unregister(c); c.close(); bufs.pop(c, None); continue
            buf = bufs[c] + d
            out = b""
            while len(buf) >= 16:
                n = struct.unpack_from("<i", buf, 0)[0]
                if n < 16 or len(buf) < n:
                    break
                out += handle(buf[:n]); buf = buf[n:]
            bufs[c] = buf
            if out:
                try:
                    c.sendall(out)
                except OSError:
                    pass


if __name__ == "__main__":
    main()
