#!/usr/bin/env python3
"""A minimal MySQL/MariaDB protocol client (text protocol, no dependencies): probes the
server on Talaria from the qemu container (a fallback when the static mariadb client is not built).
  mysqlprobe.py HOST PORT "stmt1; stmt2; ..."   (user root, no password)
"""
import os, socket, struct, sys

def lenenc(b, i):
    c = b[i]
    if c < 0xfb: return c, i + 1
    if c == 0xfc: return struct.unpack_from('<H', b, i + 1)[0], i + 3
    if c == 0xfd: return int.from_bytes(b[i+1:i+4], 'little'), i + 4
    return struct.unpack_from('<Q', b, i + 1)[0], i + 9

def lenenc_str(b, i):
    n, i = lenenc(b, i)
    return b[i:i+n], i + n

class Conn:
    def __init__(self, host, port):
        self.s = socket.create_connection((host, port), timeout=float(os.environ.get('MYSQLPROBE_TIMEOUT', '10')))  # the env: a TCG guest is slow
        self.seq = 0
    def read_packet(self):
        hdr = self._recv(4)
        n = int.from_bytes(hdr[:3], 'little'); self.seq = hdr[3] + 1
        return self._recv(n)
    def _recv(self, n):
        out = b''
        while len(out) < n:
            c = self.s.recv(n - len(out))
            if not c: raise IOError('connection closed')
            out += c
        return out
    def send_packet(self, payload):
        self.s.sendall(len(payload).to_bytes(3, 'little') + bytes([self.seq]) + payload); self.seq += 1
    def handshake(self, user='root'):
        p = self.read_packet()
        if p[0] == 0xff: raise IOError('server error: ' + p[3:].decode(errors='replace'))
        i = 1; j = p.index(b'\0', i); self.server_version = p[i:j].decode(); i = j + 1
        i += 4  # connection id
        i += 8 + 1  # auth-plugin-data part1 + filler
        caps = struct.unpack_from('<H', p, i)[0]; i += 2
        i += 1 + 2  # charset, status
        caps |= struct.unpack_from('<H', p, i)[0] << 16; i += 2
        CL_PROTOCOL_41 = 0x200; CL_SECURE = 0x8000; CL_PLUGIN_AUTH = 0x80000; CL_LONG_PASSWORD = 1
        my = CL_LONG_PASSWORD | CL_PROTOCOL_41 | CL_SECURE | CL_PLUGIN_AUTH
        resp = struct.pack('<IIB', my, 16 * 1024 * 1024, 33) + b'\0' * 23 + user.encode() + b'\0' + b'\0' + b'mysql_native_password\0'
        self.send_packet(resp)
        r = self.read_packet()
        while r[0] == 0xfe:  # AuthSwitchRequest -> empty answer (no password)
            self.send_packet(b'')
            r = self.read_packet()
        if r[0] == 0xff: raise IOError('auth error: ' + r[3:].decode(errors='replace'))
    def query(self, sql):
        self.seq = 0
        self.send_packet(b'\x03' + sql.encode())
        p = self.read_packet()
        if p[0] == 0x00:
            aff, i = lenenc(p, 1); last, i = lenenc(p, i)
            return ('OK', f'affected={aff} last_insert_id={last}')
        if p[0] == 0xff:
            code = struct.unpack_from('<H', p, 1)[0]
            return ('ERR', f'{code} {p[9:].decode(errors="replace")}')
        ncol, _ = lenenc(p, 0)
        cols = []
        for _ in range(ncol):
            c = self.read_packet(); i = 0
            for k in range(4): _, i = lenenc_str(c, i)
            name, i = lenenc_str(c, i)
            cols.append(name.decode(errors='replace'))
        eof = self.read_packet()  # EOF after the column definitions
        rows = []
        while True:
            r = self.read_packet()
            if r[0] == 0xfe and len(r) < 9: break
            if r[0] == 0xff: return ('ERR', r[9:].decode(errors='replace'))
            vals = []; i = 0
            for _ in range(ncol):
                if r[i] == 0xfb: vals.append('NULL'); i += 1
                else:
                    v, i = lenenc_str(r, i); vals.append(v.decode(errors='replace'))
            rows.append(vals)
        return ('ROWS', (cols, rows))

def main():
    host, port, script = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    c = Conn(host, port); c.handshake()
    print(f'connected: server version {c.server_version}')
    for stmt in [s.strip() for s in script.split(';') if s.strip()]:
        kind, res = c.query(stmt)
        if kind == 'ROWS':
            cols, rows = res
            print(f'> {stmt}'); print(' | '.join(cols))
            for r in rows: print(' | '.join(r))
            print(f'({len(rows)} rows)')
        else:
            print(f'> {stmt} -> {kind} {res}')
            if kind == 'ERR': sys.exit(1)
    c.seq = 0; c.send_packet(b'\x01')  # COM_QUIT
    c.s.close()

if __name__ == '__main__':
    main()
