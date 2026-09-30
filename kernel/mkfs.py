#!/usr/bin/env python3
import struct, sys, os
SECTOR = 512
# mkfs.py <image> [user-dir]: the binaries come from user/ (x86-64) or user/aarch64/ (the arm64
# set, built by `make -C user aarch64`); one that is not built is left out with a note, so a
# partial arm64 set still gives a disk for the tests that do not need it.
udir = sys.argv[2] if len(sys.argv) > 2 else "user"
files = [("hello.txt", "Hello from the Talaria disk! (virtio-blk + TFS1)\n".encode())]
for name in ("mhello", "epsrv", "memhog"):
    path = os.path.join(udir, name)
    if os.path.exists(path): files.append((name, open(path, "rb").read()))
    else: print(f"mkfs.py: {path} is not built — left out of the image", file=sys.stderr)
lba = 1
entries = b""
body = b""
for name, data in files:
    nb = name.encode()[:24].ljust(24, b'\0')
    entries += nb + struct.pack("<II", lba, len(data))
    padded = data + b'\0' * ((-len(data)) % SECTOR)
    body += padded
    lba += len(padded) // SECTOR
block0 = struct.pack("<II", 0x31534654, len(files)) + entries
block0 = block0.ljust(SECTOR, b'\0')
img = block0 + body
# pad to a whole sector
img = img + b'\0' * ((-len(img)) % SECTOR)
# The output path is an argument: the regression suite builds images in tmpfs, not
# in the working tree (the image is derived, and on a spinning disk it is costly too).
out = sys.argv[1] if len(sys.argv) > 1 else "disk.img"
open(out, "wb").write(img)
print("%s: %d bytes (%d sectors), files=%d" % (out, len(img), len(img)//SECTOR, len(files)))
for name, data in files:
    print("  %-12s %d bytes" % (name, len(data)))
