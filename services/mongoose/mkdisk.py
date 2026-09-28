#!/usr/bin/env python3
import struct, sys
SECTOR = 512
files = [("mongoose_srv", open("mongoose_srv","rb").read()), ("index.html", open("index.html","rb").read())]
lba = 1; entries = b""; body = b""
for name, data in files:
    nb = name.encode()[:24].ljust(24, b'\0')
    entries += nb + struct.pack("<II", lba, len(data))
    padded = data + b'\0' * ((-len(data)) % SECTOR)
    body += padded; lba += len(padded)//SECTOR
block0 = struct.pack("<II", 0x31534654, len(files)) + entries
block0 = block0.ljust(SECTOR, b'\0')
img = block0 + body
img = img + b'\0' * ((-len(img)) % SECTOR)
open(sys.argv[1] if len(sys.argv)>1 else "disk-mongoose.img","wb").write(img)
print("disk %d bytes (%d sectors), %s %d bytes" % (len(img), len(img)//SECTOR, files[0][0], len(files[0][1])))
