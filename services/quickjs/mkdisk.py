import struct,sys
S=512; files=[("qjs",open("qjs_bin","rb").read()),("demo.js",open("demo.js","rb").read())]
lba=1;e=b"";body=b""
for n,d in files:
    e+=n.encode()[:24].ljust(24,b'\0')+struct.pack("<II",lba,len(d))
    p=d+b'\0'*((-len(d))%S);body+=p;lba+=len(p)//S
img=(struct.pack("<II",0x31534654,len(files))+e).ljust(S,b'\0')+body
img+=b'\0'*((-len(img))%S)
open(sys.argv[1] if len(sys.argv)>1 else "disk-quickjs.img","wb").write(img)
print("disk %d bytes: %s"%(len(img),", ".join("%s(%d)"%(n,len(d)) for n,d in files)))
