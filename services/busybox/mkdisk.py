import struct,sys
S=512; files=[("busybox",open("busybox_srv","rb").read())]
lba=1;e=b"";body=b""
for n,d in files:
    e+=n.encode()[:24].ljust(24,b'\0')+struct.pack("<II",lba,len(d))
    p=d+b'\0'*((-len(d))%S);body+=p;lba+=len(p)//S
img=(struct.pack("<II",0x31534654,len(files))+e).ljust(S,b'\0')+body
img+=b'\0'*((-len(img))%S)
open(sys.argv[1] if len(sys.argv)>1 else "disk-busybox.img","wb").write(img)
print("disk %d bytes, busybox %d"%(len(img),len(files[0][1])))
