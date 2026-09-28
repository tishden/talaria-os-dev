#!/usr/bin/env python3
"""A minimal MongoDB wire protocol client (OP_MSG + BSON, no dependencies): probes mongod on Talaria.
  mongoprobe.py HOST PORT ping   -> {ping:1}
  mongoprobe.py HOST PORT demo   -> buildInfo, insert, find, count (database talaria, collection t)
"""
import socket, struct, sys, time

def enc_doc(d):
    out = b''
    for k, v in d.items():
        key = k.encode() + b'\0'
        if isinstance(v, bool): out += b'\x08' + key + (b'\x01' if v else b'\x00')
        elif isinstance(v, int):
            if -2**31 <= v < 2**31: out += b'\x10' + key + struct.pack('<i', v)
            else: out += b'\x12' + key + struct.pack('<q', v)
        elif isinstance(v, float): out += b'\x01' + key + struct.pack('<d', v)
        elif isinstance(v, str): b = v.encode(); out += b'\x02' + key + struct.pack('<i', len(b) + 1) + b + b'\0'
        elif isinstance(v, dict): out += b'\x03' + key + enc_doc(v)
        elif isinstance(v, list): out += b'\x04' + key + enc_doc({str(i): x for i, x in enumerate(v)})
        elif v is None: out += b'\x0a' + key
        else: raise TypeError(type(v))
    return struct.pack('<i', len(out) + 5) + out + b'\0'

def dec_doc(b, i=0):
    n = struct.unpack_from('<i', b, i)[0]; end = i + n - 1; i += 4; d = {}
    while i < end:
        t = b[i]; i += 1; j = b.index(b'\0', i); k = b[i:j].decode(); i = j + 1
        if t == 0x01: v = struct.unpack_from('<d', b, i)[0]; i += 8
        elif t == 0x02: l = struct.unpack_from('<i', b, i)[0]; v = b[i+4:i+4+l-1].decode(errors='replace'); i += 4 + l
        elif t in (0x03, 0x04): l = struct.unpack_from('<i', b, i)[0]; v = dec_doc(b, i); i += l; v = list(v.values()) if t == 0x04 else v
        elif t == 0x05: l = struct.unpack_from('<i', b, i)[0]; v = f'<bin {l}>'; i += 5 + l
        elif t == 0x07: v = b[i:i+12].hex(); i += 12
        elif t == 0x08: v = b[i] != 0; i += 1
        elif t == 0x09: v = struct.unpack_from('<q', b, i)[0]; i += 8
        elif t == 0x0a: v = None
        elif t == 0x10: v = struct.unpack_from('<i', b, i)[0]; i += 4
        elif t == 0x11: v = struct.unpack_from('<Q', b, i)[0]; i += 8
        elif t == 0x12: v = struct.unpack_from('<q', b, i)[0]; i += 8
        elif t == 0x13: v = '<dec128>'; i += 16
        else: raise ValueError(f'bson type {t:#x}')
        d[k] = v
    return d

class Mongo:
    def __init__(self, host, port):
        self.s = socket.create_connection((host, port), timeout=20); self.rid = 1
    def cmd(self, doc):
        body = struct.pack('<I', 0) + b'\x00' + enc_doc(doc)
        hdr = struct.pack('<iiii', 16 + len(body), self.rid, 0, 2013); self.rid += 1
        self.s.sendall(hdr + body)
        h = self._recv(16); ln, rid, rto, op = struct.unpack('<iiii', h)
        p = self._recv(ln - 16)
        if op != 2013: raise IOError(f'opcode {op}')
        i = 4
        while i < len(p):
            kind = p[i]; i += 1
            if kind == 0: return dec_doc(p, i)
            l = struct.unpack_from('<i', p, i)[0]; i += l
        raise IOError('no body section')
    def _recv(self, n):
        out = b''
        while len(out) < n:
            c = self.s.recv(n - len(out))
            if not c: raise IOError('closed')
            out += c
        return out

def main():
    host, port, mode = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    m = Mongo(host, port)
    if mode == 'ping':
        r = m.cmd({'ping': 1, '$db': 'admin'}); print(r); sys.exit(0 if r.get('ok') == 1 else 1)
    bi = m.cmd({'buildInfo': 1, '$db': 'admin'})
    print(f"buildInfo: version={bi.get('version')} gitVersion={bi.get('gitVersion')} ok={bi.get('ok')}")
    r = m.cmd({'insert': 't', 'documents': [{'_id': int(time.time() * 1000), 'v': 'talaria', 'n': 1}, {'_id': int(time.time() * 1000) + 1, 'v': 'bare metal', 'n': 2}], '$db': 'talaria'})
    print(f"insert: n={r.get('n')} ok={r.get('ok')} {r.get('writeErrors', '')}")
    r = m.cmd({'find': 't', 'filter': {}, 'sort': {'_id': 1}, '$db': 'talaria'})
    batch = r.get('cursor', {}).get('firstBatch', [])
    print(f"find: ok={r.get('ok')} rows={len(batch)}")
    for d in batch: print('  ', d)
    r = m.cmd({'count': 't', '$db': 'talaria'}); print(f"count: n={r.get('n')} ok={r.get('ok')}")
    r = m.cmd({'listDatabases': 1, '$db': 'admin'}); print('databases:', [d.get('name') for d in r.get('databases', [])])
    print('mongoprobe: ALL OK' if bi.get('ok') == 1 and len(batch) >= 2 else 'mongoprobe: FAILED')

if __name__ == '__main__':
    main()
