#!/usr/bin/env python3
"""A minimal PostgreSQL client (protocol v3, the simple query, trust authentication, no
dependencies): probes the server on Talaria where the static psql of the x86 build does not
run (the arm64 gate, the arm64 KVM stand).
  pgprobe.py HOST PORT "stmt1; stmt2; ..."   (user postgres, database postgres)
Prints each result row as `col|col`, each command tag, and `pgprobe: ERROR ...` on an error;
the exit code is 0 only when every statement succeeded.
"""
import os, socket, struct, sys


def recv(s, n):
    out = b''
    while len(out) < n:
        c = s.recv(n - len(out))
        if not c:
            raise IOError('connection closed')
        out += c
    return out


def msg(s):
    t = recv(s, 1)
    n = struct.unpack('!I', recv(s, 4))[0]
    return t, recv(s, n - 4)


def fields(body):  # an ErrorResponse's fields -> {code: text}
    out = {}
    for part in body.split(b'\0'):
        if part:
            out[chr(part[0])] = part[1:].decode(errors='replace')
    return out


def main():
    host, port, sql = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    # PGPROBE_TIMEOUT: under TCG every idle PostgreSQL process spins its wait (a set with a
    # signalfd is not parked), and a statement's first catalog work takes tens of seconds
    s = socket.create_connection((host, port), timeout=float(os.environ.get('PGPROBE_TIMEOUT', '20')))
    params = b'user\0postgres\0database\0postgres\0\0'
    s.sendall(struct.pack('!II', 8 + len(params), 196608) + params)
    while True:
        t, b = msg(s)
        if t == b'R' and struct.unpack('!I', b[:4])[0] != 0:
            print('pgprobe: ERROR the server wants a password'); return 1
        if t == b'E':
            print('pgprobe: ERROR', fields(b).get('M')); return 1
        if t == b'Z':
            break
    s.sendall(b'Q' + struct.pack('!I', 4 + len(sql) + 1) + sql.encode() + b'\0')
    rc = 0
    while True:
        t, b = msg(s)
        if t == b'D':
            n = struct.unpack('!H', b[:2])[0]; i = 2; vals = []
            for _ in range(n):
                ln = struct.unpack('!i', b[i:i + 4])[0]; i += 4
                if ln < 0:
                    vals.append('NULL')
                else:
                    vals.append(b[i:i + ln].decode(errors='replace')); i += ln
            print('|'.join(vals))
        elif t == b'C':
            print(b.rstrip(b'\0').decode())
        elif t == b'E':
            print('pgprobe: ERROR', fields(b).get('M')); rc = 1
        elif t == b'Z':
            break
    s.sendall(b'X\0\0\0\4')
    return rc


if __name__ == '__main__':
    try:
        sys.exit(main())
    except Exception as e:  # a refused or reset connection while the server is starting
        print('pgprobe: ERROR', e); sys.exit(2)
