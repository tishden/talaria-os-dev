#!/usr/bin/env python3
import struct, sys, os
SECTOR = 512
files = [
    ("hello.txt", "Hello from the Talaria disk! (virtio-blk + TFS1)\n".encode()),
    ("mhello", open("user/mhello", "rb").read()),
    ("epsrv", open("user/epsrv", "rb").read()),
    ("memhog", open("user/memhog", "rb").read()),
]
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
