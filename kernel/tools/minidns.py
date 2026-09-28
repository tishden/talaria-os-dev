#!/usr/bin/env python3
"""Mini DNS for the Talaria regression: answers any name with an A record (default 10.0.0.1).
Usage: minidns.py <bind_ip> [answer_ip] [port]. UDP only, no dependencies."""
import socket, struct, sys
bind_ip = sys.argv[1] if len(sys.argv) > 1 else "10.0.0.1"
answer = sys.argv[2] if len(sys.argv) > 2 else "10.0.0.1"
port = int(sys.argv[3]) if len(sys.argv) > 3 else 53
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind((bind_ip, port))
while True:
    data, peer = s.recvfrom(512)
    print("query from", peer, "len", len(data), flush=True)
    if len(data) < 12: continue
    tid = data[:2]
    flags = struct.pack(">H", 0x8180)
    qd = data[4:6]
    # question: the name up to 0x00 + qtype/qclass
    i = 12
    while i < len(data) and data[i] != 0: i += data[i] + 1
    q_end = i + 5
    question = data[12:q_end]
    qtype = data[i+1:i+3]
    if qtype == b"\x00\x01":  # A
        ans = b"\xc0\x0c" + b"\x00\x01\x00\x01" + struct.pack(">IH", 30, 4) + socket.inet_aton(answer)
        resp = tid + flags + qd + b"\x00\x01" + b"\x00\x00\x00\x00" + question + ans
    else:  # no answer (AAAA etc.) — NOERROR, 0 answers
        resp = tid + flags + qd + b"\x00\x00" + b"\x00\x00\x00\x00" + question
    try: s.sendto(resp, peer)
    except Exception as e: print("send error", e, flush=True)
