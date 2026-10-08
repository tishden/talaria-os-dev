import socket, sys, time
s = socket.socket(socket.AF_UNIX)
s.connect("/tmp/lg.sock")
s.settimeout(0.5)
out = open(sys.argv[1] if len(sys.argv) > 1 else "/s/gp-perf.log", "wb")


def drain(t):
    end = time.time() + t
    while time.time() < end:
        try:
            d = s.recv(65536)
            if d:
                out.write(d)
        except socket.timeout:
            pass


drain(1)
import os
for cmd in [c.encode() + b"\r" for c in os.environ.get("LGCMDS", "perf top;perf record 4000").split(";")]:
    for ch in cmd:
        s.send(bytes([ch]))
        time.sleep(0.01)
    drain(20)
out.close()
