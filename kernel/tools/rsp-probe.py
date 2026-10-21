#!/usr/bin/env python3
"""A scripted gdb client for the gates: speak the GDB Remote Serial Protocol to a stub
over TCP and print each answer, one per line, as `<packet> -> <answer>`.

  rsp-probe.py PORT [PACKET...]

The first line is the answer to `?` (`first: T05...`), asked until the stub stands. Special
words among the packets: `c!` sends a continue and does not wait for a stop (the machine
runs), `INTR` sends gdb's interrupt (^C, 0x03) and waits for the stop it causes, `SLEEP`
waits 1.5 s. With
TAIL=<seconds> in the environment, what the line carries after the last packet is printed
too (`after: ...`) — on arm64 the stub shares the console's line, so that is the console.
Bytes outside a `$...#xx` are skipped, as gdb skips them.
"""
import os
import re
import socket
import sys
import time

port = int(sys.argv[1])
packets = sys.argv[2:]
s = None
for _ in range(100):
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=30)
        break
    except OSError:
        time.sleep(0.3)
if s is None:
    sys.exit("rsp-probe: nothing listens on :%d" % port)
buf = b""


def pkt():
    global buf
    while True:
        m = re.search(rb"\$([^#$]*)#..", buf)
        if m:
            buf = buf[m.end():]
            s.sendall(b"+")
            return m.group(1).decode(errors="replace")
        d = s.recv(4096)
        if not d:
            sys.exit("rsp-probe: the line closed")
        buf += d


def cmd(c):
    s.sendall(("$%s#%02x" % (c, sum(c.encode()) % 256)).encode())
    return pkt()


# The stub answers `?` once it stands (it does not speak first: gdb opens with qSupported,
# and a stop reply crossing it would be taken for the answer). Until the kernel reaches the
# stub the bytes may be eaten by the console, so `?` is repeated, as gdb repeats a packet.
# A reset here is the stand's, not the stub's: docker's port proxy accepts the connection
# and RESETS it when QEMU does not listen yet (under a loaded host the 3 s the gate waits
# were not enough — ten FAILs of one block at the first packet). Reconnect and ask again.
s.settimeout(2)
resets = 0
while True:
    try:
        print("first: " + cmd("?"))
        break
    except ConnectionResetError:
        resets += 1
        if resets > 60:
            sys.exit("rsp-probe: the line keeps resetting on :%d" % port)
        time.sleep(0.5)
        buf = b""
        s = socket.create_connection(("127.0.0.1", port), timeout=30)
        s.settimeout(2)
    except socket.timeout:
        pass
# A repeated `?` the stub found queued is answered twice: the spare answers go, or they
# would be read as the replies to the packets below.
s.settimeout(0.5)
try:
    while True:
        pkt()
except socket.timeout:
    pass
s.settimeout(30)
for c in packets:
    if c == "SLEEP":
        time.sleep(1.5)
        continue
    if c == "c!":
        s.sendall(b"$c#63")
        time.sleep(1.5)
        print("c! -> (running)")
        continue
    r = (s.sendall(b"\x03") or pkt()) if c == "INTR" else cmd(c)
    print("%s -> %s" % (c, r))
tail = float(os.environ.get("TAIL", "0"))
if tail:
    s.settimeout(0.5)
    end = time.time() + tail
    out = buf
    while time.time() < end:
        try:
            d = s.recv(4096)
            if not d:
                break
            out += d
        except socket.timeout:
            pass
    print("after: " + out.decode(errors="replace").replace("\r", "").replace("\n", " | ")[-800:])
