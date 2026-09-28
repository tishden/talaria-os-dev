# busybox on Talaria — an open-source multi-tool in ring 3

Unmodified [busybox](https://busybox.net) 1.36.1, built statically against musl
(linked at 0x10000000, a minimal applet set: awk/echo/sort/sha256sum/…), runs
**in ring 3 on the Talaria kernel**. The awk applet computes and prints:

```
busybox awk: sum(i^2,1..1000)=333833500
busybox awk: upper(talaria)=TALARIA len=7
busybox 1.36.1 on Talaria OS
```

## Build and run

```
curl -sSL https://busybox.net/downloads/busybox-1.36.1.tar.bz2 | tar xj
cd busybox-1.36.1 && make allnoconfig
for a in ECHO PRINTF CAT AWK SORT SHA256SUM WC SEQ EXPR HEAD TAIL GREP TR CUT UNAME; do
  sed -i "s/# CONFIG_$a is not set/CONFIG_$a=y/" .config; done
sed -i 's/# CONFIG_STATIC is not set/CONFIG_STATIC=y/' .config
sed -i 's|CONFIG_EXTRA_LDFLAGS=""|CONFIG_EXTRA_LDFLAGS="-Wl,-Ttext-segment=0x10000000"|' .config
yes "" | make oldconfig && make -j4 CC=musl-gcc HOSTCC=gcc
cp busybox ../busybox_srv && cd .. && python3 mkdisk.py disk-busybox.img
services/busybox/run.sh
```

Kernel mode: `talaria.mode=busybox` (runs the binary from disk with an applet and
its arguments). One applet = one process (no fork). The defconfig is not used
(it pulls kernel headers such as `linux/kd.h`); a minimal text/compute set is built.
