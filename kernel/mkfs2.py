#!/usr/bin/env python3
"""TFS2 packer (the read-only Talaria FS with directories and paths up to 83 bytes).
Usage: mkfs2.py out.img host_path:disk/path [host_path:disk/path ...]
       mkfs2.py out.img @spec.txt   (lines host_path:disk/path, # starts a comment)
Parent directories are added automatically (an entry with flags=1, len=0).
Format: block 0 = magic "TFS2", count u32, dirsec u32; entries {name[84], lba u32, len u32,
flags u32} of 96 bytes each from byte 16 across sectors 0..dirsec-1; files from LBA dirsec."""
import struct, sys, os
SECTOR = 512
NAMELEN = 84
def main():
    if len(sys.argv) < 3: print(__doc__); sys.exit(1)
    out = sys.argv[1]
    specs = []
    for a in sys.argv[2:]:
        if a.startswith('@'):
            for line in open(a[1:]):
                line = line.strip()
                if line and not line.startswith('#'): specs.append(line)
        else: specs.append(a)
    files = []  # (diskpath, data)
    for sp in specs:
        host, disk = sp.rsplit(':', 1)
        disk = disk.strip('/')
        data = open(host, 'rb').read()
        files.append((disk, data))
    # parent directories
    dirs = []
    for disk, _ in files:
        parts = disk.split('/')
        for i in range(1, len(parts)):
            d = '/'.join(parts[:i])
            if d not in dirs: dirs.append(d)
    entries = [(d, b'', True) for d in dirs] + [(f, d, False) for f, d in files]
    for name, _, _ in entries:
        assert len(name.encode()) <= NAMELEN - 1, "path too long: " + name
    count = len(entries)
    dirbytes = 16 + count * 96
    dirsec = (dirbytes + SECTOR - 1) // SECTOR
    assert dirsec <= 32, "too many entries (directory > 32 sectors)"
    lba = dirsec
    body = b''
    table = b''
    for name, data, isdir in entries:
        nb = name.encode().ljust(NAMELEN, b'\0')
        if isdir:
            table += nb + struct.pack('<III', 0, 0, 1)
        else:
            table += nb + struct.pack('<III', lba, len(data), 0)
            padded = data + b'\0' * ((-len(data)) % SECTOR)
            body += padded
            lba += len(padded) // SECTOR
    hdr = struct.pack('<III', 0x32534654, count, dirsec) + b'\0' * 4
    dirblk = (hdr + table).ljust(dirsec * SECTOR, b'\0')
    img = dirblk + body
    open(out, 'wb').write(img)
    print("%s: %d bytes (%d sectors), entries=%d (%d directories, %d files), directory %d sect." % (
        out, len(img), len(img) // SECTOR, count, len(dirs), len(files), dirsec))
    for name, data, isdir in entries:
        print("  %-40s %s" % (name + ('/' if isdir else ''), '' if isdir else str(len(data)) + ' bytes'))
if __name__ == '__main__': main()
