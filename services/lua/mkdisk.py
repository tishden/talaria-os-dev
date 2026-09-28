#!/usr/bin/env python3
import struct, sys
SECTOR=512
files=[("lua",open("lua","rb").read()),("script.lua",open("script.lua","rb").read())]
lba=1; entries=b""; body=b""
for name,data in files:
    entries += name.encode()[:24].ljust(24,b'\0') + struct.pack("<II",lba,len(data))
    p=data+b'\0'*((-len(data))%SECTOR); body+=p; lba+=len(p)//SECTOR
b0=(struct.pack("<II",0x31534654,len(files))+entries).ljust(SECTOR,b'\0')
img=b0+body; img+=b'\0'*((-len(img))%SECTOR)
open(sys.argv[1] if len(sys.argv)>1 else "disk-lua.img","wb").write(img)
print("disk %d bytes, files: %s"%(len(img),", ".join("%s(%d)"%(n,len(d)) for n,d in files)))
