#!/bin/sh
# Talaria's regression harness: it boots every mode in QEMU and asserts the output.
# Run from the host: kernel/test.sh   (it brings up the docker talaria-qemu2 itself;
# TAL_IMAGE= names another stand image)
set -u
REPO=$(cd "$(dirname "$0")/.." && pwd)
# The directory of the DISK IMAGES. By default it is a tmpfs (/dev/shm): over one run the
# set rebuilds ~720 MB of images and drives synchronous writes with the device's
# barriers over them (the FS gates themselves require it), and on a spinning disk
# that is the dominating load of the run. In memory it disappears, and the working
# tree stops running wild with derived files.
# It is overridden through TAL_IMGDIR=; if a tmpfs is unavailable — a fallback into kernel/.
IMG=${TAL_IMGDIR:-/dev/shm/talaria-img}
mkdir -p "$IMG" 2>/dev/null || IMG=$REPO/kernel
[ -w "$IMG" ] || IMG=$REPO/kernel
echo "the disk images: $IMG"
# The kernel for the regression — RELATIVE to the repository. `TAL_KERNEL=kernel/target/exp/talaria`
# runs the regression over a candidate build without touching the release kernel: at
# that moment the set on EC2 (`run-remote-all.sh`) may be packing it into an AMI, and
# swapping the file under it is a run on an unverified kernel.
K=${TAL_KERNEL:-kernel/target/x86_64-talaria/release/talaria}
# The CPU model for QEMU. The knob exists exactly so that the set can be run over a
# path that does not otherwise happen on this stand: `-cpu host` under microvm does
# not announce x2APIC, and the whole path "the LAPIC's registers through MSRs" — the
# very one that will execute on a c6i — would never be checked locally.
#   TAL_QEMU_CPU=host,+x2apic kernel/test.sh
CPU=${TAL_QEMU_CPU:-host}
# The stand image. It is a knob because the image PINS THE QEMU VERSION every number in
# docs/ was measured against (services/docker/Dockerfile.qemu), and judging a build of the
# image — a fresh one, or one from another Fedora — must not require editing this script.
#   TAL_IMAGE=my-qemu2 kernel/test.sh
IMAGE=${TAL_IMAGE:-talaria-qemu2}
[ -f "$REPO/$K" ] || { echo "there is no kernel $K — build it first"; exit 1; }
# The uefi-stub CARRIES the kernel's ELF inside itself, so after every rebuild of the
# kernel it goes stale — and a stale stub silently took 14 checks out of the set (the
# whole UEFI block: OVMF, PCIe, NVMe, SMP4 under the MADT). The regression's counter
# then fell from 155 to 149 and looked like "something has been broken". We rebuild it ourselves: it takes seconds.
EFI_P="$REPO/kernel/uefi-stub/target/x86_64-unknown-uefi/release/talaria-uefi-stub.efi"
# Freshness is judged by the kernel's CONTENT, not by mtime: after a build with a
# different feature set (say `--no-default-features` for clippy and back) cargo
# restores the release binary from its own cache, so the file is NEWER than the
# stub while holding different bytes — and a `-nt` rule then calls a stub that
# carries the other kernel fresh. That cost two FAILs in the UEFI block
# (a stub with a kernel built without `selftest`: "this mode runs an embedded
# test binary, but the kernel was built without the selftest feature").
# The stamp lives next to the .efi in target/ and holds the sha256 of the kernel
# the stub was built around. The stub's own source is touched before the build:
# for the same reason cargo calls the stub crate FRESH although the bytes behind
# its `include_bytes!` changed, and then it only relinks the old .efi (MEASURED:
# dropping `.fingerprint` was not enough, `touch src/main.rs` was).
EFI_STAMP="$EFI_P.kernel-sha256"
K_SHA=$(sha256sum "$REPO/$K" | cut -d' ' -f1)
if [ ! -f "$EFI_P" ] || [ ! -f "$EFI_STAMP" ] || [ "$K_SHA" != "$(cat "$EFI_STAMP")" ]; then
  echo "the uefi-stub carries another kernel — rebuilding..."
  touch "$REPO/kernel/uefi-stub/src/main.rs"
  if "$REPO/kernel/uefi-stub/build.sh" >/dev/null 2>&1; then
    echo "$K_SHA" > "$EFI_STAMP"
  else
    echo "  ATTENTION: the uefi-stub did not build — the UEFI block will be skipped"
  fi
fi
# The base TFS1 volume (hello.txt + mhello/epsrv/memhog) — the default disk for almost
# every gate. It is built here rather than kept in the repository: it is just as much a
# derived artefact as the other images, and it goes stale when kernel/user/* is rebuilt.
# kernel/user/*.
(cd "$REPO/kernel" && python3 mkfs.py "$IMG/disk.img" >/dev/null) || echo "mkfs.py: disk.img was not built (no kernel/user/mhello?)"
# The ext2 image for the tests of the RW FS and of the process model (it is recreated: the tests write onto the disk)
E2=$(mktemp -d); cp "$REPO/kernel/user/sigtest" "$REPO/kernel/user/sigtest-hi" "$REPO/kernel/user/sigtest-lo" "$REPO/kernel/user/execmt" "$REPO/kernel/user/fputest" "$REPO/kernel/user/memrace" "$REPO/kernel/user/mbench" "$REPO/kernel/user/fsinfo" "$REPO/kernel/user/fsbig" "$REPO/kernel/user/schedapi" "$REPO/kernel/user/forksrv" "$REPO/kernel/user/forktls" "$E2/"; echo hello > "$E2/hello.txt"
# the symlinks in the image: a "fast" one (the target inside the inode) to a file and to a directory —
# exactly what the launch with the runtime from Ubuntu stumbled over (/lib64/ld-linux… is a symlink)
mkdir -p "$E2/dir"; echo inside > "$E2/dir/inside.txt"
ln -s hello.txt "$E2/link.txt"; ln -s dir "$E2/dirlink"
printf 'binary=/sigtest\nargs=sigtest\nenv=HOME=/\n' > "$E2/cell.manifest"   # for talaria.mode=proc (the path of the DBMS's manifest)
"$REPO/kernel/mkext2.sh" "$E2" "$IMG/disk_ext2.img" 64M >/dev/null 2>&1 || { echo "no mkfs.ext2 (e2fsprogs) — disk_ext2.img was not built"; }
rm -rf "$E2"
# §8: separate ext2 images for fsbench and for the journal test (the test itself spoils them)
# A separate volume for fsbig: it writes files FOUR TIMES larger than the block cache,
# and does not fit into the 64 MB that are enough for the others (256 MB: fsread leaves
# 96 MB, fsyncconc writes another 68).
T=$(mktemp -d); cp "$REPO/kernel/user/fsbig" "$REPO/kernel/user/fsread" "$REPO/kernel/user/fsyncconc" "$T/" 2>/dev/null; "$REPO/kernel/mkext2.sh" "$T" "$IMG/disk_fsbig.img" 256M >/dev/null 2>&1 || echo "no mkfs.ext2 — disk_fsbig.img was not built"; rm -rf "$T"
# §core dumps: a separate ext2 image with `crashme` — the gate itself spoils it (it
# writes core.elf there), and it cannot be shared with the others.
T=$(mktemp -d); cp "$REPO/kernel/user/crashme" "$T/" 2>/dev/null
"$REPO/kernel/mkext2.sh" "$T" "$IMG/disk_core.img" 32M >/dev/null 2>&1 || echo "no mkfs.ext2 — disk_core.img was not built"
rm -rf "$T"
# A restart of a multithreaded cell on SMP: after a crash and by the watchdog.
T=$(mktemp -d); cp "$REPO/kernel/user/crashme" "$T/" 2>/dev/null
printf 'binary=/crashme\nargs=crashme mt\nrestart=1\n' > "$T/cell.manifest"
"$REPO/kernel/mkext2.sh" "$T" "$IMG/disk_crashr.img" 32M >/dev/null 2>&1 || echo "no mkfs.ext2 — disk_crashr.img was not built"
printf 'binary=/crashme\nargs=crashme live\nrestart=1\nwatchdog=2\n' > "$T/cell.manifest"
"$REPO/kernel/mkext2.sh" "$T" "$IMG/disk_wdog.img" 32M >/dev/null 2>&1 || echo "no mkfs.ext2 — disk_wdog.img was not built"
rm -rf "$T"
# Live debugging on SMP: crashme live, a stop before the first instruction.
T=$(mktemp -d); cp "$REPO/kernel/user/crashme" "$T/" 2>/dev/null
printf 'binary=/crashme\nargs=crashme live\nrestart=0\n' > "$T/cell.manifest"
"$REPO/kernel/mkext2.sh" "$T" "$IMG/disk_gdbsmp.img" 32M >/dev/null 2>&1 || echo "no mkfs.ext2 — disk_gdbsmp.img was not built"
rm -rf "$T"
SPIN=$(nm "$REPO/kernel/user/crashme" 2>/dev/null | awk '$3=="spinner"{print $1}'); echo "$SPIN" > "$IMG/spinner.addr"
# A kernel instruction nothing runs after the boot (the 32-bit multiboot2 entry, at its high
# address): the gdb-smp gate puts a SOFTWARE breakpoint there — the code is read-only at the alias
# (W^X), so the stub writes it through the direct map (ARM64-PLAN §10.16 stage 7.4).
nm "$REPO/$K" 2>/dev/null | awk '$3=="mb2_start"{print $1}' > "$IMG/ktext.addr"
# The children that live and die on an AP (crashme forkap, mode=proc).
T=$(mktemp -d); cp "$REPO/kernel/user/crashme" "$T/" 2>/dev/null
printf 'binary=/crashme\nargs=crashme forkap\n' > "$T/cell.manifest"
"$REPO/kernel/mkext2.sh" "$T" "$IMG/disk_forkap.img" 32M >/dev/null 2>&1 || echo "no mkfs.ext2 — disk_forkap.img was not built"
rm -rf "$T"
# The isolation: the workload does not read the physical memory through the kernel's identity window.
T=$(mktemp -d); cp "$REPO/kernel/user/crashme" "$T/" 2>/dev/null
printf 'binary=/crashme\nargs=crashme peek\nrestart=0\n' > "$T/cell.manifest"
"$REPO/kernel/mkext2.sh" "$T" "$IMG/disk_peek.img" 32M >/dev/null 2>&1 || echo "no mkfs.ext2 — disk_peek.img was not built"
rm -rf "$T"
# The same for the crash of ONE thread of a multithreaded cell (crashme mt, mode=cell).
T=$(mktemp -d); cp "$REPO/kernel/user/crashme" "$T/" 2>/dev/null
printf 'binary=/crashme\nargs=crashme mt\nrestart=0\n' > "$T/cell.manifest"
"$REPO/kernel/mkext2.sh" "$T" "$IMG/disk_coremt.img" 32M >/dev/null 2>&1 || echo "no mkfs.ext2 — disk_coremt.img was not built"
rm -rf "$T"
for img in disk_fsb disk_jrn disk_ms disk_filed disk_dcnt; do
  T=$(mktemp -d); echo hello > "$T/hello.txt"
  [ "$img" = disk_ms ] && cp "$REPO/kernel/user/mapshared" "$T/" 2>/dev/null
  "$REPO/kernel/mkext2.sh" "$T" "$IMG/$img.img" 64M >/dev/null 2>&1 || echo "no mkfs.ext2 — $img.img was not built"
  rm -rf "$T"
done
# §3.1: an image with an INDEXED directory (the htree is built by e2fsck -D on the host) —
# 3000 files in /big. Without e2fsck the image is not built and the test is honestly SKIPped.
if command -v e2fsck >/dev/null 2>&1; then
  DXD=$(mktemp -d); mkdir -p "$DXD/big"
  i=1; while [ $i -le 3000 ]; do echo x > "$DXD/big/file_$i.txt"; i=$((i+1)); done
  DIRIDX=1 "$REPO/kernel/mkext2.sh" "$DXD" "$IMG/disk_dx.img" 64M >/dev/null 2>&1 || rm -f "$IMG/disk_dx.img"
  rm -rf "$DXD"
  # the second copy — for the test of the tree's GROWTH in depth (dxlimit=25: a node is
  # "full" early, the tree has to grow by a level), the third — for the test of the
  # DE-INDEXING (dxlimit=4: there is nowhere left to grow, the index is removed)
  [ -f "$IMG/disk_dx.img" ] && cp "$IMG/disk_dx.img" "$IMG/disk_dx2.img"
  [ -f "$IMG/disk_dx.img" ] && cp "$IMG/disk_dx.img" "$IMG/disk_dx3.img"
else rm -f "$IMG/disk_dx.img" "$IMG/disk_dx2.img" "$IMG/disk_dx3.img"; fi
# The ext2 image for the test of a dynamic ELF (PIE + ld-musl + libz): kernel/user/dyn/*
# Without the third-party artefacts (ld-musl/glibc — user/README.md) the image is NOT built,
# and a stale one is deleted — the corresponding tests are honestly SKIPped.
if [ -f "$REPO/kernel/user/dyn/hello" ] && [ -f "$REPO/kernel/user/dyn/ld-musl-x86_64.so.1" ] && [ -f "$REPO/kernel/user/dyn/libz.so.1" ]; then
  DY=$(mktemp -d); mkdir -p "$DY/lib"; cp "$REPO/kernel/user/dyn/hello" "$DY/"; cp "$REPO/kernel/user/dyn/ld-musl-x86_64.so.1" "$REPO/kernel/user/dyn/libz.so.1" "$DY/lib/"
  "$REPO/kernel/mkext2.sh" "$DY" "$IMG/disk_dyn.img" 32M >/dev/null 2>&1 || echo "no mkfs.ext2 — disk_dyn.img was not built"
  rm -rf "$DY"
else rm -f "$IMG/disk_dyn.img"; fi
# The ext2 image for the dynamic tsynctest on a PATCHED ld-musl (PLAN-SYNC stage 3: the automatic pickup)
if [ -f "$REPO/kernel/user/tsync/tsynctest.dyn" ] && [ -f "$REPO/kernel/user/dyn/ld-musl-x86_64.so.1" ]; then
  DS=$(mktemp -d); mkdir -p "$DS/lib"; cp "$REPO/kernel/user/tsync/tsynctest.dyn" "$DS/"; cp "$REPO/kernel/user/dyn/ld-musl-x86_64.so.1" "$DS/lib/"
  "$REPO/kernel/mkext2.sh" "$DS" "$IMG/disk_dynsync.img" 32M >/dev/null 2>&1 || echo "no mkfs.ext2 — disk_dynsync.img was not built"
  rm -rf "$DS"
else rm -f "$IMG/disk_dynsync.img"; fi
# The ext2 image for the glibc dynamics (Ubuntu 22.04: ld-linux + libc.so.6 + libz): kernel/user/glibc/*
if [ -f "$REPO/kernel/user/glibc/hello" ] && [ -f "$REPO/kernel/user/glibc/ld-linux-x86-64.so.2" ] && [ -f "$REPO/kernel/user/glibc/libc.so.6" ]; then
  GL=$(mktemp -d); mkdir -p "$GL/lib64" "$GL/lib/x86_64-linux-gnu"; cp "$REPO/kernel/user/glibc/hello" "$GL/"; cp "$REPO/kernel/user/glibc/ld-linux-x86-64.so.2" "$GL/lib64/"; cp "$REPO/kernel/user/glibc/libc.so.6" "$REPO/kernel/user/glibc/libz.so.1" "$GL/lib/x86_64-linux-gnu/"
  "$REPO/kernel/mkext2.sh" "$GL" "$IMG/disk_glibc.img" 32M >/dev/null 2>&1 || echo "no mkfs.ext2 — disk_glibc.img was not built"
  rm -rf "$GL"
else rm -f "$IMG/disk_glibc.img"; fi

# the talaria-pack disk for the test of the canonical launch (talaria.mode=cell, RELEASE-1.0 §2.1/2.2)
python3 "$REPO/kernel/tools/talaria-pack" -o "$IMG/disk_cell.img" --app "$REPO/kernel/user/cppdtor" >/dev/null 2>&1 || echo "talaria-pack: disk_cell.img was not built"
# ntp under a workload that never touches the network (idleshoot: 5 s of threads, mmap, sleeps)
python3 "$REPO/kernel/tools/talaria-pack" -o "$IMG/disk_ntpcell.img" --app "$REPO/kernel/user/idleshoot" >/dev/null 2>&1 || echo "talaria-pack: disk_ntpcell.img was not built"
# V1.9: the same production path, but the layout of the threads is given IN THE MANIFEST
# (not on the command line): talaria-pack itself writes pin=/spread= into /cell.manifest.
python3 "$REPO/kernel/tools/talaria-pack" -o "$IMG/disk_sched.img" \
  --app "$REPO/kernel/user/schedspread" --pin main:0 >/dev/null 2>&1 \
  || echo "talaria-pack: disk_sched.img was not built"
# The same with NAMED POOLS and a given NUMA node — the whole declarative layer at
# once: pool= + pin= on a pool + numa=<N>.
# ext2 rather than TFS1: the move of the BLOCK CACHE onto the node is checked on this disk too.
python3 "$REPO/kernel/tools/talaria-pack" -o "$IMG/disk_pool.img" --fs ext2 --size 64M \
  --app "$REPO/kernel/user/schedspread" --pool bg:1,2,3 --pin 'main:0' --pin '*:bg' \
  --numa-node 1 >/dev/null 2>&1 || echo "talaria-pack: disk_pool.img was not built"
# §3.2/§3.3: a HYBRID image (GPT: bios-boot + ESP + /boot + talaria-data) — one disk
# that boots both by BIOS (GRUB+multiboot2) and by UEFI (uefi-stub). It is built on the
# host (sgdisk/mkfs.ext2/mtools and docker with grub i386-pc are needed); when the tools
# are absent the test block is honestly SKIPped. TAL_NOBIOS=1 — skip it (it is long).
E2=$(mktemp -d); echo hello > "$E2/hello.txt"
"$REPO/kernel/mkext2.sh" "$E2" "$IMG/disk_hybdata.img" 16M >/dev/null 2>&1; rm -rf "$E2"
rm -f "$IMG/disk_hyb.img"
rm -f "$IMG/disk_mbr.img"
rm -f "$IMG/disk_chain.img"
rm -f "$IMG/disk_tboot.img"
rm -f "$IMG/disk_suite.img"
if [ "${TAL_NOBIOS:-0}" != 1 ] && [ -f "$REPO/kernel/uefi-stub/target/x86_64-unknown-uefi/release/talaria-uefi-stub.efi" ]; then
  # A FAILURE TO BUILD AN IMAGE MUST BE AUDIBLE. Earlier the output went into
  # /dev/null, and any cause turned into one and the same SKIP "sgdisk/mtools/docker
  # are needed" — that is, into SOMEONE ELSE'S explanation. On the 11th of September the
  # ESP turned out to be smaller than the stub, mcopy said "Disk full", and the set
  # silently came five gates of the BIOS path short: that could be caught only by comparing the counter with the previous run.
  HYBRID=1 "$REPO/tools/mk-bios.sh" "$IMG/disk_hyb.img" "$IMG/disk_hybdata.img" \
    "console=ttyS0 talaria.pci=1 talaria.test=ext2" >"$IMG/mk-bios-hyb.log" 2>&1 \
    || { echo "  ATTENTION: mk-bios.sh (the hybrid) did not build the image: $(tail -1 "$IMG/mk-bios-hyb.log")"; rm -f "$IMG/disk_hyb.img"; }
  # §1.3: a non-hybrid (pure MBR) image — the second mode of mk-bios.sh, which until
  # now was checked only by hand
  HYBRID=0 "$REPO/tools/mk-bios.sh" "$IMG/disk_mbr.img" "$IMG/disk_hybdata.img" \
    "console=ttyS0 talaria.pci=1 talaria.test=ext2" >/dev/null 2>&1 || rm -f "$IMG/disk_mbr.img"
  # §8 (A1): an image with OUR OWN loader (tools/mk-tboot.sh) — without GRUB at all.
  # talaria.trace=1024 (A9): the early kernel log is flushed as raw sectors into the gap
  # before the kernel — on a *.metal that is the ONLY channel through which it is visible
  # how far the kernel got. The gate for it is here: an instrument used to measure a run
  # that costs 1.5 $ must break at home.
  TRACE_LBA=2047 "$REPO/tools/mk-tboot.sh" "$IMG/disk_tboot.img" "$IMG/disk_hybdata.img" \
    "console=ttyS0 talaria.pci=1 talaria.test=ext2 talaria.trace=2048" >/dev/null 2>&1 || rm -f "$IMG/disk_tboot.img"
  # A13: a separate COPY of the image for the run with a lot of memory — otherwise the
  # second boot of the same file increases the counter of the boots in the trace, and the
  # check of that counter ("boots 1") honestly fails out of the blue.
  [ -f "$IMG/disk_tboot.img" ] && cp "$IMG/disk_tboot.img" "$IMG/disk_tboot6g.img"
  # A9: an image with a SET of modes (two items): the kernel goes through them in one
  # boot with a reboot of its own between them, and the logs land in different slots.
  # The second item carries A KNOB IN THE TAIL (talaria.wakepreempt=12345): an item's tail
  # is glued onto the cmdline only AFTER the settings are parsed at the start of main, and
  # until session 28 such knobs were silently not applied — that is exactly why
  # talaria.stat= from the user-data did not work on EC2 (see `parse_tuning_opts`).
  SUITE="talaria.test=ext2;talaria.test=wc talaria.wakepreempt=12345" TRACE_LBA=2047 "$REPO/tools/mk-tboot.sh" \
    "$IMG/disk_suite.img" "$IMG/disk_hybdata.img" \
    "console=ttyS0 talaria.pci=1 talaria.suite=1000 talaria.trace=2048" >/dev/null 2>&1 \
    || rm -f "$IMG/disk_suite.img"
  # §1 (A1): a copy of the hybrid with a "talking" marker in the sector 0 — a check of the
  # instrument of the metal diagnostics itself (as/ld are needed on the host; without them — a SKIP).
  if [ -f "$IMG/disk_hyb.img" ]; then
    cp "$IMG/disk_hyb.img" "$IMG/disk_chain.img"
    CHAIN=1 "$REPO/tools/mbr-marker.sh" "$IMG/disk_chain.img" >/dev/null 2>&1 || rm -f "$IMG/disk_chain.img"
  fi
fi

# TAL_CI / TAL_SKIP travel INTO the container: the inner script is a heredoc, so it
# inherits nothing from this shell (a knob that is read but never arrives is the
# defect of session 28 in miniature).
docker run --rm -i --device /dev/kvm --device /dev/net/tun --device /dev/vhost-net \
  -e TAL_CI="${TAL_CI:-0}" -e TAL_SKIP="${TAL_SKIP:-}" \
  --cap-add NET_ADMIN -v "$REPO":/w -v "$IMG":/img "$IMAGE" sh -s "$K" "$CPU" <<'INNER'
set -u
K=$1
CPU=$2          # the QEMU CPU model (TAL_QEMU_CPU=; see the header)
PASS=0; FAIL=0
ip tuntap add tap0 mode tap 2>/dev/null
ip addr add 10.0.0.1/24 dev tap0 2>/dev/null; ip link set tap0 up 2>/dev/null

# boot MODE [net] -> writes the serial into /tmp/o.log, returns the QPID (net: with virtio-net)
# A guest WITH HYPERTHREADS (2 physical cores of 2 threads each): without this QEMU does
# not announce the SMT level in CPUID 0x0B, and there is nothing to check the layout of
# the logical cores over the physical ones on (session 35).
bootht() {
  rm -f /tmp/o.log
  qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 4,cores=2,threads=2 -m 512 \
    -kernel "/w/$K" -append "console=ttyS0 $1" -display none -serial file:/tmp/o.log -no-reboot \
    -drive id=d0,file=/img/disk.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!; qstarted "$1"
}
boot() {
  rm -f /tmp/o.log
  if [ "${2:-}" = net ]; then
    qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
      -kernel "/w/$K" -append "console=ttyS0 $1" -display none -serial file:/tmp/o.log -no-reboot \
      -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
      -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
      -drive id=d0,file=/img/disk.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!; qstarted "$1"
  else
    qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
      -kernel "/w/$K" -append "console=ttyS0 $1" -display none -serial file:/tmp/o.log -no-reboot \
      -drive id=d0,file=/img/disk.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!; qstarted "$1"
  fi
}
# bootd MODE DISK -> like boot, but with the given disk (an ext2 image and so on)
bootd() {
  rm -f /tmp/o.log
  qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
    -kernel "/w/$K" -append "console=ttyS0 $1" -display none -serial file:/tmp/o.log -no-reboot \
    -drive id=d0,file=$2,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!; qstarted "$1"
}
# bootdn MODE DISK -> the disk + virtio-net (talaria.mode=proc requires a NIC)
bootdn() {
  rm -f /tmp/o.log
  qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
    -kernel "/w/$K" -append "console=ttyS0 $1" -display none -serial file:/tmp/o.log -no-reboot \
    -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
    -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
    -drive id=d0,file=$2,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!; qstarted "$1"
}
# THE SUMMARY IN ONE PLACE. It is printed at the end of the set and at the CI
# boundary (`TAL_CI=1`), and a copy of these two lines in two places is exactly how
# the two totals start diverging.
summary() {
  echo ""
  echo "TOTAL: PASS=$PASS FAIL=$FAIL"
  [ $FAIL = 0 ] && echo "GREEN" || echo "THERE ARE FAILURES"
}
# TAL_CI=1 — the DETERMINISTIC SUBSET for CI: the functional blocks (ring 3, the FS,
# the process model, the operator's side) and nothing that measures time, drives the
# network or assembles a boot image. The rest stays the local gate: on a shared
# runner a latency gate does not fail the code, it fails the neighbour.
ci_boundary() {
  [ "${TAL_CI:-0}" = 1 ] || return 0
  echo ""
  echo "TAL_CI=1: the deterministic subset is done (the network, timing and boot-image blocks are the local gate)"
  summary
  [ $FAIL = 0 ] && exit 0 || exit 1
}
# A mode may be excluded by a regexp over its cmdline: TAL_SKIP='talaria.test=clock'.
# CI uses it for the gates that measure time — they judge the runner, not the kernel.
skip_mode() {
  [ -n "${TAL_SKIP:-}" ] && echo "$1" | grep -qE "$TAL_SKIP"
}
# QEMU did not start — say so LOUDLY and with its stderr (session 30: two boots in a
# row without a serial log, and in the regression's log only "got ''").
qstarted() { ( sleep 1.5; [ -f /tmp/o.log ] || echo "  !!! QEMU did not start ($1): $(tail -1 /tmp/q.log 2>/dev/null | cut -c1-160)" >&2 ) & }
# assert PAT NAME : waits for the pattern in the log (up to 6 s), PASS/FAIL
assert() {
  pat=$1; name=$2; ok=0
  for _ in 1 2 3 4 5 6 7 8 9 10 11 12; do
    grep -qE "$pat" /tmp/o.log 2>/dev/null && { ok=1; break; }; sleep 0.5
  done
  if [ $ok = 1 ]; then echo "  PASS  $name"; PASS=$((PASS+1)); else echo "  FAIL  $name (no /$pat/)"; FAIL=$((FAIL+1)); echo "      --- the last lines of the serial:"; tail -4 /tmp/o.log 2>/dev/null | cut -c1-160 | sed 's/^/      /'; noserial; fi
}
# There is no serial log — which means QEMU did not start at all: the cause is in its
# stderr (session 30: two boots in a row fell without a single line, and by the
# regression's log "the kernel is silent" could not be told from "tap0 is taken by the previous QEMU").
noserial() { [ -f /tmp/o.log ] || { echo "      --- there is NO serial log, QEMU did not start; q.log:"; tail -3 /tmp/q.log 2>/dev/null | cut -c1-160 | sed 's/^/      /'; }; }
# assertn PAT N NAME : there are exactly N lines by the pattern in the log. It is needed
# where the defect is not "there is no line" but "there are fewer lines than were
# printed": a line cut in half does not disappear from the log — it stops being a line.
assertn() {
  pat=$1; want=$2; name=$3; got=0
  for _ in 1 2 3 4 5 6 7 8 9 10 11 12; do
    got=$(grep -acE "$pat" /tmp/o.log 2>/dev/null); [ -n "$got" ] || got=0
    [ "$got" = "$want" ] && break; sleep 0.5
  done
  if [ "$got" = "$want" ]; then echo "  PASS  $name"; PASS=$((PASS+1)); else echo "  FAIL  $name (the lines $got, we expected $want)"; FAIL=$((FAIL+1)); fi
}
# refute PAT NAME : the pattern MUST NOT be in the log. Call it AFTER an assert over the
# same run: by that moment the mode has already worked, and "empty" means "it has not
# been printed" rather than "it has not got there yet".
refute() {
  pat=$1; name=$2
  if grep -qE "$pat" /tmp/o.log 2>/dev/null
  then echo "  FAIL  $name (the log has /$pat/)"; FAIL=$((FAIL+1)); grep -nE "$pat" /tmp/o.log | head -2 | cut -c1-160 | sed 's/^/      /'
  else echo "  PASS  $name"; PASS=$((PASS+1)); fi
}
# stopq PID: kill qemu and WAIT for the process's death. The former pattern
# "kill $q; wait $q" did not wait: qemu is a child of the subshell $(boot ...), a wait on
# somebody else's child returns at once, and the next boot started while the previous one
# still held the image's write lock (vars.fd/disk*.img) -> an instant death of QEMU, an
# empty serial (the historical flake of ext2v and of the uefi pairs is exactly this).
stopq() {
  kill $1 2>/dev/null
  for _ in $(seq 1 100); do kill -0 $1 2>/dev/null || return 0; sleep 0.1; done
}
# assert_slow: the same with a window of 20 s — the boots through OVMF (the firmware + a
# PE of ~28MB) under the host's load do not fit into 6 s (it was caught as a flake FAIL of the uefi tests)
assert_slow() {
  pat=$1; name=$2; ok=0
  for _ in $(seq 1 40); do
    grep -qE "$pat" /tmp/o.log 2>/dev/null && { ok=1; break; }; sleep 0.5
  done
  if [ $ok = 1 ]; then echo "  PASS  $name"; PASS=$((PASS+1)); else echo "  FAIL  $name (no /$pat/)"; FAIL=$((FAIL+1)); echo "      --- the last lines of the serial:"; tail -4 /tmp/o.log 2>/dev/null | cut -c1-160 | sed 's/^/      /'; fi
}

echo "== the non-network modes =="
for spec in \
  "talaria.test=cmdline|cmdline-test: ALL OK|cmdline: the mode's name is compared IN FULL — a prefix of a foreign name does not start a foreign mode" \
  "talaria.test=user|Hello from Talaria.s userspace.*ring 3|user: ring-3 ELF" \
  "talaria.test=musl|musl static binary -> Talaria OS|musl: a static binary" \
  "talaria.test=printf|Talaria: the line 2, malloc|printf: musl printf/malloc" \
  "talaria.test=crypto|AES-128-GCM self-test .NIST.: PASS|crypto: AES-GCM self-test" \
  "talaria.test=fs|Hello from the Talaria disk|fs: reading a file from virtio-blk (TFS1)" \
  "talaria.test=diskexec|musl static binary -> Talaria OS|fs: the launch of a binary from the disk" \
  "talaria.test=pthreads|PTHREADS: counter=200000 .we expected 200000. -> OK|threads: clone+futex+mutex+join" \
  "talaria.test=cpp|Talaria runs C..! sum.sq 1..10.=385|cpp: STL + exceptions + static-init" \
  "talaria.test=cpp2|C.. threads: atomic=200000 guarded=200000|cpp2: std::thread+mutex+chrono" \
  "talaria.test=cpp3|iostream.map.sort: talaria=3|cpp3: iostream + map + sort" \
  "talaria.test=cppdtor|the global destructor with the futex path has worked|cppdtor: the global destructors (a thread+a condvar after main)" \
  "talaria.test=cppdtor|exit_group.the code=0. — the process has finished|cppdtor: a clean exit exit_group(0) — without a #DE/a crash (RELEASE-1.0 §1.3)" \
  "talaria.test=mem|MEMTEST: 2000 cycles.*the reclaim works|mem: mmap/munmap reclaim" \
  "talaria.test=devnull|DEVNULL: /dev/null OK|devnull: the fd table + /dev/null (the write is discarded, read=EOF, close)" \
  "talaria.test=uaf|uaf: ALL CHECKS PASSED|uaf: munmap really removes the mapping (a #PF after munmap, no UAF)" \
  "talaria.test=identhole|identhole: the kernel's write by identity after munmap: OK|identhole: A MINE — the kernel writes into a frame by its identity address while staying under the AS of a workload that removed that address (before the fix — a #PF in ring 0 and the machine stopping)" \
  "talaria.test=identhole|identhole: U/S cleared on the removed page: OK|identhole: the removed page is closed to ring 3 (identity is kept for THE KERNEL, not for the workload — otherwise a UAF)" \
  "talaria.test=identhole|identhole: the identity invariant in the workload's AS: holes 0 substitutions 0 read-only 0|identhole: in the whole 0..4GB window of the workload's address space there is not a single hole (kstat paging prints the same counter)" \
  "talaria.test=wxorx|wxorx: ALL CHECKS PASSED|wxorx: W^X + NX + a guard page + a live #PF" \
  "talaria.test=allocfail|allocfail: ALL OK|allocfail: a refused contiguous request leaves the bump whole (it used to take the rest with it: a Graviton's refused fscache=512 panicked the next kernel stack)" \
  "talaria.test=kimg|kimg: ALL OK|kimg: the image's virtual and physical addresses — KIMG_OFF is the linker's KOFF, kpa of a .text/.rodata/.data/.bss word is what the tables say (ARM64-PLAN §10.16 stage 7)" \
  "talaria.test=dmapid|dmapid: ALL OK|dmapid: the address kinds round-trip (a low and a high frame, a .bss and a stack word: physical -> kernel pointer -> physical, written and read back) — the contract the direct map keeps (ARM64-PLAN §10.16)" \
  "talaria.test=dmapid|dmapid: the direct map ON at 0xffff800000000000|dmapid: the direct map is on by default (slot 256): every kptr goes through it, a word written there reads back through the identity" \
  "talaria.test=dmapid talaria.dmap=0|dmapid: the direct map OFF at .* outside it: 0 |dmapid: talaria.dmap=0 leaves the identity for everything (the A/B and the fallback), and no kva is a miss" \
  "talaria.test=dmapstrict talaria.dmap=strict|dmapstrict: ALL OK|dmapstrict: under talaria.dmap=strict a process space and its fork child carry no identity leaf over the map's RAM, the kernel's space keeps them, the kernel's write through the map does not show in the process's own page at the frame's former identity address (ARM64-PLAN §10.16 stage 4b)" \
  "talaria.test=dmapstrict talaria.dmap=strict talaria.kwin=0|talaria.dmap=strict REFUSED|dmapstrict: strict refuses to start without the stacks' window (an identity kernel stack would be RAM by its identity)" \
  "talaria.test=mwin|mwin: ALL OK|mwin: the kernel reaches every device through the devices' window (slot 384): the LAPIC and the IOAPIC pointers are window addresses that round-trip, a process space shares the window's table and reads the same ID word, no mmio_va fell back to the identity (ARM64-PLAN §10.16 stage 5)" \
  "talaria.test=mwin talaria.mwin=0|mwin: the devices' window OFF .* misses 0 |mwin: talaria.mwin=0 leaves every device at its identity (the A/B and the fallback)" \
  "talaria.test=identhole talaria.dmap=strict|identhole: ALL CHECKS PASSED|identhole: under strict a munmap leaves no leaf at all (not the identity one), the kernel writes the frames through the map, no unintended hole" \
  "talaria.test=sharedpd|sharedpd: the gigabyte 0x80000000 was split for a device window: OK|sharedpd: the shared gigabyte of the kernel PDPT is split into a directory+table (like the LLQ window of ENA on EC2)" \
  "talaria.test=sharedpd|sharedpd: the kernel's directory after the AS teardown: OK|sharedpd: A MINE — tearing a process's AS down freed the kernel's SHARED directory (the frame into the free list -> a #PF in the NVMe doorbell under any CR3; PostgreSQL on EC2 died after 0.3 s)" \
  "talaria.test=fault|FLIGHT RECORDER: [0-9]+ events|flightrec: the crash dump + the ring of the events (the post-mortem)" \
  "talaria.test=pmu|pmu: OK .cycles=[0-9]+|pmu: the rdpmc counters (cycles/instrs/IPC, a TSC fallback)" \
  "talaria.test=dirtyprot|dirtyprot: OK|paging: a written page made read-only keeps its dirty bit (protect rebuilt the leaf without D — a MAP_SHARED page lost its writeback after an mprotect)" \
  "talaria.test=wc|wc-test: ALL OK|wc: the PAT4 slot = write-combining, the page has been moved into WC (A1: ENA's LLQ window)" \
  "talaria.test=wc|wc-test: PTE .*PAT=1 PWT=0 PCD=0|wc: a 4K PTE — the PAT is set, the PCD/PWT are cleared (the neighbouring page did not change its type)" \
  "talaria.test=vga|vga-test: the line was found in the screen.s buffer|vga: the screen as a second channel of output — the bytes reach the buffer 0xB8000 (PLAN-V1.6 §2)" \
  "talaria.test=vga|vga-test: the last line is at the bottom=true the top ones went away=true|vga: the scrolling of the screen (25 lines) — the old goes away, the new is at the bottom" \
  "talaria.test=slice|slice-test: ALL OK|slice: the scheduler's slice is given by TIME rather than by a LAPIC counter (the timer's frequency is measured by the TSC)" \
  "talaria.test=slice|slice-test: the effective slice [0-9]+ ns -> the counter [0-9]+ .we expected|slice: the timer's counter corresponds to the given slice" \
  "talaria.test=slice talaria.slice=200000|slice-test: the effective slice 200000 ns|slice: the knob talaria.slice= gives the slice IN NANOSECONDS (earlier the gate itself programmed the default and with the knob it would have failed on a healthy machine)" \
  "talaria.test=slice talaria.slice=200000|slice-test: ALL OK|slice: with an overridden slice the timer's counter agrees too" \
  "talaria.test=clock|clock-selftest: ALL OK|clock: the wall-clock servo on a synthetic drift (the convergence, the monotonicity, the clamp)" \
  "talaria.test=clock|clock-selftest: a jerk of the reference .2 ms -> over a second 500000 ns was picked up|clock: a jerk of the reference is absorbed by the FREQUENCY in portions of 500 ppm rather than by a jump of the value" \
  "talaria.test=clock|clock-selftest: a foreign clock -2 s -> steps 1, backwards 1|clock: a step BACKWARDS is possible only when the epochs diverge and it is counted (a silent jump = a negative latency in a measurement)" \
  "talaria.test=clock|clock-selftest: CLOCK_MONOTONIC at a change of the rate -> backwards 0 ns \(with the former construction [1-9][0-9]{5,} ns\)|clock: the publication into the vDSO changes the RATE rather than the value — CLOCK_MONOTONIC in ring 3 does not jump backwards at an adjustment (it used to: up to 520 us on mdpipe)" \
  "talaria.test=clock|clock-test: over a second samples [0-9]+, adjustments [0-9]+, steps 0, reads [0-9]+, backwards 0|clock: the live discipline by kvmclock — millions of reads of wall_ns on top of the adjustments, not a single step backwards" \
  "talaria.test=clock|clock-test: ALL OK|clock: the residual error against kvmclock < 1 ms, no step by the value was needed" \
  "talaria.test=nicdev|nicdev: binding rules checked 7|netdev: the rules of binding a driver to a device (a mistake here does not crash but SILENTLY gives the card to a foreign driver)" \
  "talaria.test=nicdev|nicdev: the classification of the interrupts: MSI-X / MSI / INTx / both -> MSI-X|netdev: MSI is parsed on a par with MSI-X, a card with a single INTx is recognised (earlier such a one silently did not interrupt)" \
  "talaria.test=nicdev|netdev: the card 'loop' #0 — mac=02:54:41:4c:00:01|netdev: a NEW driver has been brought up THROUGH THE TABLE (one file + one line — and nowhere else)" \
  "talaria.test=nicdev|nicdev: sent 32, received 32, mismatches 0, the loop looped back 32, dropped 0|netdev: the datapath (NicQ) drives an unknown card knowing nothing about it — the bytes agree" \
  "talaria.test=nicdev|nicdev: ALL OK|netdev: the model of the device in full (the table -> probe -> the rings -> the frames)" \
  "talaria.test=cat|cat-selftest: ALL OK|cat: the validation of the L3 masks (the contiguity of the bits is the hardware's requirement, breaking it = a #GP) + the arithmetic of the rings' footprint" \
  "talaria.test=cat|cat-selftest: the verdict on the footprint at an L3 of 24 MB: 1 MB=in the window 8 MB=in the L3, but not in the window 32 MB=DOES NOT fit|cat: the rule of the leaky DMA (v0.15) has been checked on a FIXED L3 — not only where it is observed anyway" \
  "talaria.test=cat|cat: the footprint of the ENA receive with 32 queues .A6. rings — 32 queues, 16640 KB|cat: the footprint of a multi-queue receive is counted BY ITSELF (A6 will break the rule silently — 32 queues are 16 MB)" \
  "talaria.test=cat|sched-plan: ATTENTION — llc= refers to an unknown group of cores|cat: an llc= onto a non-existent group of cores is rejected LOUDLY (a silently applied share of the cache = an isolation on paper)" \
  "talaria.test=cat|cat-test: ALL OK|cat: the end-to-end path the manifest -> the pool of cores -> the L3 share -> the hardware (the parsing of llc=, the refusal without CAT)" \
  "talaria.test=manifest|manifest: ALL CHECKS PASSED|manifest: the workload declaration parser (binary/args/restart + the placement rules pin=/spread=/numa=)" \
  "talaria.test=manifest|manifest: placement rules 4 .broken 1., spread=false balance=true numa=false|manifest: a broken pin= line IS COUNTED rather than swallowed" \
  "talaria.test=affinity|affinity: ALL CHECKS PASSED|affinity: the policy of the layout — the choice of a core, the dedication, the NUMA locality, the convergence of the balancing (V1.9)" \
  "talaria.test=affinity|affinity: 19 threads on 8 cores: [0-9]+ rearrangements, it became .2..3.|affinity: more threads than cores -> an even load (the spread <= 1) within a finite number of migrations" \
  "talaria.test=affinity|affinity: the demand over a decision window .32 updates.: a ready one 1024 out of 1024|affinity: the demand is a RUNNING AVERAGE, and its time constant is given by a number of updates — within one window of the decision it must REACH the limit (while it was refreshed once per pass it reached a third and the skew was never found)" \
  "talaria.test=affinity|affinity: OK   the demand of TWO always-ready threads over one window reaches the skew.s threshold|affinity: three constants are bound by one assertion — the frequency of the demand's refreshing, the filter's constant and the skew's threshold (none of them can be broken singly)" \
  "talaria.test=affinity|affinity: OK   the demand of a fallen-asleep thread over the same window falls almost to zero|affinity: the demand of a thread that has fallen asleep fades within the same window (otherwise the donor would be chosen by the past)" \
  "talaria.test=affinity|affinity: OK   the skew.s threshold lies STRICTLY between one and two ready threads|affinity: the threshold is not exactly two threads — a running average approaches the limit from below" \
  "talaria.test=netcfg|netcfg-test: ALL OK|netcfg: the arithmetic of the routing (the mask by the prefix, the normalisation of the network, the parsing of a MAC) — a mistake here does not crash but SILENTLY sends the packet into another card" \
  "talaria.test=filed|filed-test: ALL OK|filed: the CRC32 matches the IEEE reference and is counted over the pieces the same as over the whole (otherwise 'the file matched' would be said about DIFFERENT files)" \
  "talaria.test=console|console: the self-test is OK|console: the command dispatcher (help/status/mem/ps/dump/log)" \
  "talaria.test=console talaria.console=nopoll|talaria: the operator console is NOT polled from the tick|console: talaria.console=nopoll switches the COM1 poll off in the tick (for a machine where every microsecond matters)" \
  "talaria.test=console|Talaria  uptime .* tasks [0-9]+|console: top works OUTSIDE an interactive console too (from a tick — one snapshot rather than a loop waiting for a key)" \
  "talaria.test=console|kstat sched: tasks [0-9]+ .the ceiling 192.|console: kstat gives out the scheduler's section" \
  "talaria.test=console|perf: SUMMARY  IPC |console: perf computes IPC from the hardware counters" \
  "talaria.test=obs|paging: the frame allocator starts at 0x[0-9a-f]+ — above the supervisor leaf 0x[0-9a-f]+: OK|paging: the frames for ring 3 are not taken from the supervisor list of 2 MB with the kernel's tail (otherwise the workload's stack falls on a #PF, and the refusal depends on the SIZE OF THE IMAGE)" \
  "talaria.test=obs|unwind: the unwinding checks all passed|unwind: stack unwinding on a SYNTHETIC frame chain — a loop, a step backwards, a frame that is too large, an unmapped page, a foreign ring, the depth ceiling" \
  "talaria.test=profdeep|profdeep: ALL OK|profdeep: the WORKLOAD's stack unwinds on a live workload (five levels of calls, built with -fno-omit-frame-pointer)" \
  "talaria.test=profdeep|profdeep: ring3 samples [0-9]+, with a stack [0-9]+, the depth up to [5-8], the most frequent stack [2-9][0-9]* times|profdeep: the chain of calls is STABLE — one and the same stack is seen dozens of times rather than crumbling into unique ones" \
  "talaria.test=profdeep|#talaria-prof v2 samples=[0-9]+ total=[0-9]+|profdeep: perf record dumps in format v2 (read by kernel/tools/talaria-prof)" \
  "talaria.test=profdeep|SAMPLE 3 [0-9]+ [0-9]+ [^ ]+ 0x[0-9a-f]+ 0x[0-9a-f]+ 0x[0-9a-f]+|profdeep: what lies in the dump is a STACK rather than a single address" \
  "talaria.test=profdeep|perf: WORKLOAD .ring 3. — [0-9]+ samples, average depth [3-8]|profdeep: perf top shows the average stack depth, not just leaves" \
  "talaria.test=obs|obs: ALL OK|obs: the observability — the formatting of the values, the walk over the tables for the RSS, the arithmetic of the PMU, every command runs to the end" \
  "talaria.test=obs|obs: the walk over AS 0x[0-9a-f]+: resident [0-9]+ pages|obs: the RSS is counted by A WALK OVER THE TABLES rather than by a counter on the fault's path" \
  "talaria.test=obs|kstat: there is no section 'no-such-thing'. There are: sched mem paging fs net sync irq cache syscalls|obs: an unknown section of kstat answers with a list rather than with silence" \
  "talaria.test=obs|NET    the stack is not up|obs: the commands run to the end on a machine WITHOUT a network and WITHOUT an FS — that is exactly when they are called" \
  "talaria.test=sysns|SYSNS: 188137 ms / 448 calls -> 4199[0-9]+ ns per call|sysns: the ns per call are counted in u128 — 188 s of accumulated time overflowed u64, and the instrument printed 8.19 ms instead of 420 (session 42)" \
  "talaria.test=sysns|SYSNS: talaria.sysrow=.1,18,74. -> accepted 3, .x, ,999999. -> accepted 0|sysns: talaria.sysrow= accepts numbers and REJECTS garbage out loud (a knob that stays silent at a typo is dearer than an absent one)" \
  "talaria.test=sysns|SYSNS: OK|sysns: the arithmetic of the stat: block agrees with itself — 'ms in total' and 'ns per call' no longer contradict each other" \
  "talaria.test=reqwho|REQWHO: the decision table AGREES .6 combinations.|reqwho: the decision table of the instrument 'who got the core' — all the combinations (in a request / alive / waiting for the network / ring0) rather than one path" \
  "talaria.test=reqwho|REQWHO: a switched-off instrument does not count: yes|reqwho: the instrument stays silent until talaria.appprof=1 is on (the price of the switched-off one is one read of an atomic in strace_note)" \
  "talaria.test=reqwho|REQWHO: rotations 3 — also in a request 2, nobody 1, returns 1|reqwho: the counters of the rotations and the time until a taken-off thread returns close on a SYNTHETIC sequence of decisions" \
  "talaria.test=reqwho|REQWHO: OK|reqwho: the instrument of the step 1 of session 43 agrees with itself — the shares, the buckets 'threads in a request on a core' and the return" \
  "talaria.test=reqfirst|REQFIRST: switched off .0. -> the protection is silent|reqfirst: with talaria.reqfirst=0 there is no policy at all (the default is the scheduler's former behaviour)" \
  "talaria.test=reqfirst|REQFIRST: a network waiter -> we keep the core, an ordinary thread -> we give it away|reqfirst: the policy is NARROW — it shields only the one waiting for the network; it does not touch a rotation to another thread in a request (otherwise it is a long quantum, and that is harmful — session 42)" \
  "talaria.test=reqfirst|REQFIRST: the ceiling is 1 us, 1 ms has passed -> the protection is lifted .expiries [1-9]|reqfirst: THERE IS NO STARVATION — the protection lives no longer than the ceiling from the start of the request, the expiries are counted" \
  "talaria.test=reqfirst|REQFIRST: OK|reqfirst: the policy of the step 2 of session 43 — all four decisions and the anti-starvation" \
  "talaria.test=obslive|obs-live: ALL OK|obs-live: on a live workload the time on the processor, the syscalls BY THREAD, the switches and the profile's samples are non-zero" \
  "talaria.test=obslive|obs-live: the processor time per task [0-9]+[^,]+, switches [1-9]|obs-live: the accounting of the time by the TSC's ticks (the ticks would lie with a zero on a core without a timer)" \
  "talaria.test=obslive|obs-live: samples with a stack deeper than the leaf [0-9]+ out of [0-9]+|obs-live: the profile's samples have a depth — the stack is taken on an ordinary workload too" \
  "talaria.test=console|console-ed: the editor's checks passed .all.|console: the line editor — the history, Ctrl-A/D/U/W, Tab completion, inserting in the middle (one typo no longer costs retyping)" \
  "talaria.test=console|unknown command: 'stauts' — did you mean 'status'|console: a typo -> a hint of the closest command rather than a blank 'unknown'" \
  "talaria.test=vmspace|vmspace: ISOLATION CONFIRMED|vmspace: per-process address spaces (a CR3 switch)" \
  "talaria.test=kstackguard|kstackguard: ALL OK|kstackguard: a forked task kernel stack is guarded in the CHILD space and the kernel one, released with no hole" \
  "talaria.test=kstackguard|kstackguard: the child, the parent and a later space share the window.s tables: OK|kstackguard: the kernel stacks' window (slot 257): one guard for every space, never mapped (ARM64-PLAN §10.16 stage 3)" \
  "talaria.test=kstackguard talaria.kwin=0|kstackguard: ALL OK|kstackguard: talaria.kwin=0 — the identity stacks with the per-space guard still pass the old gate (the fallback)" \
  "talaria.test=kstackpush|#DF on its own stack \\(IST1\\).*CR2=0xffff8080000|kstackpush: a kernel stack overflowed by a PUSH: the #PF cannot deliver, the #DF runs on its own IST1 stack instead of a triple fault (ARM64-PLAN §10.16 stage 3's leftover)" \
  "talaria.test=kstackpush|A KERNEL STACK OVERFLOW: CR2 is the GUARD page|kstackpush: the dump names the window stack's guard page" \
  "talaria.test=fork|fork: THE MEMORY WAS CLONED|fork: the clone of the address space (the copy semantics of the memory)" \
  "talaria.test=exec|musl static binary -> Talaria OS|exec: a musl binary in the process's private AS (a CR3 switch)" \
  "talaria.test=procs|sched: all the 2 task.s. have finished|procs: two processes in separate ASes under the scheduler (a per-process CR3)" \
  "talaria.test=procs|stat: syscalls=[1-9][0-9]* page_faults=[0-9]|metrics: the kernel's counters grow (A2.1: syscalls>0 after a workload, available through stat)" \
  "talaria.test=ctlauth|ctlauth: ALL CHECKS PASSED|ctlauth: control-plane authentication (a token is mandatory)" \
  "talaria.test=syscallfilter|sfilter: PASS|syscallfilter: an allowlist of the syscalls — a forbidden one -> EPERM (A3.2)" \
  "talaria.test=soak|soak: ALL CHECKS PASSED|soak: 20k iterations of the allocator's churn — 0 leak, free is stable, 0 crashes (A4.1)" \
  "talaria.test=watchdog|watchdog: ALL CHECKS PASSED|watchdog: the detection of a hang of the workload -> a restart (A4.2)" \
  "talaria.test=procdisk|FROM THE DISK in separate ASes .identical vaddrs|procdisk: two disk workloads in private ASes (A1.3: the run_disk class is isolated)" \
  "talaria.test=forkp|all the 2 task.*COW copies: [0-9]|forkp: fork() COW — the child (rax=0) in a private AS, a refcount copy at a write"; do
  mode=$(echo "$spec" | cut -d'|' -f1); pat=$(echo "$spec" | cut -d'|' -f2); name=$(echo "$spec" | cut -d'|' -f3)
  if skip_mode "$mode"; then echo "  SKIP  $name (TAL_SKIP)"; continue; fi
  q=$(boot "$mode"); sleep 1; assert "$pat" "$name"; stopq $q
done

# Syscall pointer validation (uaccess.rs, release plan 4.2): a process passes
# pointers into the kernel image, the kernel's frames, unmapped, non-canonical,
# NULL, off the end of a mapping, a read-only destination — every one must be
# EFAULT with the kernel alive. The A/B run without the check proves the test
# bites: the same program then crashes the whole kernel.
echo "== syscall pointers (uaccess) =="
q=$(boot "talaria.test=efault"); sleep 1
assert "efault: ALL OK" "efault: pointers the process does not own -> EFAULT, valid buffers and demand-zero pages still work"
refute "CRASH DUMP" "efault: the kernel survives every bad pointer"; stopq $q
q=$(boot "talaria.test=efault talaria.uacheck=0"); sleep 1
assert "CRASH DUMP" "efault: with talaria.uacheck=0 the same program crashes the kernel (the gate tests what it claims)"; stopq $q

# §26.1: THE KNOBS DO NOT STAY SILENT. Every `talaria.*` is printed with its parsed
# value, while an unknown key / a value of the wrong kind / a repeat / an unknown mode is
# a refusal to boot (`cmdline::refuse`) rather than silent work "as it comes". The gate
# holds both sides: the refusal where it must be, and the printing where everything is right.
echo "== the knobs: the registry and the refusal to boot (§26.1) =="
q=$(boot "talaria.test=cmdline talaria.chepuha=1"); sleep 1
assert "the knob talaria.chepuha=1 — AN UNKNOWN KEY" "knobs: the unknown key talaria.chepuha=1 is named by its name"
assert "BOOT REFUSED" "knobs: an unknown key — A BOOT REFUSAL (before: silence and a green run)"
refute "cmdline-test: ALL OK" "knobs: after a refusal the mode does NOT start"; stopq $q
q=$(boot "talaria.test=cmdline talaria.smp=x"); sleep 1
assert "the knob talaria.smp=x — the value is of the wrong kind \(expected an integer\)" "knobs: a non-number in a numeric knob — a refusal naming the kind"; stopq $q
q=$(boot "talaria.test=cmdline talaria.slice=1000 talaria.slice=2000"); sleep 1
assert "the knob talaria.slice=2000 — the key is given TWICE" "knobs: a key with one value given twice — a refusal (which one was meant?)"; stopq $q
q=$(boot "talaria.mode=cel"); sleep 1
assert "the knob talaria.mode=cel — the value is not from the list" "knobs: a typo in the mode (mode=cel) — a refusal, the list of the modes is closed"; stopq $q
q=$(boot "talaria.test=cmdlin"); sleep 1
assert "talaria.test=cmdlin — there is no such mode" "knobs: a typo in the name of a self-check — a refusal (earlier: a warning and the production branch)"
assert "BOOT REFUSED" "knobs: an unknown self-check — A BOOT REFUSAL"; stopq $q
q=$(boot "talaria.test=cmdline talaria.slice=300000 talaria.ena=llq talaria.ena=rss"); sleep 1
assert "the knob talaria.slice = 300000 — the scheduler's slice, ns" "knobs: a correct knob is printed with its PARSED value and its purpose"
assert "the knob talaria.ena = rss — the ENA driver.s options" "knobs: an accumulating key (ena) MAY be repeated"
assert "cmdline-test: ALL OK" "knobs: the registry is alphabetical, without duplicates, the audit self-test and both forms of a key (key/key=) give one value"
refute "BOOT REFUSED" "knobs: a correct string is not rejected"; stopq $q

echo "== restartmem: memory/the image across supervise restarts =="
q=$(boot "talaria.test=restartmem")
for _ in $(seq 1 50); do grep -q "the restart limit" /tmp/o.log 2>/dev/null && break; sleep 0.5; done
assert "the restart limit .2. is exhausted" "restartmem: 3 memhog instances under supervise (reloading the image after a crash works)"
if grep -qE "drift=" /tmp/o.log && ! grep -E "drift=" /tmp/o.log | grep -qvE "drift=-?[0-9]{1,3}KB"; then
  echo "  PASS  restartmem: the memory drift between restarts is < 1MB (the mmap regions/kernel stacks were returned)"; PASS=$((PASS+1))
else
  echo "  FAIL  restartmem: the memory drift on a restart is >= 1MB"; FAIL=$((FAIL+1)); grep -aE "drift=" /tmp/o.log | sed 's/^/      /'
fi
stopq $q

echo "== talaria.mode=cell: the canonical launch of a talaria-pack disk (RELEASE-1.0 §2.1/2.2) =="
q=$(bootd "talaria.mode=cell" /img/disk_cell.img); sleep 1
assert "a workload by the manifest /cell.manifest" "cell: the talaria-pack disk + the generated /cell.manifest have been read (a launch without editing the kernel)"
assert "the workload finished normally .code 0." "cell: the workload ran under the supervisor and exited cleanly"
stopq $q

# THE TUNING SETTINGS FROM THE MANIFEST: the parsing is checked by talaria.test=manifest,
# while HERE it is checked that the value reached the machine. The difference is not
# formal: `core=N` had been parsed since session 4 and was applied by nobody until session 22.
echo "== the manifest: the application of the tuning settings (slice/idleslice/netslice/wakepreempt/fscache/readahead/netyield/stealpush) =="
q=$(bootd "talaria.test=tuning" /img/disk_ext2.img); sleep 3
assert "tuning: ALL CHECKS PASSED" "tuning: all eight manifest settings REACHED the scheduler, the FS and the network stack (they are read back from the working atomics)"
assert "ext2: the block cache from the manifest: [0-9]+ -> 48 MB" "tuning: fscache= re-issues the block cache on a LIVE mounted FS (the size is chosen at the mount, and the manifest lies on the same disk)"
assert "tuning: a read through the reallocated cache: h=[0-9]+ bytes=6" "tuning: after the cache reallocation the FS reads THE SAME THING (changing the cache base past the tags = reading from someone else's memory, silently)"
stopq $q
# SESSION 28: THE OPERATOR'S KNOB IS SENIOR TO THE MANIFEST. A tuning setting belongs to
# the workload and lives in the manifest — that is true exactly until the moment it has to
# be CHECKED: an A/B on the stand can change only the command line (the image with the
# manifest is built in advance), and while the manifest won silently, an experiment on
# such a knob compared two identical kernels. Here the manifest asks for netyield=0, the
# command line for 7 us; the command line must win, and out loud.
q=$(bootd "talaria.test=tuning talaria.netyield=7" /img/disk_ext2.img); sleep 3
assert "tuning: netyield\(ns\) = 7000" "tuning: a command-line knob OUTRANKS the manifest (otherwise an A/B on the seven settings on a stand is impossible)"
assert "manifest: the setting was not applied — it is set on the command line" "tuning: the manifest's refusal is announced out loud rather than done silently"
stopq $q

echo "== ext2 RW + the process model (the fd tables, pipe, the signals, fork) =="
q=$(bootd "talaria.test=ext2" /img/disk_ext2.img); sleep 1
assert "ext2-test: ALL OK" "ext2: the RW FS on virtio-blk (create/write/pread/ftruncate/rename/unlink/mkdir/rmdir/getdents64, e2fsck-clean)"; stopq $q
# The vDSO: the process's clock without a syscall (clock_gettime from user space).
q=$(boot "talaria.test=sysbench"); sleep 3
assert "CLOCKBENCH: the vDSO WORKS" "vdso: clock_gettime is counted in user space (several times cheaper than a syscall, the values agree)"; stopq $q
q=$(bootd "talaria.test=execmt" /img/disk_ext2.img); sleep 3
assert "exec: the process.s threads finished: 3" "execmt: the exec took ALL the sister threads off (the Linux semantics)"
assert "execmt: ALL OK" "execmt: a multithreaded execve — the new image is alive, the argv/env arrived, the threads start again"; stopq $q
q=$(bootd "talaria.test=symlink" /img/disk_ext2.img); sleep 2
assert "symlink-test: ALL OK" "symlink: the ext2 links (a read through a link, a link in the middle of a path, readlink, stat/lstat, a creation by the kernel, ELOOP)"; stopq $q
q=$(bootd "talaria.test=sigtest" /img/disk_ext2.img); sleep 2
assert "sigtest: MAP_SHARED at 0x6[0-9a-f]{11}, MAP_SHARED\|MAP_FIXED at 0x6f0000000000" "proc: an anonymous MAP_SHARED has an address of its own (the mmap region, not the frames' identity) and a MAP_SHARED|MAP_FIXED is honoured — both stay shared after a fork (ARM64-PLAN §10.16 stage 4a)"
assert "sigtest: SysV shm at 0x70[0-9a-f]{10}, POSIX shm at 0x70[0-9a-f]{10}" "proc: SysV and POSIX shm in the named window (one non-identity address in every process, so a futex in it still wakes across processes), shared after a fork (ARM64-PLAN §10.16 stage 4a)"
assert "the direct map is STRICT" "proc: strict is the default (no identity leaf over the map's RAM in a process space; ARM64-PLAN §10.16 stage 4b)"
assert "sigtest: a low MAP_FIXED at 0xa0000000: granted" "proc: a MAP_FIXED below 4 GiB is granted by default (strict: no identity there)"
assert "sigtest: ALL OK" "proc: the fd tables/dup2/pipe/cwd/readdir + the signals(handler,mask,kill,SIGCHLD,SIGTERM,a sigwait thread,signalfd) + fork(COW,mprotect-RO,MAP_SHARED,SysV shm,the memory at exit) + eventfd/timerfd/procfs/openat(dirfd)/a file mmap"; stopq $q
q=$(bootd "talaria.test=sigtesthi talaria.dmap=strict" /img/disk_ext2.img); sleep 2
assert "sigtest: ALL OK" "proc: sigtest LINKED AT 0x40000000 (RAM's former identity, GB1) under strict — the image, its forks (COW) and its shared memory where the kernel's identity used to be (ARM64-PLAN §10.16 stage 4b)"
refute "RAM BY ITS IDENTITY|DEVICE BY ITS IDENTITY|CRASH DUMP" "proc: the image at 1 GiB under strict — no kernel access missed its kva, no crash"; stopq $q
q=$(bootd "talaria.test=sigtestlo" /img/disk_ext2.img); sleep 2
assert "sigtest: ALL OK" "proc: sigtest LINKED AT 0x200000 — the kernel image's own load address (ARM64-PLAN §10.16 stage 7.4): the image runs at its high alias, a process space has none of its identity"
refute "RAM BY ITS IDENTITY|DEVICE BY ITS IDENTITY|CRASH DUMP" "proc: the image at 2 MB — no kernel access missed its kva, no crash"; stopq $q
q=$(bootd "talaria.test=sigtest talaria.dmap=1" /img/disk_ext2.img); sleep 2
assert "sigtest: ALL OK" "proc: the same with talaria.dmap=1 — the map with RAM's identity kept in the process spaces (the A/B of strict), fork/COW/MAP_SHARED/shm/signals intact"
assert "sigtest: a low MAP_FIXED at 0xa0000000: refused" "proc: with talaria.dmap=1 a MAP_FIXED below 4 GiB is refused (the kernel's identity is in the process space)"
refute "the direct map is STRICT" "proc: talaria.dmap=1 turns strict off"
refute "RAM BY ITS IDENTITY|DEVICE BY ITS IDENTITY|CRASH DUMP" "proc: under strict no kernel access missed its kva, no crash"; stopq $q
q=$(bootd "talaria.test=ext2v" /img/disk_ext2.img); sleep 1
assert "ext2v: .*PERSISTED OK" "ext2v: the data survived a reboot (the write-back cache was flushed at the fsync/exit_group)"; stopq $q

echo "== the operator's side: the shell and the editor in the kernel (without ring 3 and without busybox) =="
q=$(bootd "talaria.test=shell" /img/disk_ext2.img); sleep 1
assert "shell-test: ALL OK" "shell: the operator's shell in the kernel — the parsing of the paths (.. beyond the root), ls/cat/write/append/cp/mv, rm -r, the Tab completion"
assert "shell-test: ok the directory.s entries are sorted by name" "shell: a directory is shown sorted (the order in ext2 is the hash index's, random for a human)"; stopq $q
q=$(bootd "talaria.test=edit" /img/disk_ext2.img); sleep 1
assert "edit-test: ALL OK" "edit: the console's editor (the loading/dd/the undo/the cut/the join/the search/the write)"
assert "edit-test: ok the writing of a truncated file is REJECTED" "edit: a file that did not fit into the buffer is opened READ-ONLY — a truncated one cannot be written over the original"; stopq $q

# §8.5: a file mmap(MAP_SHARED) through the block cache — the coherence and the write-back
q=$(bootd "talaria.test=mapshared" /img/disk_ms.img); sleep 2
assert "mapshared: ALL OK" "mapshared: mmap(MAP_SHARED,fd) — the read/write and the mapping see the same bytes, msync takes it down to the disk"
assert "frag: mmap\(MAP_SHARED\) of a scattered file" "mapshared: a file SCATTERED over the disk is mapped (it used to be an ENODEV)"; stopq $q
# §3.3: the same test in the kernel's identity AS (talaria.cell=identity) — the path where
# the window of the mapping used to have to lie in a contiguous row of the cache's slots
# (otherwise an ENODEV), while the binary itself was read only from the TFS.
q=$(bootd "talaria.test=mapshared talaria.cell=identity" /img/disk_ms.img); sleep 2
assert "mapshared: ALL OK" "mapshared identity-AS: a file MAP_SHARED in the kernel's identity space (a vaddr window outside the identity)"; stopq $q

# §8: the price of an fsync (the coalescing of the ranges, the per-file flush) and the JBD2 journal
q=$(bootd "talaria.test=fsbench" /img/disk_fsb.img); sleep 1
assert "fsbench: ALL OK" "fsbench: an fsync of 512 blocks — the coalescing into ranges, a repeated fsync is almost free"
# THE QUEUE'S DEPTH. A one would mean a queue of depth one: submitted — waited — submitted
# the next, and then somebody else's barrier (tens of milliseconds) takes the device
# entirely. The gate is on virtio-blk; the same one on NVMe is below, in the uefi block.
assert "fsbench: the queue: [0-9]+ commands submitted, the greatest depth ([2-9]|[1-9][0-9]+)" "fsbench: the block layer has a DEPTH (virtio-blk: the commands fly in a batch rather than one by one)"; stopq $q
# The journal: the phase 1 commits a transaction and SIMULATES A CRASH (the checkpoint has
# not been done — the metadata is only in the journal), the phase 2 must replay it at the mount.
q=$(bootd "talaria.test=journal1" /img/disk_jrn.img); sleep 1
assert "journal1: COMMITTED" "journal: the transaction has been committed (a descriptor+the blocks+a commit with the barriers)"
kill -9 $q 2>/dev/null; wait $q 2>/dev/null
q=$(bootd "talaria.test=journal2" /img/disk_jrn.img); sleep 1
assert "journal2: .*RECOVERED OK" "journal: after a crash the metadata has been restored by replaying the journal"; stopq $q
# §2.5 journal=data: the data goes IN THE TRANSACTION rather than by its place — after a
# crash the replay must bring the file's CONTENT back too (in ordered it would have been
# on the disk before the commit, so this path is not checked there).
q=$(bootd "talaria.test=journal1 talaria.ext2=journal=data" /img/disk_jrn.img); sleep 1
assert "the mode journal=data" "journal=data: the mode is on (the data is in the transaction)"
assert "journal1: COMMITTED" "journal=data: a transaction with data has been committed"; stopq $q
q=$(bootd "talaria.test=journal2 talaria.ext2=journal=data" /img/disk_jrn.img); sleep 1
assert "journal2: .*RECOVERED OK" "journal=data: after a crash the file's DATA has been restored too"; stopq $q
# §3.2: a file LARGER than the journal (1500 blocks against 1024). Earlier such a
# transaction rolled the mode back to ordered; now it is cut into parts (the data in
# portions, the metadata last), and the replay must bring the file back in full.
q=$(bootd "talaria.test=journalbig1 talaria.ext2=journal=data" /img/disk_jrn.img); sleep 4
assert "the transactions [2-9]" "journal=data: a transaction larger than the journal has been CUT into parts (not a fallback to ordered)"
assert "journal1: COMMITTED" "journal=data: the cut transaction has been committed"
# The estimate of the room counted the descriptors by the tags rather than by the batch
# (63 blocks): a transaction of 1018 blocks took 17 descriptors instead of 3 and moved
# over the journal's end onto its own descriptor. The kernel says so out loud.
refute "THE JOURNAL HAS BEEN OVERFLOWED" "journal=data: not a single part moved over the journal's end onto its own beginning"
kill -9 $q 2>/dev/null; wait $q 2>/dev/null
q=$(bootd "talaria.test=journalbig2 talaria.ext2=journal=data" /img/disk_jrn.img); sleep 4
assert "journal2: .*RECOVERED OK" "journal=data: the file from the cut transaction has been restored in full (1500 blocks)"; stopq $q

# §3.1 htree: an image with a FOREIGN index (mkfs -O dir_index + e2fsck -D), 3000 files
# in /big. The kernel must find them by a descent over the index (dxcheck compares with a
# full enumeration on EVERY name), and split the leaves with an edit of the node for the
# 3000 new ones it creates; the integrity of the write is confirmed by e2fsck on the host (the block below).
if [ -f /img/disk_dx.img ]; then
  q=$(bootd "talaria.test=htree talaria.ext2=dxcheck" /img/disk_dx.img); sleep 6
  assert_slow "htree: ALL OK" "htree: 3000 names in an indexed directory — the search by the dir_index matches the enumeration"
  assert_slow "the leaves. splits [1-9]" "htree: the growth of a directory by 3000 names splits the leaves and edits the index's node"
  assert_slow "500 names have been deleted, the neighbours by the hash are in place: 500" "htree: a deletion from an indexed directory does not spoil the index"
  # A5: the index FROM SCRATCH — a fresh directory /fresh is created linear, and when it
  # outgrows the first block the kernel builds an htree itself (earlier the index could
  # appear only from foreign tools — e2fsck -D).
  assert_slow "has been indexed \(an htree was built from scratch" "htree: a linear directory that has outgrown a block is indexed by the kernel"
  assert_slow "the fresh directory /fresh: 600 created, 600 found, the index 0" "htree: all the 600 names of a fresh directory are found by their index"
  stopq $q
  # A5: THE GROWTH IN DEPTH. With talaria.ext2=dxlimit=N a node is considered full early:
  # the root overflows, and the tree must grow by a level (indirect_levels 0 -> 1) rather
  # than get de-indexed. A real root with a 4K block holds 508 records — without the knob
  # such a test would require a directory of tens of thousands of files.
  q=$(bootd "talaria.test=htree talaria.ext2=dxlimit=25" /img/disk_dx2.img); sleep 6
  assert_slow "the levels added [1-9]" "htree: at an overflow of the root the tree grows in depth (a level of nodes appears)"
  assert_slow "the node.s splits [1-9]" "htree: a full node of the index is split, the link to one half goes into the root"
  assert_slow "the depth of /big = 1" "htree: the directory lives as a TWO-level tree (indirect_levels=1)"
  assert_slow "htree: ALL OK" "htree: after the tree's growth all the 6000 names are in place"
  stopq $q
  # The path of the DE-INDEXING: with a very tight node (dxlimit=4) there is nowhere left
  # to grow — the root is full with a level already added. The directory must clear
  # EXT2_INDEX_FL and go on working as a linear one.
  q=$(bootd "talaria.test=htree talaria.ext2=dxlimit=4" /img/disk_dx3.img); sleep 6
  assert_slow "deindexed" "htree: when the tree can grow no more, the directory de-indexes itself (it does not break)"
  assert_slow "htree: ALL OK" "htree: after the de-indexing all the 6000 names are found by an enumeration"
  stopq $q
else
  echo "  SKIP  htree: there is no /img/disk_dx.img (mkfs.ext2 -O dir_index and e2fsck are needed)"
fi
# ext2 WITHOUT a journal (talaria.ext2=nojournal): the path by which the FS lives on a
# volume created without has_journal. Until now it did not take part in the regression.
q=$(bootd "talaria.test=ext2 talaria.ext2=nojournal" /img/disk_ext2.img); sleep 2
assert "the journal is off by the flag" "nojournal: the mode without transactions is on"
assert "ext2-test: ALL OK" "nojournal: an RW ext2 works without a journal (write-back + an fsync by the place)"; stopq $q

q=$(bootd "talaria.test=stop talaria.stop=2" /img/disk_ext2.img); sleep 3
assert "sleeper: got SIGTERM, exiting cleanly" "stop: talaria.stop=N -> a SIGTERM to pid 1 -> a clean exit of the process"; stopq $q
# THE POWER BUTTON (stop-instances / system_powerdown): a q35 with PM1 events (microvm has
# none), an NVMe as on EC2. The file is written NOT under an fsync; the press is through QMP.
# The path 1: a SIGTERM -> the process exited -> a sync -> S5 (QEMU finishes by itself).
# The path 2: the process ignores the SIGTERM -> the deadline -> a sync from a tick -> S5.
# After each one — a second boot: the file is in place and the journal is CLEAN (the checkpoint has been done).
bootpwr() {  # MODE -> pid; the button is pressed from the outside (pressbtn)
  rm -f /tmp/o.log /tmp/qmp.sock
  qemu-system-x86_64 -M q35 -enable-kvm -cpu "$CPU" -smp 2 -m 512 \
    -kernel "/w/$K" -append "console=ttyS0 talaria.test=$1 talaria.pci=1 talaria.pwrbtn=3" \
    -drive id=d0,file=/tmp/pwr.img,format=raw,if=none -device nvme,drive=d0,serial=tal001 \
    -qmp unix:/tmp/qmp.sock,server,nowait -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 & echo $!
}
pressbtn() { python3 - <<'PY'
import socket,time
s=socket.socket(socket.AF_UNIX); s.connect('/tmp/qmp.sock'); s.settimeout(3); s.recv(4096)
s.send(b'{"execute":"qmp_capabilities"}'); time.sleep(0.2); s.recv(4096)
s.send(b'{"execute":"system_powerdown"}'); time.sleep(0.2); s.recv(4096)
PY
}
qemu_gone() {  # pid -> PASS if QEMU finished by itself within 20 s (the machine went out by S5)
  ok=0; for _ in $(seq 1 40); do kill -0 $1 2>/dev/null || { ok=1; break; }; sleep 0.5; done
  if [ $ok = 1 ]; then echo "  PASS  $2"; PASS=$((PASS+1)); else echo "  FAIL  $2 (QEMU is alive)"; FAIL=$((FAIL+1)); stopq $1; fi
}
for m in pwrbtn pwrbtnign; do
  cp /img/disk_ext2.img /tmp/pwr.img
  q=$(bootpwr $m)
  assert_slow "sleeper: pid=1 (waiting for|ignoring) SIGTERM" "$m: the process waits, the file is in the cache without an fsync"
  pressbtn
  assert_slow "acpi: the power button \(ACPI PWRBTN\) — a SIGTERM to the processes \(1\)" "$m: the press has been seen by the poll of PM1a_STS from a tick, a SIGTERM to the workload"
  if [ $m = pwrbtn ]; then assert_slow "sleeper: got SIGTERM, exiting cleanly" "$m: the process exited by the SIGTERM"
  else assert_slow "acpi: the workload did not leave in time — a sync of the FS and a power-off" "$m: the SIGTERM has been ignored -> by the deadline a sync from a tick"; fi
  assert_slow "acpi: the FS has been synchronised" "$m: a sync of the FS before the S5"
  qemu_gone $q "$m: the machine went out (ACPI S5), QEMU finished by itself"
  q=$(bootpwr pwrbtnv)
  assert_slow "pwrbtn: PERSISTED OK" "$m: the second boot — the file written without an fsync is on the disk"
  assert "ext2: a JBD2 journal — .* start 0," "$m: the journal after the button is CLEAN (the checkpoint has been done, there is no replay)"
  stopq $q
done
q=$(bootdn "talaria.mode=proc" /img/disk_ext2.img); sleep 3
assert "sigtest: ALL OK" "proc-mode: a process by /cell.manifest (the launch path of PostgreSQL/MariaDB/MongoDB) with a NIC"; stopq $q
# dyn/glibc: a SKIP without the third-party artefacts (the ld-musl/glibc .so — user/README.md)
if [ -f /img/disk_dyn.img ]; then
  q=$(bootd "talaria.test=dyn" /img/disk_dyn.img); sleep 2
  assert "dyn-hello: ALL OK" "dyn: a dynamic ELF (a PIE + a PT_INTERP ld-musl, a file mmap/MAP_FIXED, the threads, dlopen libz)"; stopq $q
else echo "  SKIP  dyn: there is no disk_dyn.img (the ld-musl/libz artefacts — kernel/user/README.md)"; fi
if [ -f /img/disk_glibc.img ]; then
  q=$(bootd "talaria.test=dyn" /img/disk_glibc.img); sleep 2
  assert "dyn-hello: ALL OK" "dynglibc: glibc 2.35 ld-linux (an Ubuntu 22.04 PIE, clone3->ENOSYS, rseq off, CET, uname 5.15) — the threads + dlopen"; stopq $q
else echo "  SKIP  dynglibc: there is no disk_glibc.img (the glibc artefacts — kernel/user/README.md)"; fi

ci_boundary
echo "== the network modes (the functional probes) =="
# userver: a musl UDP echo through the socket syscalls :9998
q=$(boot "talaria.mode=userver" net); sleep 5
R=$(printf 'PONGPONG' | timeout 3 socat -T2 - udp:10.0.0.2:9998 2>/dev/null)
[ "$R" = "PONGPONG" ] && { echo "  PASS  userver: the UDP echo through the socket syscalls"; PASS=$((PASS+1)); } || { echo "  FAIL  userver: UDP socket (got '$R')"; FAIL=$((FAIL+1)); }
stopq $q

# §1.2 netidle on virtio: when idle, SLEEP on the word of the receive ring's used index
# rather than spin iface.poll(). Earlier only ENA had an idle_watch. cpu-pm=on — so that
# the guest is given MONITOR/MWAIT (otherwise only the timer insurance of 200 us works).
# The number for the calibration: the same run WITHOUT netidle gives ~8 000 000 passes per
# turnaround (95 million passes over 12 turnarounds), with netidle ~4 700.
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -overcommit cpu-pm=on -cpu "$CPU" -smp 1 -m 512 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=userver talaria.netidle=1 talaria.netprof=4" \
  -display none -serial file:/tmp/o.log -no-reboot \
  -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
  -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
  -drive id=d0,file=/img/disk.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 &
q=$!
sleep 5
for i in 1 2 3 4 5 6 7 8 9 0; do printf 'IDLEIDLE' | timeout 2 socat -T1 - udp:10.0.0.2:9998 >/dev/null 2>&1; done
R=$(printf 'IDLEIDLE' | timeout 3 socat -T2 - udp:10.0.0.2:9998 2>/dev/null)
[ "$R" = "IDLEIDLE" ] && { echo "  PASS  netidle: virtio — the echo is not lost during the sleep on the receive ring"; PASS=$((PASS+1)); } || { echo "  FAIL  netidle: virtio the echo (got '$R')"; FAIL=$((FAIL+1)); }
P=$(grep -o 'stack_passes/turnaround=[0-9]*' /tmp/o.log | tail -1 | cut -d= -f2)
if [ -n "$P" ] && [ "$P" -lt 50000 ]; then echo "  PASS  netidle: virtio — the stack's passes per turnaround $P (without netidle ~8 000 000)"; PASS=$((PASS+1));
else echo "  FAIL  netidle: virtio — the stack's passes per turnaround '$P' (< 50000 was expected)"; FAIL=$((FAIL+1)); fi
stopq $q

# THE FS LOCK IS NOT HELD OVER A WAIT FOR THE DISK (talaria.fsdevlock=1).
# The lock may be released in the middle of an operation only where nobody above holds any
# state — so the gate drives THE WHOLE set of the FS checks with the knob switched on:
# if the permission has been given to the wrong one, that breaks ext2 at once and loudly.
q=$(bootd "talaria.test=ext2 talaria.fsdevlock=0" /img/disk_ext2.img); sleep 6
assert "ext2-test: ALL OK" "fsdevlock=0: the whole set of the RW-FS checks passes with the old behaviour of the lock too"
grep -q "is held during the wait for the device too" /tmp/o.log \
  && { echo "  PASS  fsdevlock: the rollback by the knob is announced in the log (rather than ignored silently)"; PASS=$((PASS+1)); } \
  || { echo "  FAIL  fsdevlock: the rollback by the knob is not announced"; FAIL=$((FAIL+1)); }
stopq $q

# AN IDLE PASS OF THE STACK IS CANCELLED BY THE RECEIVE RING (the default, without any knobs).
# The waiter no longer asks the clock ("is it long since I looked") but asks the ring
# itself ("has the card brought anything at all") — the word of the used ring is written by
# the device's DMA, it is read without a lock and without an exit into the host. The gate
# checks both the price (the passes per turnaround) and the TRUST in the flag: "the ring
# lied" must be zero, otherwise a frame would wait for a timer debt instead of a delivery.
rm -f /tmp/o.log
q=$(boot "talaria.mode=userver talaria.netprof=4" net); sleep 5
for i in 1 2 3 4 5 6 7 8 9 0; do printf 'RINGRING' | timeout 2 socat -T1 - udp:10.0.0.2:9998 >/dev/null 2>&1; done
R=$(printf 'RINGRING' | timeout 3 socat -T2 - udp:10.0.0.2:9998 2>/dev/null)
sleep 2
[ "$R" = "RINGRING" ] && { echo "  PASS  pollring: the echo goes while the idle passes are cancelled"; PASS=$((PASS+1)); } || { echo "  FAIL  pollring: the echo (got '$R')"; FAIL=$((FAIL+1)); }
# THE DEFAULT OF THE CARD'S LINE IS BY THE DEVICE. On a virtio-mmio an interrupt costs two
# VMEXITs (clear the status + arm the bell), and the measurement of session 22 showed that
# polling WINS there; on ENA the line is an MSI-X message, and the A/B on EC2 gave the
# opposite (mongodb: better 37, worse 0). So "auto" must stay polling ON THIS stand. The
# regression cannot check ENA's side — it is judged by the A/B.
refute "the card.s line GSI" "pollring: without the knob the card's line is NOT laid on a virtio-mmio (the default is polling where an interrupt is expensive)"

P=$(grep -o 'stack_passes/turnaround=[0-9]*' /tmp/o.log | tail -1 | cut -d= -f2)
if [ -n "$P" ] && [ "$P" -lt 30000 ]; then echo "  PASS  pollring: the stack's passes per turnaround $P (with the threshold by time it was ~80 000)"; PASS=$((PASS+1));
else echo "  FAIL  pollring: the stack's passes per turnaround '$P' (< 30000 was expected)"; FAIL=$((FAIL+1)); fi
# THE TRUST IN THE FLAG IS MEASURED BY A SHARE RATHER THAN BY A ZERO. The gate demanded
# EXACTLY zero and therefore flaked: "a lie" is a frame that arrived between the read of
# the ring's word and the taking of the lock, that is, a lawful race rather than a breakage
# of the flag (session 22 measured it at 0.2 % under load). The gate itself sends eleven
# packets — silence is not its scenario, and a one appears here regularly: two regression
# runs out of four fell exactly like that.
# We check what was meant: the passes by a timer debt that brought frames must be no more
# than a percent of all such passes. A broken flag would give tens of percent and would be
# caught.
L=$(grep -o 'the_ring_lied=[0-9]*' /tmp/o.log | tail -1 | cut -d= -f2)
H=$(grep -o 'by_the_timers_debt=[0-9]*' /tmp/o.log | tail -1 | cut -d= -f2)
if [ -n "$L" ] && [ -n "$H" ] && [ "$((L * 100))" -le "$H" ]; then
  echo "  PASS  pollring: the flag \"the card is silent\" is correct — it lied $L times over $H passes by a timer debt (< 1 %)"; PASS=$((PASS+1));
else echo "  FAIL  pollring: the ring lied '$L' times over '$H' passes by a timer debt (< 1 % was expected)"; FAIL=$((FAIL+1)); fi
stopq $q

# §A netpark: A WAITER ON A SOCKET SLEEPS, AND A DELIVERY WAKES IT.
# The polling of the stack from a syscall is replaced by the card's interrupt: the waiter
# parks (it is invisible to the scheduler), the line's handler drives the stack and rings
# the socket's owner. BOTH halves are checked at once — otherwise switching the line on
# makes things worse (the handler is added on top of the polling rather than instead of it).
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -overcommit cpu-pm=on -cpu "$CPU" -smp 1 -m 512 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=userver talaria.netirq=1 talaria.netprof=4" \
  -display none -serial file:/tmp/o.log -no-reboot \
  -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
  -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
  -drive id=d0,file=/img/disk.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 &
q=$!
sleep 5
for i in 1 2 3 4 5 6 7 8 9 0; do printf 'PARKPARK' | timeout 2 socat -T1 - udp:10.0.0.2:9998 >/dev/null 2>&1; done
R=$(printf 'PARKPARK' | timeout 3 socat -T2 - udp:10.0.0.2:9998 2>/dev/null)
[ "$R" = "PARKPARK" ] && { echo "  PASS  netpark: the echo is not lost while the waiter sleeps"; PASS=$((PASS+1)); } || { echo "  FAIL  netpark: the echo (got '$R')"; FAIL=$((FAIL+1)); }
grep -q "the card.s line GSI" /tmp/o.log && { echo "  PASS  netpark: the card's line has been laid (the number is derived from the layout of the slots)"; PASS=$((PASS+1)); } || { echo "  FAIL  netpark: the card's line has not been laid"; FAIL=$((FAIL+1)); }
P=$(grep -o 'stack_passes/turnaround=[0-9]*' /tmp/o.log | tail -1 | cut -d= -f2)
if [ -n "$P" ] && [ "$P" -lt 50000 ]; then echo "  PASS  netpark: the stack's passes per turnaround $P (with polling ~80 000)"; PASS=$((PASS+1));
else echo "  FAIL  netpark: the stack's passes per turnaround '$P' (< 50000 was expected)"; FAIL=$((FAIL+1)); fi
# THE GATE'S MAIN NUMBER: the turnaround does NOT hit the parking's ceiling (1 ms). If it
# has, it means the timer insurance wakes it rather than a delivery, and the whole idea works backwards.
T=$(grep -o 'rx->tx .*p50=[0-9]*' /tmp/o.log | tail -1 | sed 's/.*p50=//')
if [ -n "$T" ] && [ "$T" -lt 200000 ]; then echo "  PASS  netpark: the turnaround rx->tx p50 $T ns — it is woken by a DELIVERY rather than by the parking's ceiling (1 000 000 ns)"; PASS=$((PASS+1));
else echo "  FAIL  netpark: rx->tx p50 '$T' ns (the parking's ceiling is 1 000 000 — which means the delivery does not wake it)"; FAIL=$((FAIL+1)); fi
kill $q 2>/dev/null; sleep 1

# §A netpark over TCP: accept + epoll_wait + read + write. For a waiter in an epoll the
# socket of a NEW CONNECTION has no owner (the listener has one), and without a separate
# ring the accepting thread would sleep until the ceiling on every connection.
q=$(boot "talaria.mode=diskserver talaria.netirq=1" net); sleep 5
rm -f /tmp/p.*; PIDS=""
for i in 1 2 3 4; do
  ( printf 'GET /p%s HTTP/1.0\r\n\r\n' $i | timeout 6 socat -T5 - tcp:10.0.0.2:80 2>/dev/null > /tmp/p.$i ) & PIDS="$PIDS $!"
done
for p in $PIDS; do wait $p; done
OK=0; for i in 1 2 3 4; do grep -q "Talaria epoll server" /tmp/p.$i 2>/dev/null && OK=$((OK+1)); done
[ $OK = 4 ] && { echo "  PASS  netpark: TCP with the parking — accept/epoll/read/write (4/4)"; PASS=$((PASS+1)); } || { echo "  FAIL  netpark: TCP with the parking $OK/4"; FAIL=$((FAIL+1)); }
stopq $q

# §A THE OWNERSHIP OF A SOCKET DOES NOT SURVIVE THE CLOSING OF THE DESCRIPTOR.
# A forking server: the parent accepts a connection (`accept` writes IT down as the owner),
# forks a backend and closes its own copy. While the ownership was not dropped at that,
# the addressed yield (`netsys::ready_peer`) saw "a socket with data whose waiter is the
# parent" and gave the core to it, that one to the background process, that one back: a
# ring for two, while the forked backends were picked by the scheduler NOT ONCE (the
# measurement on PostgreSQL: 30 tasks, zero picks for each; the server did not reach
# `ready to accept connections`).
# TWO ASSERTIONS, AND BOTH ARE NEEDED: "the server answered" passes on a broken kernel too
# (one quantum is enough for a backend), so a counter stands next to it — the mechanism
# must HAVE FIRED rather than merely not got in the way.
# bootn4d MODE DISK -> four cores, a disk and a network (the gate forkcap needs it)
bootn4d() {
  rm -f /tmp/o.log
  qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 4 -m 1024 \
    -kernel "/w/$K" -append "console=ttyS0 $1" -display none -serial file:/tmp/o.log -no-reboot \
    -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
    -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
    -drive id=d0,file=$2,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!; qstarted "$1"
}
q=$(bootdn "talaria.mode=forksrv" /img/disk_ext2.img); sleep 6
rm -f /tmp/fk.*; PIDS=""
for i in 1 2 3 4 5 6; do
  ( printf 'GET /fk%s HTTP/1.0\r\n\r\n' $i | timeout 10 socat -T8 - tcp:10.0.0.2:80 2>/dev/null > /tmp/fk.$i ) & PIDS="$PIDS $!"
done
for p in $PIDS; do wait $p; done
OK=0; for i in 1 2 3 4 5 6; do grep -q "FORKSRV OK" /tmp/fk.$i 2>/dev/null && OK=$((OK+1)); done
sleep 3
[ $OK = 6 ] && { echo "  PASS  forksrv: a fork per connection — all the 6 backends got the processor and answered"; PASS=$((PASS+1)); } || { echo "  FAIL  forksrv: the answers $OK/6"; FAIL=$((FAIL+1)); }
# The counter is printed by the periodic line of the statistics, and a process closes its
# descriptors only after all its threads have stopped (session 44): we wait for the line
# with six closings for up to 10 s rather than read the one that managed to come out.
for _ in $(seq 1 20); do
  D=$(grep -ao 'the ownership was dropped at close [0-9]*' /tmp/o.log | tail -1 | awk '{print $NF}')
  [ -n "$D" ] && [ "$D" -ge 6 ] && break; sleep 0.5
done
if [ -n "$D" ] && [ "$D" -ge 6 ]; then echo "  PASS  forksrv: the ownership of the socket has been dropped by the closing of the descriptor $D times"; PASS=$((PASS+1));
else echo "  FAIL  forksrv: the ownership has been dropped '$D' times (>= 6 was expected)"; FAIL=$((FAIL+1)); fi
stopq $q
# AFTER A fork BOTH ARE INTACT: the parent's stack and the child's TLS.
#
# The gate is about THE COUNTER OF A FRAME'S CO-OWNERS. Its table covered 1 GB ("our RAM
# is <512MB"), while the allocator on EC2 hands out up to three: for the frames above it
# `rc_get` silently gave out ZERO, that is, "there are no co-owners". Both consumers took
# that literally — `cow_fault` made a shared frame writable IN PLACE (the parent and the
# child wrote into each other's memory), and the teardown of the mappings FREED a frame
# while the second owner was alive. On the stand this killed PostgreSQL around `fork`:
# now a backend by a garbage `%fs:0`, now the postmaster by a #GP(0) exception on the
# `retq` at the end of `_Fork` — with a non-canonical return address.
#
# THE MEMORY HERE IS PART OF THE GATE. With 512-1024 MB every frame falls into the table
# and everything is green; the defect lives where the workload's address space goes beyond
# the coverage. So the guest has 4 GB and the workload takes 1.5 GB — on an unfixed kernel
# the parent is spoilt at the SECOND fork out of four hundred.
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 4 -m 4096 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.test=forktls talaria.smp=4" -display none -serial file:/tmp/o.log -no-reboot \
  -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
  -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
  -drive id=d0,file=/img/disk_ext2.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 &
q4=$!
for i in $(seq 1 180); do sleep 1; grep -aq "forktls: ALL OK\|forktls: FAILED\|forktls: THE PARENT\|panic" /tmp/o.log 2>/dev/null && break; done
assert "forktls: matched 100, did not match 0" "forktls: a hundred forks with an address space of 1.5 GB — the child's TLS is intact, the parent's stack and TLS are intact"
assert "forktls: ALL OK" "forktls: the outcome"
refute "forktls: THE PARENT is spoilt" "forktls: the parent's memory is not spoilt (before the fix of the frames' counter — a corruption at the second fork)"
refute "fault\] ring 3" "forktls: not a single crash in ring 3 over a hundred forks"
refute "ATTENTION: the COW refcount table" "forktls: the refcount table covers everything the allocator hands out"
assert "mem: the high area 0x100000000\.\.0x140000000 \(1024 MB\)" "forktls: the guest's gigabyte above 4 GB is the workload's (the high area) — the forks run over it"
stopq $q4

# THE MAIN NODE'S MEMORY ABOVE 4 GB (`mem::high_init`). Until session 58 the allocator
# stopped at 4 GB and that was all a workload could get: 850 MB of a c7g.large's 3995,
# 3 GB of a c6i.xlarge's 8. The gate checks what the high area rests on — the frames are
# identity-mapped there, a returned block comes back from the area's own free list, the
# refcount table reaches its top, a process inherits the leaf and its teardown leaves the
# kernel's, and a MAP_FIXED over such a gigabyte is refused (the kernel writes the frames
# there at their identity address under ANY address space).
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 1 -m 4096 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.test=highmem" -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 &
q4=$!
for i in $(seq 1 60); do sleep 1; grep -aq "highmem: ALL OK\|highmem: FAILED\|no high area\|panic" /tmp/o.log 2>/dev/null && break; done
assert "highmem: ALL OK" "highmem: the workload's frames above 4 GB — identity, their own free list, the refcount, the AS leaf, MAP_FIXED refused"
stopq $q4
# THE KERNEL'S MEMORY FROM ANY RAM (§10.16 stage 6): page tables, the window's stacks, the FS
# and the drivers' buffers come from the high area first (`mem::alloc_any`), and a process
# space whose own tables lie above 4 GB is walked. With the map off everything stays low.
for v in "" " talaria.dmap=0"; do
  rm -f /tmp/o.log
  qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 1 -m 4096 \
    -kernel "/w/$K" -append "console=ttyS0 talaria.test=anyram$v" -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 &
  q4=$!
  for i in $(seq 1 60); do sleep 1; grep -aq "anyram: ALL OK\|anyram: FAILED\|no high area\|panic" /tmp/o.log 2>/dev/null && break; done
  assert "anyram: ALL OK" "anyram$v: the kernel's memory from any RAM — a block and a process space's tables above 4 GB with the map, below it without"
  stopq $q4
done
# A 200 GB guest (RAM not reserved by the host): the RAM past the identity ceiling (192 GB,
# `user::PROC_HEAP_BASE`) is the kernel's own through the map, taken first.
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off,memory-backend=m -object memory-backend-ram,id=m,size=200G,reserve=off -enable-kvm -cpu "$CPU" -smp 1 -m 200G \
  -kernel "/w/$K" -append "console=ttyS0 talaria.test=anyram" -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 &
q4=$!
for i in $(seq 1 90); do sleep 1; grep -aq "anyram: ALL OK\|anyram: FAILED\|no high area\|panic" /tmp/o.log 2>/dev/null && break; done
assert "mem: RAM beyond the identity \(#0\) 0x3000000000\.\.0x3240000000" "anyram 200 GB: the RAM past the identity ceiling (192 GB) is seen"
assert "anyram: alloc_any takes the RAM beyond the identity first: OK" "anyram 200 GB: the kernel takes it first, the workload stays where the identity reaches"
assert "anyram: ALL OK" "anyram 200 GB: the whole gate"
stopq $q4
# A c7g.large's shape (850 MB below 4 GB): forktls's 1.5 GB took 2 MB pages from the LOW
# memory, and the next page table found no frame — a kernel panic on the real machine.
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 4 -m 5120 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.test=forktls talaria.smp=4 talaria.lowmem=850" -display none -serial file:/tmp/o.log -no-reboot \
  -drive id=d0,file=/img/disk_ext2.img,format=raw,if=none,file.locking=off -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 &
q4=$!
for i in $(seq 1 180); do sleep 1; grep -aq "forktls: ALL OK\|forktls: FAILED\|forktls: THE PARENT\|panic\|PANIC" /tmp/o.log 2>/dev/null && break; done
assert "forktls: ALL OK" "forktls on a c7g.large shape (talaria.lowmem=850): the huge pages come from the high area"
refute "there is no frame|PANIC|panicked" "forktls on a c7g.large shape: the kernel keeps the low memory for its page tables"
stopq $q4

# THE CEILING OF THE TASK TABLE IS A REFUSAL RATHER THAN A WRITE PAST THE BOUND.
#
# The check `NREADY >= NTASKS` stood only at the creation of a THREAD, while `fork`/`clone`
# went past it: the index was taken beyond the end of the arrays, and `PENDING[i]`,
# `RSP[i]`, `KSTACK[i]`, `CR3[i]`, `PROC[i]`, `PPID[i]` were written into foreign kernel
# memory — by an ordinary `fork()` from ring 3. From the outside this is a MUTE hang of
# the whole machine: the tick stops, the console is silent, there is no diagnosis.
#
# It is caught by a server with a fork per connection that does not reap its children: the
# 192 slots run out at the 190th connection. On a kernel WITHOUT the fix the line "250
# have been served" will never appear — the machine will stand at the 190th.
q4=$(bootn4d "talaria.mode=forksrv talaria.forkconns=250 talaria.smp=4" /img/disk_ext2.img); sleep 8
FC=$(python3 -c "
import socket
ok=0
for i in range(250):
    try:
        s=socket.create_connection(('10.0.0.2',80),timeout=4)
        s.sendall(b'GET /fk%d HTTP/1.0\r\n\r\n'%i); d=s.recv(256); s.close()
        if b'FORKSRV OK' in d: ok+=1
    except OSError: pass
print('served', ok)
" 2>&1)
echo "  | $FC"
for i in $(seq 1 30); do sleep 1; grep -aq "forksrv: 250 connections have been served" /tmp/o.log 2>/dev/null && break; done
assert "forksrv: 250 connections have been served" "forkcap: the machine SURVIVED the ceiling of the task table (without the fix — a mute jam at the 190th connection)"
assert "sched: ATTENTION: the task table has run out \(192 out of 192: live [0-9]+, dead-unreaped [0-9]+\)" \
  "forkcap: the ceiling is named OUT LOUD and with a breakdown (the live ones against the unreaped children), fork answers EAGAIN"
refute "panic" "forkcap: there is no panic"
stopq $q4

# §A A PARKED ONE IS WOKEN BY A DELIVERY RATHER THAN BY THE BACKSTOP.
# The rule "one pass of the stack per machine" (`talaria.netsolo=1`) puts every waiter to
# sleep except the polling one and relies on a ring to the socket's owner. The owner was
# written down in `epoll_wait`, `accept` and a BLOCKING `read` — but not in `poll`.
# In a server with a thread per connection and a NON-BLOCKING socket (that is how MariaDB
# is built) only `poll` can wait, the owner stayed the accepting thread, and the sleepers
# were woken by the backstop of 5 ms: the ceiling of `mysql/select1` 821 rps against
# 38350, a p50 of 2.7 ms, a p99 of 5.9 ms.
# THE MAIN NUMBER IS THE OUTCOME OF THE SLEEP rather than the time: "it fell asleep" looks
# the same in both cases, while the difference between them is 46 times by the ceiling.
q=$(boot "talaria.mode=mtsrv talaria.netsolo=1" net); sleep 5
PIDS=""
for i in 1 2 3 4; do
  ( exec 3<>/dev/tcp/10.0.0.2/80
    for k in $(seq 1 30); do printf 'Q\n' >&3; head -c 51 <&3 >/dev/null; done
    exec 3<&- ) & PIDS="$PIDS $!"
done
for p in $PIDS; do wait $p; done
sleep 3
W=$(grep -ao 'woken by a delivery [0-9]*' /tmp/o.log | tail -1 | awk '{print $NF}')
T=$(grep -ao 'by the backstop [0-9]*' /tmp/o.log | tail -1 | awk '{print $NF}')
P=$(grep -ao 'mtsrv: conn n=[0-9]* p50=[0-9]*' /tmp/o.log | tail -1 | sed 's/.*p50=//')
if [ -n "$W" ] && [ -n "$T" ] && [ "$W" -ge 20 ] && [ "$T" -le $((W / 4)) ]; then
  echo "  PASS  netsolo: the parked ones were woken by a DELIVERY $W times, by the backstop $T"; PASS=$((PASS+1));
else echo "  FAIL  netsolo: the delivery '$W', the backstop '$T' (the backstop must be a rarity)"; FAIL=$((FAIL+1)); fi
# A BACKSTOP WITH DATA IN HAND IS A DEFECT RATHER THAN A JITTER. The share of the
# backstops says nothing by itself (the idle waiters pour them in), while "it woke by the
# ceiling and the readiness was already there" is exactly a delivery that did not ring.
# That is how the ring ONLY on data was found: the end of a connection (a FIN) makes the
# socket readable, and there was no ring, and the waiter slept the full 5 ms on EVERY closing.
refute "WITH DATA in hand [1-9]" "netsolo: not a single exit by the ceiling with the readiness in hand (the delivery rings both on data and on the end of a connection)"
if [ -n "$P" ] && [ "$P" -lt 3000 ]; then echo "  PASS  netsolo: the request's turnaround p50 $P us — not the backstop's shelf (5000 us)"; PASS=$((PASS+1));
else echo "  FAIL  netsolo: the request's turnaround p50 '$P' us (the backstop's shelf is 5000)"; FAIL=$((FAIL+1)); fi
kill $q 2>/dev/null; sleep 1
# A DELIVERY MUST WAKE A LOADED WAITER. The counter "parkings N, by the ceiling 99 %"
# means nothing by itself: the ceiling is poured in by the IDLE waiters (a thread without
# traffic parks and wakes the whole run), and by it I nearly wrote a healthy mechanism
# down as a defect. Only the separated number judges: it exited by the ceiling AND THE
# DATA WAS ALREADY THERE — that is what a delivery that did not ring is. On this stand
# (mtsrv, the card's line, four connections) there must be a mere handful of them.
q=$(boot "talaria.mode=mtsrv talaria.netirq=1" net); sleep 5
PIDS=""
for i in 1 2 3 4; do
  ( exec 3<>/dev/tcp/10.0.0.2/80
    for k in $(seq 1 60); do printf 'Q\n' >&3; head -c 51 <&3 >/dev/null; done
    exec 3<&- ) & PIDS="$PIDS $!"
done
for p in $PIDS; do wait $p; done
sleep 3
L=$(grep -ao 'WITH DATA in hand [0-9]*' /tmp/o.log | tail -1 | awk '{print $NF}')
if [ -z "$L" ]; then L=0; fi
if [ "$L" -le 2 ]; then echo "  PASS  netpark: the delivery wakes a LOADED waiter — by the ceiling with data in hand $L times"; PASS=$((PASS+1));
else echo "  FAIL  netpark: by the ceiling with data in hand '$L' (the delivery does not ring the owner)"; FAIL=$((FAIL+1)); fi
stopq $q

# dhcp: -netdev user (slirp = QEMU's built-in DHCP server) -> the lease 10.0.2.15,
# the gateway/DNS from the OFFER, a UDP echo through hostfwd (PLAN-V1.1 §3.1: talaria.ip=dhcp)
bootu() {
  rm -f /tmp/o.log
  qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
    -kernel "/w/$K" -append "console=ttyS0 $1" -display none -serial file:/tmp/o.log -no-reboot \
    -netdev user,id=n0,hostfwd=udp::9998-:9998 -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
    -drive id=d0,file=/img/disk.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!; qstarted "$1"
}
q=$(bootu "talaria.mode=userver talaria.ip=dhcp"); sleep 5
assert "netsys: net0 dhcp lease 10\.0\.2\.15/24 gw=10\.0\.2\.2 dns=10\.0\.2\.3" "dhcp: the slirp lease (the IP/the gateway/the DNS) has been applied to netsys"
R=$(printf 'PONGPONG' | timeout 3 socat -T2 - udp:127.0.0.1:9998 2>/dev/null)
[ "$R" = "PONGPONG" ] && { echo "  PASS  dhcp: a UDP echo on the DHCP address (hostfwd slirp)"; PASS=$((PASS+1)); } || { echo "  FAIL  dhcp: the UDP echo (got '$R')"; FAIL=$((FAIL+1)); }
stopq $q

# imds: EC2's metadata service (IMDSv2: PUT token -> GET, §3.3) — a mock on 169.254.169.254 of the tap host
ip addr add 169.254.169.254/32 dev tap0 2>/dev/null
cat > /tmp/imds_srv.py <<'PY'
# IMPORTANT: HTTPServer.server_bind() calls socket.getfqdn(host) — a reverse DNS which in
# a container goes nowhere and hangs the process in the D state even BEFORE the bind (the
# test then failed with "there is no answer to the PUT token" — that was its flake).
# So we set the server's name ourselves, without a resolve.
import socketserver
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def _s(self, body):
        self.send_response(200); self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close"); self.end_headers(); self.wfile.write(body.encode())
    def do_PUT(self): self._s("TESTTOKEN123")
    def do_GET(self):
        if self.headers.get("X-aws-ec2-metadata-token") != "TESTTOKEN123": self.send_error(401); return
        if self.path.endswith("user-data"): self._s("talaria.mode=userver talaria.strace=7")
        elif self.path.endswith("instance-id"): self._s("i-0abc123talaria")
        else: self._s("ip-10-0-0-2.ec2.internal")
    def log_message(self, *a): pass
class Srv(HTTPServer):
    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = "imds-mock", 80
Srv(("169.254.169.254", 80), H).serve_forever()
PY
# The warm-up: the FIRST import of http.server in the container costs ~10 s (a cold
# overlayfs cache + the compilation of the .pyc) — without it the mock did not manage to
# come up inside the waiting window, and the test failed with "there is no answer to the PUT token" (that was its flake).
python3 -c 'import http.server, socketserver' 2>/dev/null
python3 /tmp/imds_srv.py >/tmp/imds_srv.log 2>&1 &
IMDS_PID=$!
for _ in $(seq 1 60); do curl -s -m 1 -X PUT http://169.254.169.254/latest/api/token 2>/dev/null | grep -q TESTTOKEN && break; sleep 0.5; done
q=$(boot "talaria.mode=userver talaria.imds=1" net); sleep 3
assert "imds: instance-id=i-0abc123talaria hostname=ip-10-0-0-2" "imds: IMDSv2 (PUT token -> GET instance-id/hostname)"
stopq $q
# §0.1 userdata: a cmdline WITHOUT a mode, the mode arrives from /latest/user-data — one
# AMI for every experiment. We check both the gluing and that the card opens anew after
# the early stack is removed (userver really answers over UDP).
q=$(boot "talaria.userdata=1" net); sleep 6
assert 'user-data -> cmdline \+= "talaria.mode=userver talaria.strace=7"' "userdata: the tail of the cmdline from /latest/user-data has been glued on"
R=$(printf 'UDPUDPUD' | timeout 3 socat -T2 - udp:10.0.0.2:9998 2>/dev/null)
[ "$R" = "UDPUDPUD" ] && { echo "  PASS  userdata: the mode from the user-data has started (a UDP echo after the card is reopened)"; PASS=$((PASS+1)); } || { echo "  FAIL  userdata: the mode from the user-data (got '$R')"; FAIL=$((FAIL+1)); }
stopq $q; kill $IMDS_PID 2>/dev/null

# ntp: the wall clock's reference over the network (talaria.ntp=, kernel/src/ntp.rs). The
# server's clock is the host's moved +3 s and running 150 ppm fast: the guest must take it as
# its base at the boot and then follow it; the judge compares the two from outside (the kernel echo's
# wall-clock stamp against an SNTP answer, both referred to the host's clock), so without the
# feature the guest is 3 s off and the gate fails.
python3 /w/kernel/tools/minintp.py 10.0.0.1 3 150 2>/tmp/ntp.log & NTPP=$!
q=$(boot "talaria.mode=userver talaria.ntp=10.0.0.1,1 talaria.kecho=7777 talaria.kts=1" net)
for _ in $(seq 1 40); do grep -q "ntp: sample 8 " /tmp/o.log 2>/dev/null && break; sleep 0.5; done
assert "ntp: sample 1 from 10\.0\.0\.1: the error \+(29|3[0-9])[0-9]{5} us.* the boot.s base is set onto it" "ntp: the first sample sets the boot's base onto the reference (+3 s)"
assert "ntp: sample 8 from 10\.0\.0\.1: .* steps 0 back 0" "ntp: eight samples, no step, never back"
R=$(python3 /w/kernel/tools/ntpcmp.py 10.0.0.2 7777 10.0.0.1 2>&1); E=$(echo "$R" | sed -n 's/.*reference = \([-+][0-9]*\) us.*/\1/p')
if [ -n "$E" ] && [ "${E#[-+]}" -lt 1000 ]; then echo "  PASS  ntp: the guest's wall clock is ${E} us from the reference, judged from outside"; PASS=$((PASS+1))
else echo "  FAIL  ntp: the guest's wall clock against the reference ($R)"; FAIL=$((FAIL+1)); fi
stopq $q
# A workload that never touches the network does not drive the stack: the client runs from the
# tick then (netsys::ntp_tick). And at the workload's end the supervisor's reset must leave the
# kernel's socket alone — it closed it once, and the next pass panicked in smoltcp (a Graviton).
q=$(bootdn "talaria.mode=cell talaria.ntp=10.0.0.1,1" /img/disk_ntpcell.img)
for _ in $(seq 1 40); do grep -q "supervise: code\|PANIC" /tmp/o.log 2>/dev/null && break; sleep 0.5; done; sleep 1
assert "ntp: sample 4 from 10\.0\.0\.1: .* steps 0 back 0" "ntp: the samples go on under a workload that does not use the network (the tick drives the client)"
refute "PANIC|handle does not refer" "ntp: the supervisor's reset at the workload's end keeps the kernel's SNTP socket"
stopq $q
q=$(boot "talaria.mode=userver talaria.ntp=10.0.0.9,1" net)
assert_slow "netsys: ntp: no sample in 3 s" "ntp: a silent server is said, and the boot goes on"
stopq $q; kill $NTPP 2>/dev/null

# uefi: OVMF (edk2, q35) -> the uefi-stub -> a trampoline -> pvh_start (PLAN-V1.1 §4.1);
# the stub carries the kernel's ELF inside — a .efi FRESHER than the kernel's binary is required (uefi-stub/build.sh)
EFI=kernel/uefi-stub/target/x86_64-unknown-uefi/release/talaria-uefi-stub.efi
if [ -f "/w/$EFI" ] && [ "/w/$EFI" -nt "/w/$K" ]; then
  rm -f /tmp/o.log; cp /usr/share/edk2/ovmf/OVMF_VARS.fd /tmp/vars.fd
  qemu-system-x86_64 -M q35 -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/edk2/ovmf/OVMF_CODE.fd \
    -drive if=pflash,format=raw,file=/tmp/vars.fd \
    -kernel "/w/$EFI" -append "console=ttyS0 talaria.test=musl" \
    -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 & q=$!
  assert_slow "musl static binary -> Talaria OS" "uefi: OVMF -> the uefi-stub -> pvh_start -> musl in ring 3"
  stopq $q
  # §4.2: the topology from the ACPI MADT + SMP under UEFI (it catches the sti/sched::start race too)
  rm -f /tmp/o.log
  qemu-system-x86_64 -M q35 -enable-kvm -cpu "$CPU" -smp 4 -m 512 \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/edk2/ovmf/OVMF_CODE.fd \
    -drive if=pflash,format=raw,file=/tmp/vars.fd \
    -kernel "/w/$EFI" -append "console=ttyS0 talaria.test=smp4" \
    -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 & q=$!
  assert_slow "acpi: MADT cpus=4" "uefi-acpi: the MADT has been parsed (the CPU topology from the ACPI)"
  assert_slow "SMP4: the parallel speedup .* ALL OK" "uefi-smp4: 4 cores under OVMF (the MADT ids, the migration of the threads)"
  stopq $q
  # §4.3: the PCIe enumeration by the ECAM from the MCFG — the NVMe and the virtio-net-pci are visible with MSI-X
  rm -f /tmp/o.log; dd if=/dev/zero of=/tmp/nvme.img bs=1M count=16 2>/dev/null
  qemu-system-x86_64 -M q35 -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/edk2/ovmf/OVMF_CODE.fd \
    -drive if=pflash,format=raw,file=/tmp/vars.fd \
    -kernel "/w/$EFI" -append "console=ttyS0 talaria.test=musl talaria.pci=1" \
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0 \
    -drive id=nv,file=/tmp/nvme.img,format=raw,if=none -device nvme,drive=nv,serial=tal001 \
    -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 & q=$!
  assert_slow "pci: ECAM 0xe0000000 .MCFG." "uefi-pci: the ECAM base from the ACPI MCFG"
  assert_slow "pci: 00:03.0 1b36:0010 class=0x010802 msi-x" "uefi-pci: the NVMe is visible in the enumeration (the class 0x0108, MSI-X)"
  assert_slow "pci: 00:02.0 1af4:1000 class=0x020000 msi-x" "uefi-pci: the virtio-net-pci is visible in the enumeration"
  stopq $q
  # virtio-net over virtio-pci (docs/ARM64-PLAN.md §10.10): the same card DRIVEN, not only
  # seen — the transport finds its registers through the vendor capabilities in the BARs
  # OVMF assigned; a UDP echo through it on the gate's tap.
  rm -f /tmp/o.log
  qemu-system-x86_64 -M q35 -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/edk2/ovmf/OVMF_CODE.fd \
    -drive if=pflash,format=raw,file=/tmp/vars.fd \
    -kernel "/w/$EFI" -append "console=ttyS0 talaria.mode=userver talaria.pci=1" \
    -netdev tap,id=n0,ifname=tap0,script=no,downscript=no -device virtio-net-pci,netdev=n0,mac=52:54:00:12:34:56 \
    -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 & q=$!
  assert_slow "virtio-net: the instance 0 — PCI 00:02.0 1af4:1000 \(the virtio-pci transport" "uefi-virtio-pci: the card is driven over virtio-pci (the BARs OVMF assigned)"
  for _ in $(seq 1 40); do grep -aq 'userver: the UDP echo' /tmp/o.log 2>/dev/null && break; sleep 0.5; done
  R=$(printf 'PCI86PCI' | timeout 3 socat -T2 - udp:10.0.0.2:9998 2>/dev/null)
  [ "$R" = PCI86PCI ] || { sleep 2; R=$(printf 'PCI86PCI' | timeout 3 socat -T2 - udp:10.0.0.2:9998 2>/dev/null); }
  if [ "$R" = PCI86PCI ]; then echo "  PASS  uefi-virtio-pci: the UDP echo through the virtio-pci card"; PASS=$((PASS+1)); else echo "  FAIL  uefi-virtio-pci: the UDP echo (got '$R')"; FAIL=$((FAIL+1)); fi
  stopq $q
  # §4.8: a device BEHIND BRIDGES and on a NON-zero bus — exactly the case on which
  # Talaria did not find the network on a c7i (the card on the bus 0x27, outside the tree
  # of the bus 0's bridges, while a hundred empty bridges pushed the devices out of the
  # table). Here the NVMe stands behind a root port -> an upstream -> a downstream, and
  # what is checked is not only the enumeration but the workability too: ext2 through this controller.
  rm -f /tmp/o.log; cp /img/disk_ext2.img /tmp/nvme_br.img
  qemu-system-x86_64 -M q35 -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/edk2/ovmf/OVMF_CODE.fd \
    -drive if=pflash,format=raw,file=/tmp/vars.fd \
    -kernel "/w/$EFI" -append "console=ttyS0 talaria.test=ext2 talaria.pci=1" \
    -device pcie-root-port,id=rp0,chassis=1 \
    -device x3130-upstream,id=up0,bus=rp0 \
    -device xio3130-downstream,id=dn0,bus=up0,chassis=10 \
    -device xio3130-downstream,id=dn1,bus=up0,chassis=11 \
    -drive id=nv,file=/tmp/nvme_br.img,format=raw,if=none -device nvme,drive=nv,serial=tal001,bus=dn0 \
    -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 & q=$!
  assert_slow "pci: 0[1-9a-f]:00.0 1b36:0010 class=0x010802" "uefi-pci: a device behind the bridges has been found on a NON-zero bus (the c7i case)"
  assert_slow "bridges skipped [1-9]" "uefi-pci: the bridges do not get into the table of the devices (they do not push the real ones out)"
  assert_slow "ext2-test: ALL OK" "uefi-pci: the NVMe behind three bridges WORKS (ext2 RW)"
  stopq $q
  # §4.5: the NVMe driver (admin+IO qid=1, polling) — ext2 RW from an NVMe disk and
  # the persistence across a reboot (the same copy of the image, two boots)
  cp /img/disk_ext2.img /tmp/nvme_e2.img
  bootnvme() {
    rm -f /tmp/o.log
    qemu-system-x86_64 -M q35 -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
      -drive if=pflash,format=raw,readonly=on,file=/usr/share/edk2/ovmf/OVMF_CODE.fd \
      -drive if=pflash,format=raw,file=/tmp/vars.fd \
      -kernel "/w/$EFI" -append "console=ttyS0 $1 talaria.pci=1" \
      -drive id=nv,file=/tmp/nvme_e2.img,format=raw,if=none -device nvme,drive=nv,serial=tal001 \
      -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 & echo $!
  }
  q=$(bootnvme "talaria.test=ext2"); sleep 1
  assert_slow "nvme: 00:03.0 BAR0=.* polling" "uefi-nvme: the controller is up (admin+IO qid=1, polling)"
  assert_slow "ext2-test: ALL OK" "uefi-nvme: ext2 RW through the NVMe driver (EC2's EBS path)"
  stopq $q
  # §4.1 (A2): a batch of writes on NVMe. A range longer than the controller's MDTS
  # (128 KB) went out as ONE command and was rejected — on a live EBS and on QEMU's NVMe
  # alike; it surfaced only after §2.4, when the files started lying down contiguously.
  cp /img/disk_fsb.img /tmp/nvme_fsb.img
  rm -f /tmp/o.log
  qemu-system-x86_64 -M q35 -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
    -drive if=pflash,format=raw,readonly=on,file=/usr/share/edk2/ovmf/OVMF_CODE.fd \
    -drive if=pflash,format=raw,file=/tmp/vars.fd \
    -kernel "/w/$EFI" -append "console=ttyS0 talaria.test=fsbench talaria.pci=1" \
    -drive id=nv,file=/tmp/nvme_fsb.img,format=raw,if=none -device nvme,drive=nv,serial=tal001 \
    -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 & q=$!
  assert_slow "fsbench: ALL OK" "uefi-nvme: fsbench through NVMe (the EBS path)"
  # The same for NVMe: a CID instead of a token, "submit" and "wait for YOUR OWN" are
  # separated. Everything was done for this number's sake — on EBS a barrier costs tens of
  # milliseconds, and with a depth of one a foreign read would stand behind it instead of flying past.
  assert "fsbench: the queue: [0-9]+ commands submitted, the greatest depth ([2-9]|[1-9][0-9]+)" "uefi-nvme: the NVMe queue has a DEPTH (a table of the commands in flight by CID)"
  # SESSION 28: ONE barrier per journal transaction rather than two. The second was needed
  # for the sake of a single block — to make the commit durable; on NVMe that is done by
  # the FUA (the same saving as in jbd2). The gate tells them apart: on virtio-blk the same
  # test case prints "the barriers 2", on NVMe it must print "the barriers 1".
  assert "fsbench: the disk: the blocks [0-9]+ the ranges [0-9]+ the barriers 1 " "uefi-nvme: a journal transaction costs ONE barrier (the commit block is written with FUA)"
  if grep -qa "an error of the group write" /tmp/o.log; then
    echo "  FAIL  uefi-nvme: a batch of writes longer than the MDTS was rejected by the controller"; FAIL=$((FAIL+1))
  else echo "  PASS  uefi-nvme: a batch of writes is cut by the controller's transfer limit (the MDTS)"; PASS=$((PASS+1)); fi
  stopq $q
  q=$(bootnvme "talaria.test=ext2v"); sleep 1
  assert_slow "ext2v: .*PERSISTED OK" "uefi-nvme: the data survived a reboot (the NVMe write-back + an fsync)"
  stopq $q
else echo "  SKIP  uefi: there is no fresh talaria-uefi-stub.efi (kernel/uefi-stub/build.sh)"; fi

# bios: a HYBRID image through SeaBIOS — without -kernel and without OVMF (§3.2/§3.3).
# This is the only path to native bare metal on EC2: a *.metal does not accept an AMI with
# the boot-mode uefi. The chain: the MBR -> core.img in the gap -> GRUB -> multiboot2 ->
# mb2_start -> the common PVH path; the kernel finds the data partition by the NAME talaria-data.
if [ -f "/img/disk_hyb.img" ]; then
  rm -f /tmp/o.log
  timeout 90 qemu-system-x86_64 -M q35 -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
    -drive file=/img/disk_hyb.img,format=raw,if=none,id=nv -device nvme,drive=nv,serial=tal001 \
    -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 &
  q=$!
  assert "multiboot2: the MBI .* -> the start_info" "bios: GRUB -> multiboot2 -> the kernel (a BIOS boot, without UEFI)"
  assert "GPT, the data partition \"talaria-data\"" "bios: the data partition has been found by its name in the GPT of the hybrid image"
  assert "ext2-test: ALL OK" "bios: ext2 on the data partition through NVMe (the BIOS path)"
  stopq $q
else echo "  SKIP  bios: there is no /img/disk_hyb.img (tools/mk-bios.sh — sgdisk/mtools/docker are needed)"; fi
# §1.3: a PURE MBR image (without a GPT and without an ESP) — the second mode of
# mk-bios.sh. Earlier only the hybrid lived in the regression while this path was checked
# by hand; it is also the one that goes to a *.metal with BOOTMODE=legacy-bios, so its breakage must be caught at home.
if [ -f "/img/disk_mbr.img" ]; then
  rm -f /tmp/o.log
  timeout 90 qemu-system-x86_64 -M q35 -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
    -drive file=/img/disk_mbr.img,format=raw,if=none,id=nv -device nvme,drive=nv,serial=tal002 \
    -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 &
  q=$!
  assert "multiboot2: the MBI .* -> the start_info" "bios-mbr: GRUB -> multiboot2 -> the kernel on an image WITHOUT a GPT"
  assert "ext2-test: ALL OK" "bios-mbr: ext2 on the data partition (an MBR layout)"
  stopq $q
else echo "  SKIP  bios-mbr: there is no /img/disk_mbr.img"; fi
# §1 (A1): a "TALKING" MBR marker (tools/mbr-marker.sh CHAIN=1) — the instrument of the
# *.metal diagnostics: it prints a line into COM1 with the FIRST instruction and hands
# control over to the original sector 0 (kept in LBA 34). At home it is checked in full:
# both that the marker sounds and that the boot after it does NOT break — otherwise a
# probe on metal (15 minutes and ~1.5 $) interprets its own breakage as a result.
if [ -f "/img/disk_chain.img" ]; then
  rm -f /tmp/o.log
  timeout 90 qemu-system-x86_64 -M q35 -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
    -drive file=/img/disk_chain.img,format=raw,if=none,id=nv -device nvme,drive=nv,serial=tal003 \
    -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 &
  q=$!
  assert "TALARIA-MBR-CHAIN-OK" "mbr-chain: the marker is printed from the sector 0 by the first instruction"
  # assert_slow rather than assert: the path is long (the marker -> GRUB -> a read of 29 MB
  # of the kernel from NVMe), and on a busy host it did not fit into 6 seconds — the gate flaked.
  assert_slow "multiboot2: the MBI .* -> the start_info" "mbr-chain: after the marker the regular chain boots (the sector 0 from the spare LBA)"
  stopq $q
else echo "  SKIP  mbr-chain: there is no /img/disk_chain.img"; fi
# §8 (A1): OUR OWN loader instead of GRUB. It reads the kernel from the disk itself
# (int 13h), takes the memory map (int 15h E820), assembles a multiboot2 and goes into
# mb2_start. All three links are checked at once: the entry into the kernel, THE MEMORY MAP
# (without it the kernel panics with "there is no memmap" — that is how the first mistake
# in the assembly of the tags was caught) and the data partition found by the MBR table of our image.
if [ -f "/img/disk_tboot.img" ]; then
  rm -f /tmp/o.log
  timeout 90 qemu-system-x86_64 -M q35 -enable-kvm -cpu "$CPU" -smp 1 -m 1024 \
    -drive file=/img/disk_tboot.img,format=raw,if=none,id=nv -device nvme,drive=nv,serial=tal004 \
    -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 &
  q=$!
  assert "multiboot2: the MBI .* the map has [1-9][0-9]* records" "tboot: our own loader -> mb2_start, the E820 memory map arrived"
  assert "MBR, the data partition from LBA" "tboot: the data partition has been found by our image's table"
  # A9: the block layer comes up BEFORE the ACPI and the PCI enumeration (otherwise on
  # metal there is nothing to write the trace from if the kernel stands in them)
  assert "talaria-trace: phase the early disk" "trace: the disk comes up before ACPI/PCI (the early trace)"
  # A9: on the BIOS path the RSDP address comes from nowhere (we do not assemble the tags
  # 14/15), and until session 9 there simply was no ACPI there: neither a MADT (SMP by
  # invented ids) nor an MCFG (the ECAM blindly). Now the kernel looks for the RSDP itself, like any BIOS OS.
  assert "acpi: the RSDP was found by scanning" "tboot: the RSDP has been found by scanning the EBDA/the ROM (the BIOS path)"
  assert "acpi: MADT cpus=[1-9]" "tboot: the MADT has been parsed — the topology of the cores is the real one"
  assert "pci: ECAM .* \(MCFG\)" "tboot: the ECAM is taken from the MCFG rather than guessed"
  assert "ext2-test: ALL OK" "tboot: ext2 on the data partition through NVMe (a boot without GRUB)"
  # The mode's line appears EARLIER than the last flush of the trace reaches the disk: the
  # former version killed QEMU right at it and caught "there is no summary of the mode in
  # the log" every other time. We wait for the phase's mark itself and let the write get there.
  assert "talaria-trace: phase ext2-test finished" "trace: the last phase is marked (a flush to disk after the mode)"
  stopq $q
  # A13: the same BIOS path on a LARGE memory (6 GB). The point: with memory above the PCI
  # hole the E820 map gets areas BEYOND 4 GB, and everything the loader and the kernel
  # assumed about a "small" map breaks exactly here. Without this check the home stand
  # showed only 512-1024 MB, that is, the class of mistakes was not visible at all.
  # The disk is attached as an ide-hd rather than as an nvme, and that is not a whim:
  # SeaBIOS on a q35 does not find a bootable NVMe with more than ~2.75 GB of memory
  # ("Booting from ROM... No bootable device" in its own log), and the run would look like
  # "our loader broke on a large memory". It is a limitation of QEMU's firmware, not ours:
  # on an i440fx the same image with an nvme boots, on metal all the more so.
  # The devices' roles are SEPARATED, and that is not a trick but a consequence of two limitations:
  #   * SeaBIOS does not find a bootable NVMe as soon as QEMU splits the memory above
  #     4 GB (on a q35 that is already from ~2.75 GB, on an i440fx from 4 GB);
  #   * on the BIOS path the kernel drives exactly the NVMe.
  # So ONE AND THE SAME image is attached twice: a virtio-blk as the firmware's boot device
  # (int 13h reads the kernel from it) and an NVMe as the kernel's working disk. As a bonus
  # the NVMe gets a 64-bit BAR ABOVE 4 GB — the very case for which map_mmio_1g exists in
  # the kernel, and which otherwise does not reproduce at home.
  # THERE ARE TWO ATTEMPTS, and that is not a softening of the gate but an acknowledgement
  # of a fact: this boot is THE ONLY one in the whole set where QEMU is asked for six
  # gigabytes right after the previous machine has given them back. In about half of the
  # runs SeaBIOS at such a moment does not even reach the read of the kernel: one marker
  # "TT" of our loader stays in the serial, and four checks fall out of the blue (the same
  # boot on its own passes in a second). The repeat is visible in the output, so a REAL
  # breakage is still noticeable: it is not cured by a second attempt.
  boot6g() {
    rm -f /tmp/o.log
    timeout 120 qemu-system-x86_64 -M q35 -enable-kvm -cpu "$CPU" -smp 1 -m 6144 \
      -drive file=/img/disk_tboot6g.img,format=raw,if=none,id=b0,file.locking=off \
      -device virtio-blk-pci,drive=b0,bootindex=0 \
      -drive file=/img/disk_tboot6g.img,format=raw,if=none,id=n0,file.locking=off \
      -device nvme,drive=n0,serial=tal006 \
      -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 & echo $!
  }
  q=$(boot6g)
  # We wait for the FIRST sign of the kernel's life; if it does not come — we put it out and try again.
  ok6g=0
  for _ in $(seq 1 40); do grep -qE "multiboot2: the MBI" /tmp/o.log 2>/dev/null && { ok6g=1; break; }; sleep 0.5; done
  if [ $ok6g = 0 ]; then
    echo "  NOTE  tboot-6g: the first attempt did not reach the kernel (in the serial: $(tr -d '\r\n' < /tmp/o.log | cut -c1-40)); qemu: $(tail -1 /tmp/q.log 2>/dev/null | cut -c1-80). A repeat."
    stopq $q; sleep 1; q=$(boot6g)
  fi
  assert_slow "multiboot2: the MBI .* the map has [1-9][0-9]* records" "tboot-6g: our own loader assembled the E820 map on 6 GB (the areas beyond 4 GB)"
  assert_slow "RAM in total 6[0-9]{3} MB" "tboot-6g: the kernel saw all the memory (a map with areas beyond 4 GB has been parsed)"
  assert_slow "nvme: .* BAR0=0x3[0-9a-f]{11}" "tboot-6g: the device's 64-bit BAR ABOVE 4 GB has been mapped (map_mmio_1g)"
  assert_slow "ext2-test: ALL OK" "tboot-6g: the FS works on a large memory (the whole BIOS path)"
  sleep 3
  stopq $q
  # The trace on the disk is the very channel by which a boot on a *.metal is measured.
  TB=$(python3 -c "
import struct
f=open('/img/disk_tboot.img','rb'); f.seek(2047*512); t=f.read(22)
print(t[:11].decode('ascii','replace'), t[11], 'boots', struct.unpack('<I', t[18:22])[0])" 2>/dev/null)
  case "$TB" in "TBOOT-TRACE 4 boots"*) echo "  PASS  tboot: the trace on the disk reached the phase 4 (the move into protected mode)"; PASS=$((PASS+1));;
    *) echo "  FAIL  tboot: the trace on the disk = '$TB' (we expected 'TBOOT-TRACE 4')"; FAIL=$((FAIL+1));; esac
  # A9: the counter of the boots in the trace. On bare metal the console is a screenshot,
  # and a hang of the kernel there is indistinguishable from an eternal reboot; the counter
  # on the disk tells them apart unambiguously. One boot of the image = exactly 1.
  case "$TB" in *"boots 1") echo "  PASS  tboot: the counter of the boots in the trace = 1 (it tells a hang from a loop of reboots)"; PASS=$((PASS+1));;
    *) echo "  FAIL  tboot: the counter of the boots: '$TB' (we expected boots 1)"; FAIL=$((FAIL+1));; esac
  # A9: THE KERNEL'S OWN LOG on the disk. The header is in the sector 2048, the text
  # further on; we check both that it has been written at all and that it has the last
  # phase in it (that is, the flushes went along the way of the boot rather than once at the end).
  KL=$(python3 -c "
f=open('/img/disk_tboot.img','rb'); f.seek(2048*512)
h=f.read(512).split(b'\x00')[0].decode('utf-8','replace')
d=f.read(65536).decode('utf-8','replace')
print(h.strip())
print('MARKS', d.count('talaria-trace: phase'))
print('EXT2OK', 'ext2-test: ALL OK' in d)
print('BOOTHEAD', 'talaria: boot' in d)" 2>/dev/null)
  case "$KL" in *"TALARIA-TRACE len="*) echo "  PASS  trace: the kernel's log has landed on the disk (the header: $(echo "$KL" | head -1))"; PASS=$((PASS+1));;
    *) echo "  FAIL  trace: there is no TALARIA-TRACE header in the sector 1024"; FAIL=$((FAIL+1));; esac
  case "$KL" in *"BOOTHEAD True"*) echo "  PASS  trace: the log has the VERY BEGINNING of the boot in it (the boot line)"; PASS=$((PASS+1));;
    *) echo "  FAIL  trace: the log has no beginning of the boot"; FAIL=$((FAIL+1));; esac
  case "$KL" in *"EXT2OK True"*) echo "  PASS  trace: the log has been brought to the end of the mode (ext2-test: ALL OK on the disk)"; PASS=$((PASS+1));;
    *) echo "  FAIL  trace: the log on the disk has no summary of the mode"; FAIL=$((FAIL+1));; esac
  M=$(echo "$KL" | sed -n 's/^MARKS //p')
  [ "${M:-0}" -ge 4 ] 2>/dev/null && { echo "  PASS  trace: the phases marked $M (the flushes went along the way of the boot)"; PASS=$((PASS+1)); } \
    || { echo "  FAIL  trace: the phases in the log $M (we expected >= 4)"; FAIL=$((FAIL+1)); }
else echo "  SKIP  tboot: there is no /img/disk_tboot.img (tools/mk-tboot.sh)"; fi
# A9: a SET of modes on one image (talaria.suite=). The kernel takes its item from the
# list on the disk, writes the log into its own slot and reboots the machine for the next.
# The gate is a home one because outside it is an instrument for a *.metal, where one
# launch of an instance costs minutes of POST and money: it must break here.
if [ -f "/img/disk_suite.img" ]; then
  rm -f /tmp/o.log
  # WITHOUT -no-reboot: the reboot between the items is part of the mechanism
  timeout 200 qemu-system-x86_64 -M q35 -enable-kvm -cpu "$CPU" -smp 1 -m 1024 \
    -drive file=/img/disk_suite.img,format=raw,if=none,id=nv -device nvme,drive=nv,serial=tal005 \
    -display none -serial file:/tmp/o.log >/tmp/q.log 2>&1 &
  q=$!
  assert "suite: run 1 of 2" "suite: the first item of the list has been taken from the disk"
  # Between the items there is the whole first mode, a reboot and a repeated boot of the
  # kernel; the 20-second window of an assert is not enough for that every other time (the gate flaked).
  sleep 20
  assert "suite: run 2 of 2" "suite: the second item — after a reboot of its own"
  assert "wc-test: ALL OK" "suite: the second item has worked to the end (its mode has reported)"
  assert "sync: a woken one preempts a computing one after 12345 ns" "suite: a TUNING knob from the item's tail has been APPLIED (until session 28 the tail was read only for the network and the \"early\" options, while stat=/wakepreempt=/netslice= from the user-data stayed silent)"
  sleep 3                      # the last flush of the trace has to reach the disk
  stopq $q
  SU=$(python3 -c "
f=open('/img/disk_suite.img','rb')
def slot(i):
    f.seek((2048+i*129)*512); f.read(512)
    return f.read(65536).split(b'\x00')[0].decode('utf-8','replace')
f.seek(1004*512); st=f.read(24)
print('IDX', int.from_bytes(st[16:20],'little'), 'N', int.from_bytes(st[20:24],'little'))
print('S1', 'ext2-test: ALL OK' in slot(1))
print('S2', 'wc-test: ALL OK' in slot(2))" 2>/dev/null)
  case "$SU" in *"S1 True"*) echo "  PASS  suite: the log of the first item has landed in its own slot (ext2 ALL OK)"; PASS=$((PASS+1));;
    *) echo "  FAIL  suite: there is no summary of the first item in the slot 1"; FAIL=$((FAIL+1));; esac
  case "$SU" in *"S2 True"*) echo "  PASS  suite: the log of the second item is in ITS OWN slot (wc ALL OK, the first has not been overwritten)"; PASS=$((PASS+1));;
    *) echo "  FAIL  suite: there is no summary of the second item in the slot 2"; FAIL=$((FAIL+1));; esac
  case "$SU" in *"IDX 2 N 2"*) echo "  PASS  suite: the counter on the disk reached 2 of 2"; PASS=$((PASS+1));;
    *) echo "  FAIL  suite: the counter on the disk: $(echo "$SU" | head -1)"; FAIL=$((FAIL+1));; esac
else echo "  SKIP  suite: there is no /img/disk_suite.img"; fi

# filed: the uploading/downloading of files over TCP (a likeness of rsync). What is checked
# is not "the bytes arrived" but THE MAIN PROPERTY: a file that matches is not transferred
# at all, while a changed one goes as one block rather than in full.
if [ -f /img/disk_filed.img ]; then
  # The token is given EXPLICITLY on both sides: the service has no default value.
  FTOK=regress-filed-token
  q=$(bootdn "talaria.mode=filed talaria.filedtoken=$FTOK" /img/disk_filed.img); sleep 3
  assert "filed: the receiving of files on tcp://10\.0\.0\.2:7373" "filed: the uploading service is up on the node (the default port)"
  # The token is not printed: the serial on EC2 is read through get-console-output.
  if grep -aq "$FTOK" /tmp/o.log; then echo "  FAIL  filed: the token has been printed in the serial in clear text"; FAIL=$((FAIL+1));
  else echo "  PASS  filed: the token in the serial is masked (the cmdline and the knobs' audit print ***)"; PASS=$((PASS+1)); fi
  TF=/w/kernel/tools/talaria-file
  export TALARIA_TOKEN=$FTOK
  head -c 200000 /dev/urandom > /tmp/big.bin
  P1=$(python3 $TF 10.0.0.2 push /tmp/big.bin /big.bin 2>&1)
  case "$P1" in *"uploaded whole"*) echo "  PASS  filed: PUT — a file of 200 KB has been uploaded onto the node"; PASS=$((PASS+1));;
    *) echo "  FAIL  filed: PUT ($P1)"; FAIL=$((FAIL+1));; esac
  P2=$(python3 $TF 10.0.0.2 push /tmp/big.bin /big.bin 2>&1)
  case "$P2" in *"identical, not sent"*) echo "  PASS  filed: a repeated upload of a file that matches transfers NOTHING (STAT+CRC32)"; PASS=$((PASS+1));;
    *) echo "  FAIL  filed: a file that matches has been transferred all the same ($P2)"; FAIL=$((FAIL+1));; esac
  python3 -c "
d=bytearray(open('/tmp/big.bin','rb').read()); d[100000:100010]=b'XXXXXXXXXX'
open('/tmp/big.bin','wb').write(bytes(d))"
  P3=$(python3 $TF 10.0.0.2 push /tmp/big.bin /big.bin 2>&1)
  SENT=$(echo "$P3" | sed -n 's/.*sent \([0-9]*\) of.*/\1/p')
  if [ -n "$SENT" ] && [ "$SENT" -gt 0 ] && [ "$SENT" -lt 40000 ]; then
    echo "  PASS  filed: an edit of 10 bytes goes as ONE block ($SENT B out of 200000) — the block-by-block comparison of SUMS works"; PASS=$((PASS+1))
  else echo "  FAIL  filed: the delta transferred '$SENT' B ($P3)"; FAIL=$((FAIL+1)); fi
  python3 $TF 10.0.0.2 pull /big.bin /tmp/back.bin >/dev/null 2>&1
  if cmp -s /tmp/big.bin /tmp/back.bin; then echo "  PASS  filed: GET — the downloaded file is byte-for-byte equal to the original (the CRC32 has been compared)"; PASS=$((PASS+1));
  else echo "  FAIL  filed: the downloaded file differs from the original"; FAIL=$((FAIL+1)); fi
  BAD=$(TALARIA_TOKEN=wrong python3 $TF 10.0.0.2 ls / 2>&1)
  case "$BAD" in *"authentication rejected"*) echo "  PASS  filed: a wrong token -> a refusal (the service writes onto the disk, access without a token is impossible)"; PASS=$((PASS+1));;
    *) echo "  FAIL  filed: a wrong token has been let through ($BAD)"; FAIL=$((FAIL+1));; esac
  # A SERIES of connect+close WITHOUT an accept (a FIN and an RST — a dozen of each with a
  # backlog of 8). Earlier such a backlog slot died for ever: the accept did not take it,
  # arm considered it raised, and after eight the listener silently stopped answering SYNs —
  # on EC2 "the stack stops accepting after ~4.6 thousand connections" (netsys::reap_backlog).
  CH=$(python3 -c "
import socket, struct
for mode in ('fin','rst'):
    for i in range(12):
        s=socket.socket(); s.settimeout(3)
        try: s.connect(('10.0.0.2',7373))
        except Exception as e: print('refused', mode, i, e); raise SystemExit
        if mode=='rst': s.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack('ii',1,0))
        s.close()
print('24 ok')" 2>&1)
  case "$CH" in *"24 ok"*) echo "  PASS  filed: 24 connect+close without an accept (a FIN and an RST) — the listener answers every SYN"; PASS=$((PASS+1));;
    *) echo "  FAIL  filed: the listener stopped answering during the series of connect+close ($CH)"; FAIL=$((FAIL+1));; esac
  sleep 1
  AFTER=$(timeout 10 python3 $TF 10.0.0.2 ls / 2>&1)
  case "$AFTER" in *"big.bin"*) echo "  PASS  filed: after the series a connection IS ACCEPTED and served (the backlog has returned into Listen)"; PASS=$((PASS+1));;
    *) echo "  FAIL  filed: after the series of connect+close the service does not accept ($(echo "$AFTER" | tail -1))"; FAIL=$((FAIL+1));; esac
  stopq $q
  # A node WITHOUT talaria.filedtoken=: there is no default password, and nobody is let in —
  # neither the former token hardwired into the sources nor an empty one. Earlier a service
  # THAT CAN WRITE FILES listened on TCP with a password from an open repository.
  q=$(bootdn "talaria.mode=filed" /img/disk_filed.img); sleep 3
  assert "talaria.filedtoken= is not set — the service will refuse all the commands" "filed: a node without a token warns at the start-up"
  NOTOK=$(TALARIA_TOKEN=talaria-secret python3 $TF 10.0.0.2 ls / 2>&1)
  case "$NOTOK" in *"authentication rejected"*) echo "  PASS  filed: a node without talaria.filedtoken= rejects the former hardwired token too (there is NO default password)"; PASS=$((PASS+1));;
    *) echo "  FAIL  filed: a node without a token let somebody in ($NOTOK)"; FAIL=$((FAIL+1));; esac
  stopq $q
  unset TALARIA_TOKEN
else echo "  SKIP  filed: there is no /img/disk_filed.img"; fi

# V1.11: TWO CARDS in one machine. What is checked is not "the list of the interfaces" but
# what it is done for: every card answers on ITS OWN wire, the listener accepts connections
# on both, the routing table lays the subnets out over the interfaces. The second wire is
# tap1 (10.0.2.0/24).
ip tuntap add tap1 mode tap 2>/dev/null
ip addr add 10.0.2.1/24 dev tap1 2>/dev/null; ip link set tap1 up 2>/dev/null
if [ -f /img/disk_filed.img ]; then
  rm -f /tmp/o.log
  cp /img/disk_filed.img /tmp/twonic.img
  qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
    -kernel "/w/$K" -display none -serial file:/tmp/o.log -no-reboot \
    -append "console=ttyS0 talaria.mode=filed talaria.filedtoken=regress-filed-token talaria.ip=0.0.0.0 talaria.net=52-54-00-12-34-56:10.0.0.2/24 talaria.net=52-54-00-12-34-57:10.0.2.2/24 talaria.route=10.9.0.0/16,10.0.2.1,net1" \
    -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
    -netdev tap,id=n1,ifname=tap1,script=no,downscript=no,vhost=on -device virtio-net-device,netdev=n1,mac=52:54:00:12:34:57 \
    -drive id=d0,file=/tmp/twonic.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & q=$!
  sleep 3
  assert "netsys: the interface net1 on the card" "netif: the machine's second card is up as a separate interface (earlier it did not exist for the OS)"
  A0=$(timeout 4 arping -c 2 -I tap0 10.0.0.2 2>/dev/null | grep -c "Unicast reply")
  A1=$(timeout 4 arping -c 2 -I tap1 10.0.2.2 2>/dev/null | grep -c "Unicast reply")
  [ "$A0" -ge 1 ] && [ "$A1" -ge 1 ] \
    && { echo "  PASS  netif: BOTH cards answer on their own wire (each one's address went into its own smoltcp table)"; PASS=$((PASS+1)); } \
    || { echo "  FAIL  netif: not both cards answered (tap0=$A0 tap1=$A1)"; FAIL=$((FAIL+1)); }
  TF=/w/kernel/tools/talaria-file
  export TALARIA_TOKEN=regress-filed-token
  S0=$(timeout 6 python3 $TF 10.0.0.2 stat /hello.txt 2>&1 | tail -1)
  S1=$(timeout 6 python3 $TF 10.0.2.2 stat /hello.txt 2>&1 | tail -1)
  case "$S0$S1" in *size*size*) echo "  PASS  netif: ONE listener accepts connections on BOTH interfaces (the backlog is laid out over the cards)"; PASS=$((PASS+1));;
    *) echo "  FAIL  netif: the listener does not answer on both (net0='$S0' net1='$S1')"; FAIL=$((FAIL+1));; esac
  stopq $q
  assert "10\.9\.0\.0" "netif: a route into a foreign subnet through the second interface has been accepted by the table (talaria.route=)"
else echo "  SKIP  netif: there is no /img/disk_filed.img"; fi

# smoltcp: a UDP echo :7777
q=$(boot "talaria.mode=smoltcp" net); sleep 4
R=$(printf 'PINGPONG' | timeout 3 socat -T2 - udp:10.0.0.2:7777 2>/dev/null)
[ "$R" = "PINGPONG" ] && { echo "  PASS  smoltcp: the UDP echo returned the payload"; PASS=$((PASS+1)); } || { echo "  FAIL  smoltcp: the UDP echo (got '$R')"; FAIL=$((FAIL+1)); }
stopq $q

# httpd: a kernel-mode HTTP + keep-alive (3 requests in ONE connection -> 3 answers)
q=$(boot "talaria.mode=httpd" net); sleep 4
KA=$(printf 'GET /1 HTTP/1.1\r\nHost: x\r\n\r\nGET /2 HTTP/1.1\r\nHost: x\r\n\r\nGET /3 HTTP/1.1\r\nHost: x\r\n\r\n' | timeout 4 socat -T2 - tcp:10.0.0.2:80 2>/dev/null | grep -c "200 OK")
[ "$KA" = 3 ] && { echo "  PASS  httpd: a kernel HTTP + keep-alive (3/3 in one connection)"; PASS=$((PASS+1)); } || { echo "  FAIL  httpd: keep-alive ($KA/3)"; FAIL=$((FAIL+1)); }
stopq $q

# A3.1: the control plane over TCP — a correct token -> 200+the telemetry, otherwise -> 401
# (one keep-alive connection right after the boot: both requests, without a re-listen race)
CTOK=regress-ctl-token
q=$(boot "talaria.mode=httpd talaria.ctltoken=$CTOK" net); sleep 4
CTL=$(printf "GET /ctl?token=$CTOK HTTP/1.1\r\nHost: x\r\n\r\nGET /ctl?token=wrong HTTP/1.1\r\nHost: x\r\n\r\n" | timeout 4 socat -T2 - tcp:10.0.0.2:80 2>/dev/null)
COK=$(echo "$CTL" | grep -c "control-plane OK"); CNO=$(echo "$CTL" | grep -c "401 Unauthorized")
[ "$COK" -ge 1 ] && [ "$CNO" -ge 1 ] && { echo "  PASS  ctlplane-tcp: /ctl a token -> 200+the telemetry, a wrong one -> 401 (A3.1)"; PASS=$((PASS+1)); } || { echo "  FAIL  ctlplane-tcp: ok=$COK deny=$CNO"; FAIL=$((FAIL+1)); }
stopq $q
# A node WITHOUT talaria.ctltoken=: the route is closed entirely. Earlier the token was a
# constant in the sources, that is, every build would have gone into the world with one password.
q=$(boot "talaria.mode=httpd" net); sleep 4
CTL0=$(printf 'GET /ctl?token=talaria-secret HTTP/1.1\r\nHost: x\r\n\r\n' | timeout 4 socat -T2 - tcp:10.0.0.2:80 2>/dev/null)
echo "$CTL0" | grep -q "401 Unauthorized" && echo "$CTL0" | grep -qv "control-plane OK" \
  && { echo "  PASS  ctlplane-tcp: without talaria.ctltoken= the route /ctl is closed — the former hardwired token gets a 401 (there is NO default password)"; PASS=$((PASS+1)); } \
  || { echo "  FAIL  ctlplane-tcp: a node without a token answered '$(echo "$CTL0" | head -c 60)'"; FAIL=$((FAIL+1)); }
stopq $q

# server: a musl userspace TCP server + fault injection
q=$(boot "talaria.mode=server" net); sleep 5
R=$(printf 'GET / HTTP/1.0\r\n\r\n' | timeout 3 socat -T3 - tcp:10.0.0.2:80 2>/dev/null)
echo "$R" | grep -q "Talaria userspace server" && { echo "  PASS  server: a musl ring-3 server"; PASS=$((PASS+1)); } || { echo "  FAIL  server (got '$(echo $R|head -c40)')"; FAIL=$((FAIL+1)); }

echo "== the concurrency (a burst of 6 in parallel) =="
rm -f /tmp/r.*; PIDS=""
for i in 1 2 3 4 5 6; do
  ( printf 'GET /b%s HTTP/1.0\r\n\r\n' $i | timeout 6 socat -T5 - tcp:10.0.0.2:80 2>/dev/null > /tmp/r.$i ) & PIDS="$PIDS $!"
done
for p in $PIDS; do wait $p; done
OK=0; for i in 1 2 3 4 5 6; do grep -q "Talaria userspace server" /tmp/r.$i 2>/dev/null && OK=$((OK+1)); done
[ $OK = 6 ] && { echo "  PASS  concurrency: 6/6 in parallel have been served"; PASS=$((PASS+1)); } || { echo "  FAIL  concurrency: $OK/6"; FAIL=$((FAIL+1)); }

echo "== the fault injection (the server must survive) =="
# 1) a partial request without a termination + an abrupt close
printf 'GET /partial' | timeout 2 socat -T1 - tcp:10.0.0.2:80 >/dev/null 2>&1
# 2) 512 bytes of binary garbage
head -c 512 /dev/urandom | timeout 2 socat -T1 - tcp:10.0.0.2:80 >/dev/null 2>&1
# 3) a connect + an instant close (without data)
timeout 1 socat -T1 /dev/null tcp:10.0.0.2:80 >/dev/null 2>&1
# 4) a series of quick connects
for _ in 1 2 3 4 5; do timeout 1 socat -T1 /dev/null tcp:10.0.0.2:80 >/dev/null 2>&1; done
# after the chaos — a normal request MUST go through
R2=$(printf 'GET /ok HTTP/1.0\r\n\r\n' | timeout 3 socat -T3 - tcp:10.0.0.2:80 2>/dev/null)
echo "$R2" | grep -q "Talaria userspace server" && { echo "  PASS  fault: the server survived and served a request after the chaos"; PASS=$((PASS+1)); } || { echo "  FAIL  fault: the server hung/died (got '$(echo $R2|head -c40)')"; FAIL=$((FAIL+1)); }
stopq $q

echo "== the epoll server (an event loop in one thread) =="
q=$(boot "talaria.mode=eserver" net); sleep 5
rm -f /tmp/e.*; PIDS=""
for i in 1 2 3 4 5 6; do
  ( printf 'GET /e%s HTTP/1.0\r\n\r\n' $i | timeout 6 socat -T5 - tcp:10.0.0.2:80 2>/dev/null > /tmp/e.$i ) & PIDS="$PIDS $!"
done
for p in $PIDS; do wait $p; done
OK=0; for i in 1 2 3 4 5 6; do grep -q "Talaria epoll server" /tmp/e.$i 2>/dev/null && OK=$((OK+1)); done
[ $OK = 6 ] && { echo "  PASS  epoll: 6/6 through the event loop"; PASS=$((PASS+1)); } || { echo "  FAIL  epoll: $OK/6"; FAIL=$((FAIL+1)); }
stopq $q

# THE CEILING OF THE SOCKET TABLE AND THE RETURN FROM IT. 64 slots per machine are the only
# hard limit on the number of connections, and before this fix hitting it was
# IRREVERSIBLE: `arm()` was called only by `listen()` and `accept4()`, while at the moment
# of the hit there are no free slots and there will never be a second reason to call
# `arm()` — `accept4` is called only when a ready connection already lies there, and there is nowhere for it to lie.
# The listener stayed without a backlog for ever: the server is alive, serves the old
# connections and answers with a refusal to EVERY new SYN, even when all the connections
# are closed. On EC2 this was read two runs in a row as "after an overload the server
# stops accepting connections".
q=$(boot "talaria.mode=eserver" net); sleep 5
SLOT=$(python3 -c "
import socket, time
def wave(n, keep):
    ok = 0
    for i in range(n):
        s = socket.socket(); s.settimeout(2)
        try:
            s.connect(('10.0.0.2', 80))
            s.sendall(b'GET /h%d HTTP/1.1\r\nHost: x\r\n\r\n' % i)
            keep.append(s); ok += 1
        except OSError: pass
    return ok
held = []
first = wave(80, held)
time.sleep(3)
for s in held: s.close()
held = []
time.sleep(3)
second = wave(20, held)
for s in held: s.close()
print('the first wave', first, 'the second wave', second)
" 2>&1)
echo "  | the table's ceiling: $SLOT"
grep -qE "netsys: ATTENTION.*the TCP slots have run out.*connections [0-9]+" /tmp/o.log \
  && { echo "  PASS  slotcap: the exhaustion of the table is named OUT LOUD and with a breakdown (by what exactly the slots are taken)"; PASS=$((PASS+1)); } \
  || { echo "  FAIL  slotcap: the table ran out silently"; FAIL=$((FAIL+1)); grep -aE "netsys:" /tmp/o.log | tail -3 | sed 's/^/      /'; }
case "$SLOT" in *"the second wave 20"*) echo "  PASS  slotcap: after the freeing the listener has been RECHARGED — twenty new connections out of twenty (hitting the ceiling is reversible)"; PASS=$((PASS+1));;
  *) echo "  FAIL  slotcap: after hitting the ceiling the listener did not come back ($SLOT)"; FAIL=$((FAIL+1));; esac
AFTERC=$(printf 'GET /after HTTP/1.0\r\n\r\n' | timeout 5 socat -T4 - tcp:10.0.0.2:80 2>/dev/null)
echo "$AFTERC" | grep -q "Talaria epoll server" \
  && { echo "  PASS  slotcap: and it serves — an answer of the application rather than only a handshake"; PASS=$((PASS+1)); } \
  || { echo "  FAIL  slotcap: after hitting the ceiling the server does not answer"; FAIL=$((FAIL+1)); }
stopq $q

echo "== THE CAPSTONE: an epoll server from the disk serves the network =="
q=$(boot "talaria.mode=diskserver" net); sleep 5
rm -f /tmp/d.*; PIDS=""
for i in 1 2 3 4; do
  ( printf 'GET /d%s HTTP/1.0\r\n\r\n' $i | timeout 6 socat -T5 - tcp:10.0.0.2:80 2>/dev/null > /tmp/d.$i ) & PIDS="$PIDS $!"
done
for p in $PIDS; do wait $p; done
OK=0; for i in 1 2 3 4; do grep -q "Talaria epoll server" /tmp/d.$i 2>/dev/null && OK=$((OK+1)); done
[ $OK = 4 ] && { echo "  PASS  the capstone: a server from the disk + the network + epoll (4/4)"; PASS=$((PASS+1)); } || { echo "  FAIL  the capstone: $OK/4"; FAIL=$((FAIL+1)); }
stopq $q

echo "== the zero-syscall fast path (the BSP in ring3 + an AP cell) =="
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 2 -m 512 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=fast" -display none -serial file:/tmp/o.log -no-reboot \
  -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
  -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 >/tmp/q.log 2>&1 &
QF=$!; sleep 5
FOK=0; for i in 1 2 3; do
  R=$(printf 'PING-%s' $i | timeout 3 socat -T2 - udp:10.0.0.2:9999 2>/dev/null)
  [ "$R" = "PING-$i" ] && FOK=$((FOK+1))
done
[ $FOK = 3 ] && { echo "  PASS  fastpath: 3/3 UDP echoes without syscalls on the hot path"; PASS=$((PASS+1)); } || { echo "  FAIL  fastpath: $FOK/3"; FAIL=$((FAIL+1)); }
# A2.2: the enforcement — the steady-state window of the hot path has been measured, 0 syscalls/0 faults
if grep -qE "hotpath enforce: .*Δsyscalls=0 Δfaults=0 -> OK zero-syscall" /tmp/o.log; then
  echo "  PASS  hotpath-enforce: measured Δsyscalls=0 Δfaults=0 on the steady state (A2.2)"; PASS=$((PASS+1))
else
  echo "  FAIL  hotpath-enforce: there is no confirmation of the zero-syscall on the steady state"; FAIL=$((FAIL+1)); fi
# A2.3: the cell's telemetry — the channel is filled and readable (pkts/p50/p99/gaps/uptime)
if grep -qE "telem: pkts=[1-9][0-9]* p50=[0-9]+c.* p99=[0-9]+c.* gaps=0 restarts=[0-9]+ uptime=[0-9]+s" /tmp/o.log; then
  echo "  PASS  telemetry: the cell's channel is filled (pkts/p50/p99/gaps/restarts/uptime) (A2.3)"; PASS=$((PASS+1))
else
  echo "  FAIL  telemetry: the cell's channel is not filled/is invalid"; FAIL=$((FAIL+1)); fi
kill $QF 2>/dev/null; wait $QF 2>/dev/null

# For the synchronisation gates the diagnostics "IT HAS HUNG" (talaria.futexhang=) is
# switched on EXPLICITLY: for them a long sleep on a futex means a lost wake-up, that is,
# exactly the defect they catch. For a real workload it is the norm, so by default it
# stays silent (see sync.rs::HANG_SECS).
echo "== PLAN-SYNC: a cross-core wake-up without a busy spin (mwait / hlt+ipi) =="
# boot2 MODE [EXTRA-QEMU-FLAGS] -> -smp 2, with a disk (for the user tests with ext2)
boot2() {
  rm -f /tmp/o.log
  qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 2 -m 512 ${2:-} \
    -kernel "/w/$K" -append "console=ttyS0 $1" -display none -serial file:/tmp/o.log -no-reboot \
    -drive id=d0,file=/img/disk.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!; qstarted "$1"
}
q=$(boot2 "talaria.test=sync talaria.futexhang=3" "-overcommit cpu-pm=on")
assert "SYNC: the mode (mwait.ring0.|waitpkg.umwait.) a cross-core wakeup: min=[0-9]+ ns p50=[0-9]{1,4} ns .* -> OK" "sync: the best tier (an mwait in ring0 / a umwait on waitpkg hardware), a wake-up by a write, p50 < 10 us"
assert "SYNC-MB: an asymmetric membarrier .* p50=[0-9]+ ns .* -> OK" "sync: membarrier (an IPI ack) works and has been measured"
stopq $q
q=$(boot2 "talaria.test=sync talaria.futexhang=3")
assert "SYNC: the mode (hlt.ipi|waitpkg.umwait.) a cross-core wakeup: .* -> OK" "sync: without cpu-pm — the hlt+IPI fallback (or a umwait: WAITPKG hardware does not degrade) (the matrix of the detection)"
stopq $q
q=$(boot2 "talaria.test=smp2" "-overcommit cpu-pm=on")
assert "SMP2: the parallel speedup [12]\.[0-9]+ .the thread on an AP, the syscalls from the AP. ALL OK" "smp2: sched_setaffinity migrates a thread onto an AP, the speedup is ~2x, the syscalls are from the AP"
stopq $q
q=$(boot2 "talaria.test=smp4" "-smp 4 -overcommit cpu-pm=on")
assert "SMP4: the parallel speedup [234]\.[0-9]+ on 4 cores .3 threads on AP1\.\.AP3. ALL OK" "smp4: three threads on AP1..AP3 (-smp 4), the speedup > 2.5x, getaffinity=0xf"
stopq $q
# ── V1.9: the layout of the threads over the cores ──────────────────────────
# Here what is checked is what was not in the kernel: SEVERAL threads on one secondary
# core. Before V1.9 an AP worked run-to-completion, and a second thread on it would never
# have got the processor. So the test is built in such a way that with no preemption it
# does not "show a bad number" but does not finish at all — silence is distinguishable
# from a hang (the pitfall of session 10: a metric can be plausible in vain).
# THE PRODUCTION PATH in full: a talaria-pack disk -> /cell.manifest with the rules ->
# mode=cell. Everything else here is given by the command line; this gate is about the
# same plan arriving from the disk and being applied without a single edit of the kernel.
bootd4() {
  rm -f /tmp/o.log
  qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 4 -m 512 -overcommit cpu-pm=on \
    -kernel "/w/$K" -append "console=ttyS0 $1" -display none -serial file:/tmp/o.log -no-reboot \
    -drive id=d0,file=$2,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!; qstarted "$1"
}
# The same, but the disk is THROTTLED down to the EBS gp3 class (125 MB/s, 3000 IOPS): a
# barrier costs 15-30 ms on it, and the holding of the FS lock across a barrier is visible to the neighbours.
bootd4gp3() {
  rm -f /tmp/o.log
  qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 4 -m 512 -overcommit cpu-pm=on \
    -kernel "/w/$K" -append "console=ttyS0 $1" -display none -serial file:/tmp/o.log -no-reboot \
    -drive id=d0,file=$2,format=raw,if=none,throttling.bps-total=125000000,throttling.iops-total=3000 \
    -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!
}
# The AVX state: a syscall, a signal handler and a preemption must not spoil the YMM.
# A mistake here does not crash at once — it spoils the data where a JIT or a garbage
# collector counts it, and surfaces minutes later as a "random" crash.
# THE CORE DUMPS. Here only the fact that the kernel WRITES them is checked; they are read
# by a real gdb on the host (below) — the format is either clear to the debugger or not.
q=$(bootd "talaria.test=corekernel talaria.core=/core.elf" /img/disk_core.img); sleep 6
assert "\[core\] [0-9]+ B into /core.elf: threads 1, memory segments" "coredump: a refusal IN THE KERNEL writes an ELF core dump onto the disk"
# The depth for THE KERNEL depends on the build: the ordinary one is built without frame
# pointers (that is a measured decision, ~4 % on a syscall — see kernel/build-prof.sh),
# and the honest depth there is 1. We check that the line is there and that it has a real
# address in it, while the DEPTH is checked on the workload below: crashme is built with frame pointers.
assert "the stack kernel .[1-8] frame" "coredump: the CALL STACK is printed in the crash dump rather than only the registers"
assert "the depth is 1 — the code is built without frame pointers" "coredump: the short chain is EXPLAINED (a build without frame pointers) rather than passed off as a real stack"; stopq $q
q=$(bootd "talaria.test=core talaria.core=/core.elf" /img/disk_core.img); sleep 6
assert "\[core\] [0-9]+ B into /core.elf: threads [0-9]+, memory segments" "coredump: a fatal signal to THE WORKLOAD writes an ELF core dump (the process is killed, the machine is alive)"
assert "the stack workload \([2-8] frame" "coredump: the stack of the crashed workload is in the log — without any debugger"
# HALF OF THE CRASHES IN RING 3 ARE "THE FS POINTS SOMEWHERE ELSE". In a workload on musl
# `errno`, the TLS and `__pthread_self()` all go through `%fs`, and a broken TLS cannot be
# told from an honest bug of the application by the rip and the cr2. On the stand on the
# 8th of September a PostgreSQL backend crashed with a write at the address 0x131 — that
# is `__errno_location()`, that is, `&__pthread_self()->errno_val`; to understand that, a
# foreign binary had to be disassembled. Now the crash line answers at once.
# `[fs:0]` is read THROUGH THE TABLES: a direct dereference in the crash handler would
# give an error inside the error handler, that is, a double one.
assert "fs=0x[0-9a-f]+ \\[fs:0\\]=0x[0-9a-f]+" "coredump: the crash line names the FS base and the TLS self-pointer"
refute "THE TLS IS BROKEN" "coredump: a healthy workload's TLS is intact — the mark \"THE TLS IS BROKEN\" is not set in vain"
stopq $q

# The crash of ONE thread of a multithreaded cell on four cores. A thread may crash on any
# AP, and until session 44 the handler jumped into the supervisor RIGHT FROM IT:
# the supervisor announced a halt, the sisters went on living on the other cores, there was
# no dump. Now it is a crash of the whole process: a dump with all the threads, all the
# threads are dead, control is given to the supervisor by the BSP.
if [ -f /img/disk_coremt.img ]; then
  q4=$(bootd4 "talaria.mode=cell talaria.smp=4 talaria.core=/core.elf" /img/disk_coremt.img); sleep 8
  assert "\[core\] [0-9]+ B into /core.elf: threads 4," "coredump-mt: the crash of a cell's thread on SMP writes a dump WITH ALL the process's threads"
  assert "supervise: the workload.s threads were killed by signal 11" "coredump-mt: the crash of one thread = the crash of the process (the supervisor learns it on the BSP)"
  refute "the main thread is alive after the sister.s crash" "coredump-mt: the sisters of the crashed thread do NOT go on working"
  refute "KVM internal error" "coredump-mt: the halt after the crash does not bring the machine down"
  stopq $q4
else echo "  SKIP  coredump-mt: there is no /img/disk_coremt.img"; fi

# A RESTART OF A MULTITHREADED CELL ON FOUR CORES. A thread killed in the middle of a
# syscall used to never start again and took with it what it held (an ext2 journal
# transaction): the new instance stood on "the journal is busy with a foreign
# transaction". And the idling APs held the CR3 of the former instance while the
# supervisor gave its tables to the new one — a "KVM internal error" at the start-up.
if [ -f /img/disk_crashr.img ]; then
  cp /img/disk_crashr.img /tmp/crashr.img
  q4=$(bootd4 "talaria.mode=cell talaria.smp=4 talaria.core=/core.elf" /tmp/crashr.img); sleep 12
  assertn "supervise: the workload.s threads were killed by signal 11" 2 "restart-mt: the crash of a thread on 4 cores -> a restart -> the second instance crashed the same way (both times it reached the supervisor)"
  assert "the restart limit .1. is exhausted" "restart-mt: the supervisor ran both instances and halted"
  refute "KVM internal error|the journal is busy with a foreign" "restart-mt: neither a crash of the machine nor a journal transaction taken away by the killed thread"
  stopq $q4
else echo "  SKIP  restart-mt: there is no /img/disk_crashr.img"; fi
if [ -f /img/disk_wdog.img ]; then
  cp /img/disk_wdog.img /tmp/wdog.img
  q4=$(bootd4 "talaria.mode=cell talaria.smp=4" /tmp/wdog.img); sleep 14
  assertn "crashme: a live instance" 2 "restart-wdog: a restart by the watchdog on 4 cores brings a new instance up (the threads in ring 3, on a condvar and in nanosleep have been taken off)"
  assert "the restart limit .1. is exhausted" "restart-wdog: the second restart hits the limit — the supervisor is alive"
  refute "KVM internal error|did not leave the tables of the previous instance|did not leave the crashed instance.s tasks" "restart-wdog: the APs left the tables of the former instance before they were freed"
  stopq $q4
else echo "  SKIP  restart-wdog: there is no /img/disk_wdog.img"; fi

# THE TABLES OF A FINISHED PROCESS ARE NOT FREED FROM UNDER AN AP. An idling AP holds the
# CR3 of the last task, while the BSP freed the address space of a dead process checking
# only the tasks "on a core". Forty children over three APs: the freeing must wait until
# the AP leaves onto the kernel's CR3.
if [ -f /img/disk_forkap.img ]; then
  cp /img/disk_forkap.img /tmp/forkap.img
  q4=$(bootd4 "talaria.mode=proc talaria.smp=4" /tmp/forkap.img); sleep 8
  assert "crashme: forkap ALL OK" "forkap: 40 children on the APs were born, worked and were reaped by waitpid"
  assert "it has left the tables of the finished process onto the kernel.s CR3" "forkap: an idling AP leaves the tables of a dead process (the protection has been engaged rather than \"not needed\")"
  refute "KVM internal error|CRASH DUMP" "forkap: the freeing of the AS of dead processes does not bring the machine down"
  stopq $q4
else echo "  SKIP  forkap: there is no /img/disk_forkap.img"; fi

# SHARED MEMORY CREATED BY TWO THREADS AT THE SAME INSTANT. `shm_open(O_CREAT)` and
# `shmget(IPC_CREAT)` are compound: find by name/key, and if there is none, take a free row
# of the kernel's table. Without the tables' lock two threads that both miss the lookup take
# TWO rows — the halves of a workload then map DIFFERENT memory while nothing crashes.
#
# THE GATE MUST MAKE THE RACE HAPPEN, otherwise it proves only that shm works: the section is
# microseconds long, four threads released from a barrier miss each other, and the kernel's
# counter of waits stays at zero (that is exactly what the first version of this gate showed).
# `talaria.shmdelay=2000` widens the window inside the section; the counter in `stat:` is
# asserted so that a run where the threads never met cannot pass. The APs must be up: on one
# core the race is impossible in principle (the syscall path runs with interrupts masked, so
# a creator inside the section is not preempted).
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 4 -m 1024 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.test=shmrace talaria.smp=4 talaria.shmdelay=2000 talaria.stat=3000" \
  -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 &
q4=$!; qstarted shmrace
for i in $(seq 1 40); do sleep 0.5; grep -aq "shmrace: ALL OK\|shmrace: FAILURES\|panic" /tmp/o.log 2>/dev/null && break; done
assert "shmrace: ALL OK" "shmrace: four threads create shm at once — one name gives ONE object, own names give own objects, one SysV key gives one segment"
assert "stat: shm: creators waited for the tables [1-9][0-9]* times" "shmrace: the race REALLY happened (the kernel counted the waits for the tables — without this the gate would pass on threads that never met)"
stopq $q4
# The A/B of the same programme: WITHOUT the lock (talaria.shmlock=0) the concurrent create
# must break. A gate that cannot fail is not a gate.
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 4 -m 1024 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.test=shmrace talaria.smp=4 talaria.shmdelay=2000 talaria.shmlock=0" \
  -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 &
q4=$!; qstarted shmrace-ab
for i in $(seq 1 40); do sleep 0.5; grep -aq "shmrace: ALL OK\|shmrace: FAILURES\|panic" /tmp/o.log 2>/dev/null && break; done
assert "shmrace: FAIL one name -> one object" "shmrace A/B: with the tables' lock OFF the same programme gets TWO objects for one name — that is the defect the lock closes"
stopq $q4
# A PIPE AND AN EVENTFD WITH THEIR ENDS ON TWO CORES (docs/ARM64-PLAN.md §10.10): each was
# one counter both ends read, changed and wrote back with plain stores — bytes lost or read
# twice, increments lost. The old kernel failed this 2 of 3 runs under TCG (4 M wrong bytes,
# an eventfd sum of 304 930 for 300 000) and hung the third.
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 4 -m 1024 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.test=piperace talaria.smp=4" \
  -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 &
q4=$!; qstarted piperace
for i in $(seq 1 120); do sleep 0.5; grep -aq "piperace: ALL OK\|piperace: FAILURES\|panic" /tmp/o.log 2>/dev/null && break; done
assert "piperace: the pipe carried 4194304 bytes in order across cores" "piperace: a pipe between threads on two cores carries 4 MB byte-exact"
assert "piperace: the eventfd summed 300000 increments from 3 threads exactly" "piperace: an eventfd written from three cores loses no increment"
stopq $q4
# A SYSCALL THAT MOVES THE THREAD KEEPS EVERY REGISTER (docs/ARM64-PLAN.md §10.13): the move
# from the boot core rebuilt the return context from the callee-saved registers only — rdi..r10
# came back zero here, x0..x18 and lr on arm64 (ClickHouse's threads returned to address 0).
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 4 -m 1024 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.test=migregs talaria.smp=4" \
  -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 &
q4=$!; qstarted migregs
for i in $(seq 1 60); do sleep 0.5; grep -aq "migregs: ALL OK\|migregs: FAILURES\|panic" /tmp/o.log 2>/dev/null && break; done
assert "migregs: sched_setaffinity -> 0, the core 0 -> [1-3]" "migregs: sched_setaffinity moves the thread off the boot core"
assert "migregs: ALL OK" "migregs: rdi/rsi/rdx/r8-r10 survive the move (a syscall preserves all but rax, rcx, r11)"
stopq $q4
# A WAITER OF A MIXED EPOLL SET, PARKED (`talaria.netmix=`, docs/ARM64-PLAN.md §10.13): a pipe,
# an eventfd and a signalfd in the set; one of two waiters is the stack's poller, the other
# parks with a 2 s ceiling, and the producers must kick it — a kernel without the kicks wakes
# it at 1970 ms. Also the epoll_wait timeout with no NIC (it was zero cycles: an instant 0).
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 4 -m 1024 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.test=mixwait talaria.smp=4 talaria.netmix=2000000000" \
  -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 &
q4=$!; qstarted mixwait
for i in $(seq 1 120); do sleep 0.5; grep -aq "mixed-set waiters parked\|mixwait: FAILURES\|panic" /tmp/o.log 2>/dev/null && break; done
assert "mixwait: ALL OK" "mixwait: a parked mixed-set waiter is woken by a pipe, an eventfd and a signal at once, not by its 2 s ceiling"
assert "mixed-set waiters parked [1-9][0-9]* \(woken [1-9][0-9]*, by the ceiling 0\)" "mixwait: the waiters really parked, and no park ended by the ceiling"
stopq $q4

# THE ISOLATION OF THE MEMORY. The identity window GB0 (32 MB-1 GB of the physical memory)
# in a process's AS was marked U/S: from ring 3 the page tables, the kernel stacks and the
# frames of foreign processes could be read — both in `cell` and in `proc`. A read must
# end with a SIGSEGV on the very first access.
if [ -f /img/disk_peek.img ]; then
  for m in cell proc; do
    cp /img/disk_peek.img /tmp/peek.img
    q=$(bootd "talaria.mode=$m" /tmp/peek.img); sleep 5
    assert "the signal 11 to the thread t[0-9]+: the rip 0x[0-9a-f]+, the address 0x8000000" "isolation ($m): a read of the physical memory through the identity window from ring 3 -> a SIGSEGV"
    refute "READ .* the window is OPEN" "isolation ($m): the workload did NOT read the kernel's memory"
    stopq $q
  done
else echo "  SKIP  isolation: there is no /img/disk_peek.img"; fi

q4=$(bootd4 "talaria.test=memrace talaria.smp=4" /img/disk_ext2.img); sleep 20
assert "memrace: ALL OK" "memrace: parallel mmap/mprotect/DONTNEED/munmap on 4 cores do not spoil the memory"; stopq $q4
# A TLB FLUSH MUST BE ACKNOWLEDGED EVEN WHILE THE KERNEL IS PRINTING.
#
# Under a hypervisor every access to the UART port is a VMEXIT, while the block of the
# diagnostics is printed FROM A TICK, with IF=0: fifty lines are ten thousand VMEXITs in a
# row, and all that time the interrupts are not accepted. A neighbour that had removed a
# mapping did not get the acknowledgement within the allotted 10 ms and GAVE UP, leaving a
# live TLB entry on that core for a frame the allocator had already given to somebody
# else. In ClickHouse this looked like `Checksum doesn't match` and different sums of one
# and the same SELECT. The gate gives exactly such a combination: parallel munmaps on 4
# cores plus a frequent printing of the snapshots from a tick.
#
# Session 26: the gate "blinked" (`did not acknowledge 1-2` in every second or third run,
# and on the PRE-COMMIT kernel too), and the culprit was named by the kernel's printing at
# a refusal — `shootdown: core N did not acknowledge within 10 ms (the task, the syscall)`:
# the silent core printed a `[fault]` line from the #PF handler every time. Four memrace
# threads catch a SIGSEGV within one microsecond, four cores print at once, and a byte
# under QEMU's common lock costs ~100 us — the window "once every 64 bytes" stretched to
# 10+ ms. The servicing of the mailbox during the printing has been moved onto TIME
# (100 us by the TSC, per core), and in the walks of munmap/DONTNEED onto once every 64
# pages; 17 runs in a row with `did not acknowledge 0`, the total wait 50-100 -> 27-48 ms.
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 4 -m 2048 -overcommit cpu-pm=on \
  -kernel "/w/$K" -append "console=ttyS0 talaria.test=memrace talaria.smp=4 talaria.top=200" \
  -display none -serial file:/tmp/o.log -no-reboot \
  -drive id=d0,file=/img/disk_ext2.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & q4=$!
assert_slow "did not acknowledge 0\)" "shootdown: a TLB flush is acknowledged by ALL the neighbours even under a dense printing from a tick (otherwise the initiator gives up and leaves a live TLB entry on a foreign frame)"; stopq $q4
# LARGE PAGES ARE HANDED OUT RATHER THAN ONLY RESERVED. Twice in a row the conclusion
# "large pages give no gain" was drawn from measurements where the counter of the handed
# out ones was EXACTLY ZERO with hundreds of reservations — that is, a switched-off
# mechanism was measured. The gate closes exactly this. 2 GB of memory are needed:
# `huge_ok` requires half a gigabyte free, and on a guest's usual 512 MB the mechanism does not turn on by construction.
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 4 -m 2048 -overcommit cpu-pm=on \
  -kernel "/w/$K" -append "console=ttyS0 talaria.test=memrace talaria.smp=4 talaria.huge=2" \
  -display none -serial file:/tmp/o.log -no-reboot \
  -drive id=d0,file=/img/disk_ext2.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & q4=$!
assert_slow "2 MB huge pages=[1-9][0-9]*" "huge: with talaria.huge=2 the large pages are REALLY handed out (rather than only reserved)"; stopq $q4
q4=$(bootd4 "talaria.test=schedapi talaria.smp=4" /img/disk_ext2.img); sleep 12
assert "schedapi: ALL OK" "schedapi: the scheduler's contract — yield GIVES the core to the neighbour, membarrier puts a barrier, the binding to the cores goes there and back, and getrlimit announces the wall that is really there (dup(0) hits EMFILE exactly at RLIMIT_NOFILE, a raise is refused)"; stopq $q4
# A file LARGER than the block cache is the only place where the eviction of dirty pages
# goes on all the time, and the only gate that catches the race for the path buffer: before
# the fix `fsbigrace` on four cores failed EVERY time, on one it was always green.
q4=$(bootd4 "talaria.test=fsbigrace talaria.smp=4" /img/disk_fsbig.img); sleep 60
assert "FSBIG: ALL OK" "fsbig: a file of 32 MB (four times the cache) reads back as exactly what was written; the MergeTree parts with a rename and a read from under deleted names on 4 cores"
# THE RACE FOR THE PATH BUFFER, THE SECOND ROUND. The buffer of the path's parsing was PER
# CORE — on the invariant "a syscall cannot be interrupted". The invariant died when a
# thread waiting for the FS lock started GIVING the core to a neighbour: the path's slice
# survives a task switch, and the neighbour writes its own path into it. The measurement on
# the old kernel: 377 substitutions out of 3840 checks (10 %), in ClickHouse
# "file_size: No such file or directory" on a file of a just created part in every third
# run. It is caught ONLY by the file's size: a substituted path more often points at a
# foreign EXISTING file, and stat returns success.
assert "fsbig: the path: checks [0-9]+, substitutions 0" "fsbig: a syscall's path belongs to ITS OWN task (the buffer is per TASK rather than per core: a yield of the core inside a syscall no longer substitutes the path)"
# The gate is about a closed class of mistakes: the name `fsbigrace` starts with `fs`, and
# while the mode was chosen by a SUBSTRING search, a FOREIGN mode `talaria.test=fs` was
# executed together with it — it went for /hello.txt, which is not on this volume. There
# must be no line of the foreign mode in the log.
refute "open\(/hello\.txt\)" "cmdline: with talaria.test=fsbigrace the foreign mode talaria.test=fs is NOT executed (the collision of the prefixes is closed)"; stopq $q4
# §8.14 THE EVICTION OF A DIRTY SLOT — IN A BATCH (`talaria.evictbatch=`). A miss whose
# candidate for eviction is dirty cost a synchronous write of ONE block under the FS lock:
# a trip to the device at the price of a round-trip latency paid off 4 KB.
# The `evictbatch` mode itself squeezes the cache down to 2 MB and removes the pre-emptive
# flush — otherwise the eviction of a dirty one almost NEVER happens (on the defaults a
# whole run of fsbig gives ONE), and the gate would be checking emptiness.
# Two boots differing ONLY in the knob: without it one block per request, with it a batch.
# A knob that is parsed and does not act cost session 35 a whole rise of the stand
# (smtpack, 158ffdc), so what is checked is not "it has been printed" but "the behaviour has changed".
q4=$(bootd4 "talaria.test=evictbatch talaria.smp=4 talaria.stat=1000 talaria.evictbatch=1" /img/disk_fsbig.img); sleep 14
assert "FSBIG: ALL OK" "evictbatch: on a squeezed cache (2 MB, without the pre-emptive flush) the data agrees byte for byte and without a batch"
assert "the eviction of slots: blocks [1-9][0-9]* over [1-9][0-9]* requests \(1 blocks per request" \
  "evictbatch=1 (the default): the eviction of a dirty slot is ONE block per trip to the device, as it was"
refute "the eviction of slots: blocks [0-9]+ over [0-9]+ requests \([2-9]" \
  "evictbatch=1: no batches are assembled at all (the default changes nothing)"; stopq $q4
q4=$(bootd4 "talaria.test=evictbatch talaria.smp=4 talaria.stat=1000 talaria.evictbatch=64" /img/disk_fsbig.img); sleep 14
assert "FSBIG: ALL OK" "evictbatch=64: a file of 32 MB reads back as exactly what was written — the batch did not mix the blocks up"
assert "the eviction of slots: blocks [1-9][0-9]* over [1-9][0-9]* requests \([2-9][0-9]* blocks per request" \
  "evictbatch=64: the knob ACTS — the consecutive dirty blocks go out in one request (on QEMU 8 blocks against 1, the time of the evictions 84 ms against 353)"; stopq $q4
# THE VIRTUAL FILES OF /proc ARE NOT TRUNCATED. ClickHouse, the JVM and PostgreSQL size
# their pools by `/proc/cpuinfo`, while the buffer for it was 2048 bytes for everything:
# a record per core is ~450 B, which means that on eight cores the workload saw FIVE
# processors, and on a metal with a hundred and twenty-eight the same five. The truncation
# was SILENT: the middle of the last record broke off, the file looked plausible. The gate
# compares the number of records with the number of cores.
q4=$(bootd4 "talaria.test=virtfiles talaria.smp=4" /img/disk_ext2.img); sleep 6
assert "virtfiles: ALL OK" "virtfiles: /proc/cpuinfo and /proc/stat give out a record FOR EVERY core (a workload sizes its pools by them)"; stopq $q4
q4=$(bootd4 "talaria.test=fsinfo talaria.smp=4" /img/disk_ext2.img); sleep 8
assert "fsinfo: ALL OK" "fsinfo: statfs/fstatfs give out the volume's REAL numbers (zeroes meant \"there is no room\" to a database on an empty disk)"; stopq $q4
q4=$(bootd4 "talaria.test=mbench talaria.smp=4" /img/disk_ext2.img); sleep 12
assert "mbench: ALL OK" "mbench: the primitives of the hot loop (the ALU, the caches, the memory, a TLB miss, a syscall) — with the same binary that goes to Linux"; stopq $q4
# THE READ-AHEAD IS COUNTED BY BEHAVIOUR RATHER THAN BY THE REQUEST'S SIZE. While the
# window was limited by the current request, a file read in pieces of 4 KB went at
# 169 MB/s against 1680 in pieces of a megabyte — the same file, the same FS, every block
# cost a separate trip to the device. The gate compares these two numbers with each other
# rather than with an absolute: the absolute depends on the stand, the ratio does not.
q4=$(bootd4 "talaria.test=fsread talaria.smp=4" /img/disk_fsbig.img); sleep 40
assert "fsread: ALL OK" "fsread: a file of 96 MB is read in pieces of 4 KB no more than twice slower than in megabyte ones — the read-ahead window is alive"
# One ratio is not enough: switch the read-ahead off entirely and BOTH numbers sag while
# the ratio stays good (checked with `talaria.readahead=0`). So an assertion that the
# window fired at all stands next to it.
assert "readaheads [1-9][0-9]* \\(\\+[1-9]" "fsread: the read-ahead window really fired (it is not \"both ends are equally slow\")"; stopq $q4
# §8.6-8.8: a NEIGHBOUR's fsync must not stop the FS. The writer lays down 64 MB and does
# an fsync, the second overwrites a file round after round with fsyncs, and the third reads
# and does an fstat all this time — and not a single operation of its has the right to
# stand longer than 20 ms. On a throttled disk this caught the holding of the FS lock
# across a barrier (a mongodb insert on EC2: stalls of 3-8 s, a ceiling of 4.2 thousand
# against Linux's 9.6) — the unloading of the data by the fsync, the eviction of dirty
# metadata with a commit+a checkpoint, the barriers of the journal's superblock and of the recover flag.
q4=$(bootd4gp3 "talaria.test=fsyncconc talaria.smp=4" /img/disk_fsbig.img)
for i in $(seq 1 90); do sleep 1; grep -aq "fsyncconc: ALL OK\|fsyncconc: FAILED\|panic" /tmp/o.log 2>/dev/null && break; done
assert "fsyncconc: ALL OK" "fsyncconc: an fsync of 64 MB and an overwrite round after round on a disk of the gp3 class — the neighbour does not stand longer than 20 ms, the files read back as what was written"
assert "fsyncconc: the neighbour: operations [0-9]+, the worst [0-9.]+ ms, stalls longer than 20 ms: 0" "fsyncconc: the neighbour has ZERO stalls longer than 20 ms (the FS lock is not held across the device's barrier)"; stopq $q4
# The same workload on ONE core: the writer, the overwriter and the neighbour share the
# core. On one core "zero stalls for the neighbour" is NOT guaranteed: the owner of the FS
# lock is recognised by the CORE'S NUMBER and cannot yield, while a barrier under the lock
# on a gp3 throttle lasts up to ~170 ms (1 run out of 30 — a stall of the neighbour of
# 169 ms); the gate here judges the files' content (open, by name: docs/ROADMAP.md §2.9).
q4=$(bootd4gp3 "talaria.test=fsyncconc talaria.smp=1" /img/disk_fsbig.img)
for i in $(seq 1 180); do sleep 1; grep -aq "summary: the tasks by their wait\|panic" /tmp/o.log 2>/dev/null && break; done
sleep 2
assert "fsyncconc: ok a file overwritten round after round keeps the LAST round" "fsyncconc on one core: the writer, the overwriter and the neighbour share the core — the files read back as what was written"; stopq $q4
# The accounting of a yield of the core while waiting for the DEVICE separately from
# waiting for the PROCESSOR: the metric "ready->processor" ascribed the whole barrier to
# the one waiting for it (WTJourn.Flusher "waited 10.7 s" under the gp3 throttle). This
# cannot be judged by a workload with a disk — the length of a barrier is a property of
# the STAND (on a tmpfs there is nothing to yield for), so it is pure arithmetic on one
# core: the task A yields for half a second under the io_yield marker, the task B counts;
# for A "it gave up to the device" is almost the whole wall and "it waited" almost zero.
# SESSION 28: the pickup loop of an empty core must apply the scheduler's hint. The
# mechanism "give the processor to the woken one" ended at a core WITHOUT a task: the loop
# took the first task of its core by number, without looking at the hint and without
# asking whether it was ready. The gate tells them apart — without the fix A leaves the
# core TWICE (it is woken only to yield again).
q4=$(bootd4 "talaria.test=hintpick" /img/disk_ext2.img)
for i in $(seq 1 20); do sleep 1; grep -aq "hintpick: ALL OK\|hintpick: FAILED\|panic" /tmp/o.log 2>/dev/null && break; done
assert "hintpick: ALL OK" "hintpick: the pickup loop takes a task BY THE HINT rather than by its number (A leaves the core once)"
assert "hintpick: A left the core 1 time\(s\), hints applied by the pickup loop [1-9]" "hintpick: the hint has been applied by the pickup loop itself"; stopq $q4

# SESSION 35, the step 2: the order of the logical cores decides how many PHYSICAL cores
# talaria.smp=N takes (on a c6i.xlarge the MADT gives [0,2,1,3], that is, the first two
# numbers are different physical cores, and the host has nowhere to run: a steal of 249 ms
# against 2 ms with smp=1). There is no such machine at home — the permutation is judged.
q=$(boot "talaria.test=smtpack")
for i in $(seq 1 15); do sleep 1; grep -aq "smtpack: ALL OK\|smtpack: FAILED\|panic" /tmp/o.log 2>/dev/null && break; done
assert "smtpack: ALL OK" "smtpack: the permutation of the logical cores by the hyperthreads of one physical one (c6i.xlarge, the idempotence, the ids not starting from zero)"; stopq $q

# A KNOB MUST ACT RATHER THAN ONLY BE PARSED. The first version applied the permutation in
# topology_init (the parsing of the MADT), while the tail of the cmdline on EC2 arrives
# from the user-data much later — the knob was honestly printed by the audit and did
# nothing (the stand's phase smt35-20260916-122712/B passed with apic_of=[0,2,1,3] under
# talaria.smtpack=1). Now the permutation goes before the APs are brought up, on the merged
# cmdline, and prints the layout AFTER itself — and that is what is checked.
q=$(bootht "talaria.smp=2 talaria.smtpack=1 talaria.test=fsinfo")
for i in $(seq 1 15); do sleep 1; grep -aq "the SMP scheduler\|panic" /tmp/o.log 2>/dev/null && break; done
assert "the layout by the physical cores .CPUID 0x0B, the SMT shift 1.: the logical ones 0..4 -> the cores .0, 0, 1, 1. the threads .0, 1, 0, 1.; DIFFERENT physical cores 2" \
  "smtpack: the layout of the logical cores over the PHYSICAL ones is printed by CPUID 0x0B (a 2x2 guest)"
assert "talaria.smtpack=1 — the logical numbers go by the hyperthreads of one physical core" \
  "smtpack: the knob ACTS before the APs are brought up (on the merged cmdline) rather than only being parsed by the audit"; stopq $q

# SESSION 35: a thread waiting for a disk command that must not yield the core spun with
# IF=0 — the tick did not come for the whole time of the command (the instrument "the
# tick's hole": #18 pwrite64, 16-18 ms with zero time of the host). The gate is about the
# mechanism rather than about the disk: 50 ms in `io_wait_step` with the interrupts closed.
q4=$(boot "talaria.test=iowin")
for i in $(seq 1 20); do sleep 1; grep -aq "iowin: ALL OK\|iowin: FAILED\|panic" /tmp/o.log 2>/dev/null && break; done
assert "iowin: ALL OK" "iowin: the window of the interrupts while waiting for the device — the tick gets through (talaria.iowin=1) and does not with 0"
assert "iowin: 50 ms in io_wait_step with IF=0: with the window ticks [1-9][0-9]+ \(windows [1-9]" "iowin: the windows really opened rather than \"the tick came by itself\""; stopq $q4

q4=$(boot "talaria.test=ioyacct")
for i in $(seq 1 20); do sleep 1; grep -aq "ioyacct: ALL OK\|ioyacct: FAILED\|panic" /tmp/o.log 2>/dev/null && break; done
assert "ioyacct: ALL OK" "ioyacct: the yield to the device >= 60 % of the wall, the wait for the processor <= 10 %, the sum adds up to the wall (+-50 ms)"
assert "^ioyacct: +t[0-9]+ .*it waited [0-9]{1,2} ms, it ran [0-9]+ ms over [0-9]+ intervals, it gave up to the device [3-9][0-9]{2,} ms" "ioyacct: in the summary by the tasks the yielding one has \"it waited for the processor\" — not the time of the yield"; stopq $q4
# A console command from the BSP's tick against the network's syscalls on the other cores.
# The marker "in a tick" was ONE PER MACHINE: while the BSP printed the answer to `stat`
# (the set's harness prints it at the end of every point), the stack answered EAGAIN to
# the syscalls of ALL the cores — and mongod's `close()` is not repeated, the socket got
# stuck in CLOSE-WAIT and the slots ran out (session 26). The same gate caught a SECOND
# defect with the same outcome: the process's descriptor table was filled without a CAS,
# two threads on different cores took one number and one of the two sockets stayed without
# a descriptor for ever. The threshold of 1000 calls is so that the gate does not pass by
# "silence" (in reality it is ~4 million over 2 s).
bootn4() {
  rm -f /tmp/o.log
  qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 4 -m 512 -overcommit cpu-pm=on \
    -kernel "/w/$K" -append "console=ttyS0 $1" -display none -serial file:/tmp/o.log -no-reboot \
    -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
    -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
    -drive id=d0,file=/img/disk.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!; qstarted "$1"
}
# A THREAD WAITING FOR THE NETWORK MUST ACKNOWLEDGE A TLB FLUSH TO ITS NEIGHBOURS.
#
# The initiator of a `munmap` waits for the acknowledgement from the neighbouring cores
# for 10 ms and GIVES UP, leaving a stale TLB entry at the one that did not answer — that
# is, it goes on walking over a frame the allocator has already given to somebody else. So
# every place where the kernel waits with the interrupts disabled services the mailbox of
# the requests itself: the spin of the FS lock, the spin of the stack's lock, membarrier,
# the printing into the serial. The wait for the NETWORK — the most populated of them (in
# a server with a thread per connection almost all the tasks stand here almost all the time) — did not do it.
#
# HONESTLY: the stand's symptom does not reproduce locally — on a kernel WITHOUT the fix
# this gate is green too (in EC2's console "did not acknowledge" 957 times over a run of
# mongodb and not once for mysql/postgresql/redis; locally once over a ten-minute run of
# mongodb). The gate holds the INVARIANT rather than reproducing that case: a thread
# waiting for the network against three neighbours removing 512 pages per turn, and not a
# single unacknowledged flush.
# AN ACKNOWLEDGEMENT IS NOT AWAITED FROM A NEIGHBOUR STANDING IN `hlt`.
#
# The initiator of a `munmap` waits for the acknowledgement of a TLB flush for 10 ms and
# gives up. On Nitro the PV IPI is not announced (`smp: the KVM PV IPI is not announced`
# in the stand's console), the ring goes as a write into the ICR, and the waking of a
# STOPPED neighbour this way does not fit into the ceiling: over the run of mongodb on the
# 8th of September "did not acknowledge within 10 ms" was printed 440 times (the tasks
# with the last syscalls 35 nanosleep and 202 futex — that is, idling cores). Locally,
# where the PV IPI is there, the same sleep is acknowledged within 1057 us over 29403
# flushes — so the matter is not in our waiting loops but in the delivery of the ring, and
# there is no point waiting for it: the ring ALREADY hangs in the neighbour's IRR.
#
# The gate's arrangement: one thread removes 512 pages per turn, one sleeps in nanosleep,
# and two cores out of four are busy with nothing — that is, they stand in `hlt`.
q4=$(bootn4 "talaria.test=idleshoot talaria.smp=4 talaria.stat=3000")
for i in $(seq 1 40); do sleep 1; grep -aq "idleshoot: ALL OK\|idleshoot: FAILED\|panic" /tmp/o.log 2>/dev/null && break; done
assert "idleshoot: ALL OK" "idleshoot: one thread removes mappings, one sleeps, two cores idle"
for i in $(seq 1 20); do sleep 1; grep -aq "acknowledgements were not waited for" /tmp/o.log 2>/dev/null && break; done
assert "\(the neighbour STANDS IN hlt\) [1-9][0-9]*" "idleshoot: the rule has fired — the waits for an acknowledgement from a stopped neighbour have been STOPPED (without the fix the counter is zero and the gate is red)"
refute "shootdown: core .* did not acknowledge" "idleshoot: not a single \"did not acknowledge\" — there are no give-ups by the ceiling"
grep -a "TLB flushes to neighbours\|acknowledgements were not waited for" /tmp/o.log | tail -2 | sed 's/^/  | /'
stopq $q4

q4=$(bootn4 "talaria.test=netshoot talaria.smp=4")
# The gate lasts its own time: four seconds of waiting for the network under the removal
# of mappings plus the boot. The `assert` window (6 s) is not meant for that — we wait for the verdict explicitly.
for i in $(seq 1 40); do sleep 1; grep -aq "netshoot: ALL OK\|netshoot: FAILED\|panic" /tmp/o.log 2>/dev/null && break; done
assert "netshoot: ALL OK" "netshoot: a thread waiting for the network against neighbours in mmap/munmap — the arrangement itself has worked"
NSH=$(grep -a "netshoot: the removals" /tmp/o.log | tail -1)
echo "  | $NSH"
refute "shootdown: core .* did not acknowledge" "netshoot: a thread waiting for the network acknowledges a TLB flush — not a single \"did not acknowledge\" under the removal of mappings"
stopq $q4

q4=$(bootn4 "talaria.test=tickcmd talaria.smp=4")
for i in $(seq 1 40); do sleep 1; grep -aq "tickcmd: ALL OK\|tickcmd: FAILED\|panic" /tmp/o.log 2>/dev/null && break; done
assert "tickcmd: 200 commands executed from the tick" "tickcmd: the console's commands really went from the BSP's tick alongside the workload"
assert "tickcmd: socket\+close on the APs: calls [0-9]+, EAGAIN refusals 0, other errors 0" "tickcmd: the network's syscalls on the APs do not get an EAGAIN from the BSP's tick, the descriptor table does not lose sockets under three threads"
assert "tickcmd: ALL OK" "tickcmd: ALL OK"; stopq $q4
# AN INSTRUMENT HAS NO RIGHT TO SPOIL THE MEASUREMENT. The `talaria.stat=` block is 7 KB
# over a polled UART, and a byte there costs 40-60 us: printing the block IN FULL from the
# tick (with IF=0) gave a maximum of 417-555 ms on a mongodb insert against 16 ms without
# the snapshots, and that stall was listed as "the preallocation of WiredTiger's journal".
# Now the block is assembled in memory and goes onto the line in portions, and the gate
# watches exactly the portion's size: one portion is tens of bytes rather than the whole block.
q4=$(bootn4 "talaria.test=schedbal talaria.smp=4 talaria.stat=2000")
for i in $(seq 1 40); do sleep 1; grep -aqE "stat: the worst single pour: [0-9]+ .* for [1-9]" /tmp/o.log 2>/dev/null && break; done
assert "stat: snapshots assembled [1-9][0-9]*, skipped \(the previous one not poured out\) [0-9]+, bytes that did not fit 0" \
  "stat: the snapshot is assembled in memory in full (nothing is lost) and goes onto the line in portions"
assert "stat: the worst single pour: [0-9]+ .* for ([1-9]|[1-5][0-9]|6[0-4]) bytes" \
  "stat: the worst portion of the output is tens of bytes rather than the whole block (the instrument does not hold the core)"
stopq $q4
# A LINE IN THE CONSOLE IS PRINTED IN FULL. A `println!` with arguments is several calls of
# write_str, and the output's lock was taken for EVERY piece: a neighbouring core
# inserted its own line into the middle of a foreign one (four splices per seven hundred
# lines in every run of mongodb). The log is an instrument of analysis, and a splice makes
# it a liar: a line does not disappear, it stops being a line. The check against the
# measurement: with the lock per piece this gate gives 237 lines out of 240 and a splice.
q4=$(bootn4 "talaria.test=conprint talaria.smp=4")
for i in $(seq 1 30); do sleep 1; grep -aq "conprint: ALL OK\|panic" /tmp/o.log 2>/dev/null && break; done
assert "conprint: ALL OK" "conprint: four ring-0 threads printed their lines racing each other"
assertn "^conprint: t[0-9]+ n[0-9]+ [0-9]+ [0-9]+ [0-9]+ tail.?$" 240 \
  "conprint: all the 240 lines are intact — not one has been cut by a foreign line"
refute "^.+conprint: t[0-9]" "conprint: no line has a foreign one starting inside it (the lock is taken per line)"
stopq $q4
# The device's lock DOES NOT GIVE IN SILENTLY. Earlier the wait gave in after a second and
# the write went without the lock (on a gp3 a barrier is sometimes 1.7-1.9 s — it fired
# under an honest load and broke the order "the copies -> the commit -> in place", that
# is, a lie of the fsync without a single counter). The BSP holds the lock for 1.5 s, an AP
# asks: exactly one diagnosis with the holder's name, the taking AFTER the release, zero bypasses.
q=$(boot2 "talaria.test=devlockdiag" "-overcommit cpu-pm=on")
for i in $(seq 1 20); do sleep 1; grep -aq "DEVLOCK: OK\|DEVLOCK: FAIL\|panic" /tmp/o.log 2>/dev/null && break; done
assert "fs: the device.s lock did not give in within 1 s: core 0 holds it \(the task [0-9]+, already 1[0-9]{3} ms\), waiters outside [0-9]+; we keep waiting \(give-ups 0\)" "devlockdiag: after a second of waiting the device's lock names its holder (the core, the task, how long it has held it)"
assert "DEVLOCK: OK" "devlockdiag: the waiter takes the lock only AFTER the release, there is one diagnosis, zero bypasses (a bypass is only for a panic)"; stopq $q
# The same rule for the FS lock: an entry into ext2 without the lock spoils the FS quietly and reliably.
q=$(boot2 "talaria.test=fslockdiag" "-overcommit cpu-pm=on")
for i in $(seq 1 20); do sleep 1; grep -aq "FSLOCK: OK\|FSLOCK: FAIL\|panic" /tmp/o.log 2>/dev/null && break; done
assert "ext2: the FS lock did not give in within 1 s: core 0 holds it \(the task [0-9]+ \".*\", already 1[0-9]{3} ms\), it is (DEAD|ASLEEP on a word|ready); it RUNS on core -?[0-9]+ \(it is assigned to [0-9]+\), the last syscall -?[0-9]+" \
  "fslockdiag: after a second of waiting the FS lock names its holder AND ITS STATE (a task's number does not answer why it does not release it)"
assert "FSLOCK: OK" "fslockdiag: the waiter enters ext2 only AFTER the release, there is one diagnosis, zero bypasses"; stopq $q
# The lock of a virtqueue RING: the same store — after a second a guard was returned
# without the lock, and its Drop released a FOREIGN lock (two cores submitted into the ring at once).
q=$(boot2 "talaria.test=ringlockdiag" "-overcommit cpu-pm=on")
for i in $(seq 1 20); do sleep 1; grep -aq "RINGLOCK: OK\|RINGLOCK: FAIL\|panic" /tmp/o.log 2>/dev/null && break; done
assert "fs: the ring.s lock did not give in within 1 s: core 0 holds it \(we are core 1\); we keep waiting \(give-ups 0\)" "ringlockdiag: after a second of waiting the ring's lock names its holder"
assert "RINGLOCK: OK" "ringlockdiag: a read from an AP finishes only AFTER the ring is released, and successfully, zero bypasses"; stopq $q
# The STACK's lock waits as long as it takes, but not silently: after a second — a diagnosis with the holder.
q=$(boot2 "talaria.test=netlockdiag" "-overcommit cpu-pm=on")
for i in $(seq 1 20); do sleep 1; grep -aq "NETLOCK: OK\|NETLOCK: FAIL\|panic" /tmp/o.log 2>/dev/null && break; done
assert "netsys: the stack.s lock did not give in within 1 s: core 0 holds it \(the depth 1, already 1[0-9]{3} ms\), we are core 1 task [0-9]+; we keep waiting" "netlockdiag: after a second of waiting the stack's lock names its holder and the depth"
assert "NETLOCK: OK" "netlockdiag: the waiter enters the stack only AFTER the release, there is one diagnosis"; stopq $q
# The JOURNAL under a foreign transaction: an fsync/a sync WAIT (a diagnosis after a
# second) rather than going around the journal — earlier after ~1 s of waiting the fsync
# returned 0 having written only the data in place, without the metadata.
rm -f /tmp/o.log
q=$(qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 2 -m 512 -overcommit cpu-pm=on \
    -kernel "/w/$K" -append "console=ttyS0 talaria.test=jrnwaitdiag" -display none -serial file:/tmp/o.log -no-reboot \
    -drive id=d0,file=/img/disk_ext2.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!)
for i in $(seq 1 20); do sleep 1; grep -aq "JRNWAIT: OK\|JRNWAIT: FAIL\|panic" /tmp/o.log 2>/dev/null && break; done
assert "ext2: the journal is busy with a foreign transaction for [0-9]+ ms \(the task [0-9]+ at the phase .the descriptor\+the copies., its transaction has been going [0-9]+ ms\), the task [0-9]+ keeps waiting — we do not write the metadata in place \(give-ups 0\)" "jrnwaitdiag: the diagnosis names the holder AND THE PHASE it stands at, zero give-ups"
assert "ext2: the journal is busy with a foreign transaction for [0-9]+ ms \(the task [0-9]+ at the phase .the commit." "jrnwaitdiag: the holder has changed its phase — the diagnosis names the NEW one rather than the remembered one"
assert "JRNWAIT: the wait by the holder.s phases: .the descriptor\+the copies. 1 episode\(s\) [5-9][0-9]{2} ms, .the commit. 1 episode\(s\) [5-9][0-9]{2} ms" "jrnwaitdiag: the waiting is broken down over the holder's phases evenly rather than dumped into one"
assert "JRNWAIT: OK" "jrnwaitdiag: a sync from an AP finishes only AFTER the journal is released, zero bypasses of the journal"; stopq $q
# THE STEP 2 OF SESSION 40: THE JOURNAL IS NOT BUSY FOR THE TIME OF A FOREIGN FLUSH OF
# DATA. The instrument of session 39 named the owner of the wait for a foreign transaction:
# 99 % is the holder's phase "the data in place" (the worst episode 257-385 ms), and this
# phase does not touch the journal at all. The gate holds the data phase for 600 ms and
# measures the neighbour TWICE on one boot with the knob talaria.jrnfree (the rule of
# session 39: an A/B by a knob rather than with last week): with 0 the neighbour must stand
# for hundreds of ms AND write an episode into that phase, with 1 it must not wait at all.
rm -f /tmp/o.log
q=$(qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 2 -m 512 -overcommit cpu-pm=on \
    -kernel "/w/$K" -append "console=ttyS0 talaria.test=jrnfreedata" -display none -serial file:/tmp/o.log -no-reboot \
    -drive id=d0,file=/img/disk_jrn.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!)
for i in $(seq 1 30); do sleep 1; grep -aq "JRNFREE: OK\|JRNFREE: FAIL\|panic" /tmp/o.log 2>/dev/null && break; done
assert "JRNFREE: the holder stretches the data phase for 600 ms; the neighbour: with talaria.jrnfree=0 it waited [3-9][0-9]{2} ms \(the episodes of .the data in place. 1\), with jrnfree=1 — [0-9]{1,3} ms \(the episodes 0\)" "jrnfreedata: the former behaviour cost the neighbour hundreds of milliseconds, and the instrument sees it"
assert "JRNFREE: OK" "jrnfreedata: the journal is free for the time of a flush of data — the neighbour no longer waits for a foreign data phase"; stopq $q
# THE STEP 1 OF SESSION 40 (the second half): THE FLUSH BY AGE. The instrument of session
# 39 said there is something to flush pre-emptively: by the moment of the flush a dirty
# block is 2.7-3.2 s old, while the threshold of §8.7 does not relate to this path at all
# (45-50 blocks per call with a threshold of 32768). The gate requires a block that has
# lain to go BEFORE the fsync, while with talaria.dirtyage=0 everything stays as it was — otherwise there is nothing to judge the A/B on the stand by.
rm -f /tmp/o.log
q=$(qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 2 -m 512 -overcommit cpu-pm=on \
    -kernel "/w/$K" -append "console=ttyS0 talaria.test=dirtyage" -display none -serial file:/tmp/o.log -no-reboot \
    -drive id=d0,file=/img/disk_dcnt.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!)
for i in $(seq 1 30); do sleep 1; grep -aq "DIRTYAGE: OK\|DIRTYAGE: FAIL\|panic" /tmp/o.log 2>/dev/null && break; done
assert "DIRTYAGE: talaria.dirtyage=0: 0 blocks were flushed by age" "dirtyage: a switched-off knob takes nothing away — the former behaviour for the A/B"
assert "DIRTYAGE: talaria.dirtyage=200: [1-9][0-9]* blocks were flushed by age \(the average age is [34][0-9]{5} us\)" "dirtyage: a block that has lain for 400 ms is taken away by the NEXT write rather than by the fsync"
assert "DIRTYAGE: OK" "dirtyage: with the knob on, an fsync carries less dirty data than with it off"; stopq $q
# THE AGE OF A DIRTY ONE IS A SIGNED DIFFERENCE OF THE STAMPS (session 41). The stand
# printed "the worst age 1 553 501 078 413 us" (18 days) with a boot of five minutes: the
# selection "older than the threshold" counted the difference unsigned, while a block's
# stamp is sometimes NEWER than the snapshot (the FS lock releases a pass for the time of
# the device, §8.6). The freshest block looked infinitely old and went off in a pre-emptive flush.
rm -f /tmp/o.log
q=$(qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
    -kernel "/w/$K" -append "console=ttyS0 talaria.test=agewrap" -display none -serial file:/tmp/o.log -no-reboot >/tmp/q.log 2>&1 & echo $!)
for i in $(seq 1 15); do sleep 1; grep -aq "AGEWRAP: OK\|AGEWRAP: FAIL\|panic" /tmp/o.log 2>/dev/null && break; done
assert "AGEWRAP: OK" "agewrap: a stamp from the future is an age of 0 and \"it has not lain\" rather than 4 294 967 295 units"; stopq $q
# THE STEP 1 OF SESSION 41: THE PRICE OF THE PRE-EMPTION MUST BE SMEARED. The stand of
# session 40 showed the knob's other side: it does take the data off the fsync call
# (47 -> 9.4 blocks), but the write pays with a BATCH (44 flushes of 136 blocks) and the
# insert's p99.9 is four times worse. The batch was a consequence of the construction — the
# period of the kick EQUALLED the age threshold. The gate carries ONE debt of 400 blocks
# that have lain twice on one boot (an A/B by a knob, the rule of session 39) and requires:
# the worst smeared flush is no larger than the kick's ceiling, the debt has gone in full, and a kick over a quiet cache does not walk the cache.
rm -f /tmp/o.log
q=$(qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 2 -m 512 -overcommit cpu-pm=on \
    -kernel "/w/$K" -append "console=ttyS0 talaria.test=dirtysmear" -display none -serial file:/tmp/o.log -no-reboot \
    -drive id=d0,file=/img/disk_dcnt.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!)
for i in $(seq 1 40); do sleep 1; grep -aq "DIRTYSMEAR: OK\|DIRTYSMEAR: FAIL\|panic" /tmp/o.log 2>/dev/null && break; done
assert "DIRTYSMEAR: in a batch \(the kick=the threshold, the ceiling 256\): the flushes [1-9][0-9]*, the blocks [4-9][0-9]{2}, THE WORST flush 2[0-9]{2} blocks" "dirtysmear: the former construction pays with a BATCH — hundreds of blocks in one flush (that is the price of the tail)"
assert "DIRTYSMEAR: smeared \(the defaults\): the flushes [1-9][0-9]+, the blocks [4-9][0-9]{2}, THE WORST flush (16|32|64) blocks" "dirtysmear: the same debt is taken away by flushes EIGHT times smaller — and it is taken away in full"
assert "DIRTYSMEAR: the silence \(everything dirty is younger than the threshold\): the kicks without a walk [3-9][0-9]*, the flushes 0" "dirtysmear: a clock instead of an enumeration — a kick over a quiet cache costs two comparisons rather than a pass"
assert "DIRTYSMEAR: OK" "dirtysmear: the price of the pre-emption is smeared, the debt has gone, the DN_OLD clock has been verified by a full recount"; stopq $q
# THE PHASES OF AN fsync MUST NAME THE OWNER OF THE TIME. Session 36: an fsync of 5173 us
# per call with a sum of the phases of 3939 — 1.2 ms belonged to nobody, and there was
# nothing to fix the write path with. The gate creates a wait for a FOREIGN transaction on
# purpose (an AP holds the journal for 333 ms) and requires it to be named by its own phase rather than by the remainder "OTHER".
rm -f /tmp/o.log
q=$(qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 2 -m 512 -overcommit cpu-pm=on \
    -kernel "/w/$K" -append "console=ttyS0 talaria.test=fsyncph" -display none -serial file:/tmp/o.log -no-reboot \
    -drive id=d0,file=/img/disk_ext2.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!)
for i in $(seq 1 20); do sleep 1; grep -aq "FSYNCPH: OK\|FSYNCPH: FAIL\|panic" /tmp/o.log 2>/dev/null && break; done
assert "FSYNCPH: under a foreign transaction: the fsync [0-9]+ us, the phase .a foreign transaction. 3[0-9]{5} us \(1 times\)" "fsyncph: the wait for a foreign transaction fell entirely into ITS OWN phase (the holder's 333 ms)"
assert "FSYNCPH: OK" "fsyncph: the phases cover the fsync — the remainder \"OTHER\" is small both in a quiet call and under a neighbour"; stopq $q
# THE COUNTERS OF THE DIRTY ONES INSTEAD OF WALKS OVER THE WHOLE CACHE (session 39). The
# instrument of the phases gave 320 us per fsync call — 10 % of its price — of pure
# processor for three walks over 131072 slots: `jrn_pending` twice and a second pass of
# `writeback_locked` which has nothing to write. Now these are counters, and a counter over
# four arrays at once pays for a missed wrapper not with slowness but with the LOSS of a
# dirty block under a successful fsync code. The gate compares the counters with a full
# recount at every turn of a slot's life, requires ZERO calls of the second pass on an
# fsync of data (and a clean cache after a sync — that is, the metadata is not lost) and
# checks that the price of the counting does not grow with the cache: 8 -> 128 MB (an enumeration would have grown sixteenfold).
rm -f /tmp/o.log
q=$(qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 2 -m 1024 -overcommit cpu-pm=on \
    -kernel "/w/$K" -append "console=ttyS0 talaria.test=dcnt" -display none -serial file:/tmp/o.log -no-reboot \
    -drive id=d0,file=/img/disk_dcnt.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!)
for i in $(seq 1 40); do sleep 1; grep -aq "DCNT: OK\|DCNT: FAIL\|panic" /tmp/o.log 2>/dev/null && break; done
assert "DCNT: the fsync \(the code 0\): the dirty ones [0-9]+ \(the data 0, the metadata [1-9][0-9]*\), waiting for the journal are 0\+0, the second pass over the cache 0 times" "dcnt: on an fsync of DATA there is no second pass over the whole cache — with dirty metadata in the cache"
assert "DCNT: the sync \(the code 0\): the dirty ones 0" "dcnt: the metadata not taken by the second pass is taken by the sync — nothing dirty stays in the cache"
assert "DCNT: the same counting with talaria.fscnt=0 \(a walk over the cache of 128 MB\) — [0-9]{4,} cycles per call" "dcnt: the A/B knob talaria.fscnt=0 HONESTLY brings the former enumeration back (otherwise a measurement on the stand would have compared nothing)"
assert "DCNT: OK" "dcnt: the counters of the dirty ones agree with a full recount, the price of the counting does not grow with the cache's size"; stopq $q
# THE INSTRUMENT OF THE STEP 2 OF SESSION 39: THE AGE OF A DIRTY ONE. The candidate "a
# pre-emptive flush by age" (Linux's dirty_expire_centisecs) exists exactly as long as the
# age of a dirty one is not measured for us: a flush within single milliseconds of the
# write means that Linux's background writeback would not have managed to take that block
# away either. An instrument that returns zero by construction is worse than an absent one —
# the gate separates "a flush at once" and "a flush after 300 ms" and requires it to TELL them apart.
rm -f /tmp/o.log
q=$(qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 2 -m 512 -overcommit cpu-pm=on \
    -kernel "/w/$K" -append "console=ttyS0 talaria.test=wbage" -display none -serial file:/tmp/o.log -no-reboot \
    -drive id=d0,file=/img/disk_dcnt.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 & echo $!)
for i in $(seq 1 30); do sleep 1; grep -aq "WBAGE: OK\|WBAGE: FAIL\|panic" /tmp/o.log 2>/dev/null && break; done
assert "WBAGE: a flush after 300 ms: the portions 1, the blocks 1, this portion.s age [23][0-9]{5} us" "wbage: the age of a dirty one is measured — a block that has lain for 300 ms is seen as 300 ms"
# THE STEP 1 OF SESSION 40: the age must be ascribed to THE ONE who took the block away.
# The "2.7-3.2 s" of session 39 are averaged over all the populations at once, while the
# knob "a flush by age" exists only for the fsync population: if the seconds belong to the
# threshold from a write or to an eviction, the fsync has nothing to pre-empt. The gate has
# been checked on a broken instrument (the threshold's portion written into the fsync population) — it fails on the third line.
assert "WBAGE: a flush by the write.s threshold: the portions 1, the blocks 1, this portion.s age [23][0-9]{5} us, the worst [23][0-9]{5} us; the fsync population is untouched: the portions \+0, the blocks \+0" "wbage: a block taken away by the threshold of §8.7 from a write lies in ITS OWN population and is not mixed into the fsync's"
assert "WBAGE: OK" "wbage: the age instrument tells a flush at once from a flush after 300 ms, the data per fsync call is counted"; stopq $q
# A DEVICE'S REFUSAL REACHES THE fsync. The device is replaced with a refusing one at the
# level of the block layer: an fsync/a sync answer EIO (rather than 0 — as earlier, when
# the refusal was printed and swallowed), the FS goes read-only (EROFS), and after the
# device comes back the data reads back as what was written.
q=$(bootd "talaria.test=ioerr" /img/disk_ext2.img)
for i in $(seq 1 20); do sleep 1; grep -aq "ioerr: ALL OK\|ioerr: FAILED\|panic" /tmp/o.log 2>/dev/null && break; done
assert "ext2: A DEVICE REFUSAL at a write \(the refusals [0-9]+\) — the FS has been moved to read-only" "ioerr: the first refusal of a write is announced and moves the FS into read-only"
assert "ioerr: ALL OK" "ioerr: an fsync/a sync at a device's refusal -> EIO, the changes -> EROFS, reading is allowed, after the device comes back the file is intact"; stopq $q
# THE PATHS WITHOUT AN EXIT INTO THE HOST. The acknowledgement of an interrupt (a PV EOI)
# and a ring to a neighbouring core (a PV IPI) are the two most frequent operations of the
# kernel, and on a stand without APICv each of them through the LAPIC costs about ten
# microseconds of emulation. Under KVM (and the whole set runs under KVM) both protocols
# must come up: a silent fallback onto the LAPIC would cost 4 % of the processor on a tick and two to five times that on a thread's wake-up.
q4=$(bootd4 "talaria.test=mbench talaria.smp=4" /img/disk_ext2.img); sleep 6
assert "smp: the KVM PV EOI is on" "PV EOI: the acknowledgement of an interrupt goes without an exit into the host"
assert "smp: the KVM PV IPI is on" "PV IPI: a ring to a neighbouring core goes as one hypercall"; stopq $q4
q4=$(bootd4 "talaria.test=fputest talaria.smp=4" /img/disk_ext2.img); sleep 8
assert "fputest: ALL OK" "fputest: the YMM state survives a syscall, a signal handler and a preemption (without a layout only the BSP preempts)"; stopq $q4
# THE KERNEL'S TICK BEFORE THE FPU IS SAVED. A tick is an interrupt in the middle of the
# workload's arbitrary code, and everything the scheduler does before the XSAVE goes on the
# workload's registers. The kernel is built with SSE, and the compiler readily zeroes an
# array through `xorps xmm0` — that is how the push of the stealing (`talaria.stealpush=1`)
# on an AP's tick zeroed xmm0 BEFORE the saving, and SObjectizer crashed in `find_handler`
# with ZERO migrations (session 26). It is caught only where the tick hits an AP with a
# neighbour on the core: without a layout the AP cores are "dedicated" and do not tick at
# all — that is why the former run above did not see this, and "on 4 cores" there was on
# one. Here the `spread` layout makes the cores shared, the knobs switch the scheduler's
# work in the tick on (the push, the histogram of the ready ones), and on the donor core the tick must give the workload its own registers back.
q4=$(bootd4 "talaria.test=fputest talaria.smp=4 talaria.sched=spread talaria.stealpush=1 talaria.runhist=1" /img/disk_ext2.img); sleep 8
assert "fputest: 8 threads on 4 cores" "fputest-tick: there are twice as many threads as cores — every core has a neighbour, the tick is not a \"lonely\" one"
assert "stat:   t[0-9]+: [0-9]+ ticks .*core [123]" "fputest-tick: the ticks REALLY hit the AP cores (otherwise the gate would be checking only the BSP)"
assert "fputest: ALL OK" "fputest-tick: an AP's tick with the push of the stealing and the histogram of the ready ones gives the workload its xmm/ymm back (the zeroing of an array in the tick went BEFORE the XSAVE)"
refute "ymm[0-9]+ is spoilt" "fputest-tick: not a single thread saw foreign/zero registers"; stopq $q4
q=$(bootd4 "talaria.mode=cell talaria.smp=4" /img/disk_sched.img); sleep 2
assert "sched-plan: rules 1, .*the scheduler.s cores 0xf" "cell-plan: the plan of the layout has been read FROM THE MANIFEST on the disk (talaria-pack --pin)"
assert "SPREAD: threads per core: 2 2 2 2" "cell-plan: the spreading by the manifest on the production path (mode=cell + talaria.smp=4)"
assert "SPREAD: all the 8 threads reached the end" "cell-plan: all the threads finished counting under the supervisor"
stopq $q
# NAMED POOLS: the operator names the cores ONCE (`pool=`), and the threads are sent into
# a pool by a pattern of the name. The spreading and the balancing go INSIDE the pool; the
# pool's cores are given neither to the dedicated ones nor to the threads of the common pool.
q=$(boot2 "talaria.test=schedpin talaria.pool=fast:1,2 talaria.pool=slow:3 talaria.pin=md_*:fast,bg*:slow" "-smp 4 -overcommit cpu-pm=on"); sleep 4
assert "sched-plan:   the pool fast = the cores 0x6" "schedpool: pool=fast:1,2 has been parsed into a mask of cores"
assert "sched-plan:   md_\* -> the pool fast" "schedpool: the rule sends the threads by a pattern of the name INTO THE POOL"
assert "PIN: it has settled: the dedicated cores [12],[12]; the background ones on 3 3 3 3" "schedpool: the threads went into their own pools and did NOT leak between them"
stopq $q
q=$(boot2 "talaria.test=schedspread talaria.sched=spread" "-smp 4 -overcommit cpu-pm=on")
assert "SPREAD: threads per core: 2 2 2 2" "schedspread: eight unpinned threads spread EXACTLY 2 onto each of the 4 cores"
assert "SPREAD: 8 threads on 4 cores, 2\.\.2 per core, all reached the end ALL OK" "schedspread: both threads of EVERY core of the pool finished counting — there is preemption on a secondary core"
stopq $q
# The pinning by the THREAD'S NAME with the goal "a dedicated core": the number is chosen
# by the OS. The check is by the settled state (getcpu) rather than by the moment of the
# renaming: the core given away as a dedicated one could be busy with an executing thread,
# and the resettling is finished by the balancer.
q=$(boot2 "talaria.test=schedpin talaria.pin=md_*:dedicated,bg*:shared" "-smp 4 -overcommit cpu-pm=on"); sleep 4
assert "PIN: the name -> the rule -> a dedicated core ALL OK" "schedpin: pin=md_*:dedicated — every thread has its own core (the number was chosen by the OS), there are no background ones on them"
assert "PIN: the thread md_feed: .*the name from the OS kernel: md_feed" "schedpin: prctl(PR_SET_NAME/PR_GET_NAME) — the thread's name reached the OS kernel and came back"
stopq $q
# The main thread leaves the control plane: the BSP stays idle, and that is a separate path
# of entering the scheduler (`sched::start_idle`) rather than a branch of the old one.
# THE BALANCER RESETTLES. The skew is created by the rule `pin=*:1` — all the nameless
# threads lie down onto one core (a pattern rule does not "pin them down"). The layout at
# creation has already worked here, the stealing of work does not touch such a skew (the
# rule "improve the balance"), which means that ONLY the periodic balancing can resettle
# them. In session 20 it was switched on and did not work: the clock was the BSP's ticks,
# and the tick's period stopped being a constant, while the demand was refreshed as often
# as it was read. The numbers then: the skew is seen as 1.9 threads instead of 6, found 0
# times out of 12 passes.
q=$(boot2 "talaria.test=schedbal talaria.pin=*:1" "-smp 4 -overcommit cpu-pm=on"); sleep 6
assert_slow "BAL: the skew [0-9]+ cores -> 4 cores, per core [0-9]+\.\.[0-9]+ — RESETTLED ALL OK" "schedbal: eight threads that had settled on ONE core have been resettled by the balancer over all four"
assert "stat: balancing: the greatest skew ([3-9][0-9][0-9]|[0-9]{4,}) threads" "schedbal: the demand is HONEST — eight threads on a core are seen as a skew of six rather than of two (while the demand was refreshed once per pass it never grew to the threshold)"
refute "BAL: FAIL" "schedbal: all the threads finished counting and spread out (not a single FAIL from the workload)"
stopq $q
# THE STEALING WITH A LIVE POOL OF SLEEPING ONES (session 25). Two computing threads lie
# down onto one core (`pin=*:1`), and twelve SLEEPING ones are next to them — and the
# stealing must resettle them: the periodic balancing is deliberately off, otherwise the
# gate would not be about the stealing. The sleeping ones here are the test itself: the
# balance of the stealing was counted by the number of ASSIGNED tasks, and a core with
# twelve sleeping ones looked more loaded than a core with two computing ones. The second
# defect of the same place is the cooling threshold: two ready threads on one core replace
# each other every slice and NEVER cool down. On ClickHouse that cost a whole fourth core
# (110 ms against Linux's 84 with max_threads=4). It has been checked both ways: with
# `talaria.nosteal=1` the gate is RED — both computing threads stay on the core 1 ("the
# cores after the counting: 1 1"), while there are a thousand and a half rings to the
# idling cores over a run. That is, the gate holds the whole path: both the ring to a sleeping core and the selection of a candidate.
q=$(boot2 "talaria.test=stealbal talaria.pin=*:1 talaria.nobalance=1" "-smp 4 -overcommit cpu-pm=on"); sleep 6
assert_slow "STEAL: the computing ones are on 2 different cores, both finished counting — SEPARATED ALL OK" "stealbal: the stealing separated two computing threads from one core without being deceived by the twelve sleeping ones next to them"
assert "stat: work steals=[1-9][0-9]*" "stealbal: the mechanism REALLY fired — there are more than zero steals (otherwise \"they spread out\" could mean anything)"
refute "STEAL: FAIL" "stealbal: both computing threads finished counting to the end"
stopq $q
# Session 26: THE PUSH — a donor on a tick gives a spare ready one to an empty neighbour
# itself (`talaria.stealpush=1`). The stealing is switched OFF here (`nosteal=1`) —
# otherwise it is unclear who resettled them, and the gate is about the push. The
# ClickHouse shape: the thief has no ready ones of its own, the donor has two, and the
# thief does not enter the scheduler (its only task sleeps) — only the donor can resettle.
q=$(boot2 "talaria.test=stealbal talaria.pin=*:1 talaria.nobalance=1 talaria.nosteal=1 talaria.stealpush=1" "-smp 4 -overcommit cpu-pm=on"); sleep 6
assert_slow "STEAL: the computing ones are on 2 different cores, both finished counting — SEPARATED ALL OK" "stealpush: the push separated two computing threads from one core with the stealing switched off"
assert "stat: the push: moves [1-9][0-9]*" "stealpush: the migration REALLY was a push — the counter is above zero"
assert "stat: work steals=0," "stealpush: the stealing is off — it was not the thief who resettled them"
refute "STEAL: FAIL" "stealpush: both computing threads finished counting to the end"
stopq $q
# Session 26: the metric "READY BUT NOT RUNNING" (stat: ready->cpu). Before it, the ones
# waiting for the processor were visible only through a wake-up, while a thread preempted
# by the quantum was woken by nobody — on ClickHouse the question "do the threads wait for
# each other or for the processor" stayed unanswered. The gate judges the metric by a
# workload with a known answer: three computing threads on ONE core (the stealing and the
# balance are off) — at any moment two are waiting, the sum of the waiting is about two
# walls of the counting (the measurement: 1151 ms with a wall of 574; every interval is
# ~1000 us = two foreign quanta of 500). The threshold is 100 % of the whole run's wall
# (the loading of the binary is in the wall too). The control is the same threads on four cores: a share of 0-8 % (the cost of the resettling at the start).
q=$(boot2 "talaria.test=rqwait talaria.pin=*:1 talaria.nobalance=1 talaria.nosteal=1" "-smp 4 -overcommit cpu-pm=on"); sleep 4
assert_slow "RQWAIT: the 3 threads finished counting, the wall of the counting is [0-9]+ ms — ALL OK" "rqwait: three computing threads on one core finished counting"
assert "RQWAIT: the ready ones waited for the processor .* the share of the waiting is ([1-9][0-9]{2,})%" "rqwait: on one core the ready ones waited for the processor for no less than the wall (two of the three threads are always in the queue)"
assert "stat: ready->cpu, us: .* <1000:[1-9][0-9]{2,}" "rqwait: the intervals of the waiting are hundreds of pieces of two foreign quanta (500-1000 us) rather than one long one"
stopq $q
q=$(boot2 "talaria.test=rqwait" "-smp 4 -overcommit cpu-pm=on"); sleep 3
assert_slow "RQWAIT: the ready ones waited for the processor .* the share of the waiting is ([0-9]|1[0-9]|2[0-4])%" "rqwait: the control — on four cores the same threads hardly wait (the share < 25 %): the metric does not invent the waiting"
stopq $q
q=$(boot2 "talaria.test=schedspread talaria.pin=main:dedicated" "-smp 4 -overcommit cpu-pm=on")
assert "sched: the control plane is idling" "schedmain: pin=main:dedicated takes the main thread off the core 0, the BSP goes idle"
assert "sched: all the 9 task\(s\) have finished" "schedmain: the workload ran to the end with an idling BSP (the end of the mode is visible from the idle context)"
stopq $q
# ── NUMA: TWO NODES and the second one's memory ABOVE 4 GB ──────────────────
# There is no two-socket machine at home, but QEMU can announce two nodes in the SRAT — and
# that is exactly what was missing: without such a stand the whole NUMA path (the map of
# the nodes, the per-node areas of memory, "the threads and the memory on one node") would
# be checked only on hardware at 6 $/hour. The memory of the node 1 deliberately lies above
# 4 GB — as on a real machine, and that also checks the mapping of such frames by 1 GB.
bootnuma() {
  rm -f /tmp/o.log
  qemu-system-x86_64 -M q35 -enable-kvm -cpu "$CPU" -m 6144 -overcommit cpu-pm=on \
    -smp 4,sockets=2,cores=2,threads=1 \
    -object memory-backend-ram,size=3072M,id=m0 -object memory-backend-ram,size=3072M,id=m1 \
    -numa node,nodeid=0,memdev=m0,cpus=0-1 -numa node,nodeid=1,memdev=m1,cpus=2-3 \
    -kernel "/w/$K" -append "console=ttyS0 $1" -display none -serial file:/tmp/o.log -no-reboot \
    -drive id=d0,file=$2,format=raw,if=none -device nvme,drive=d0,serial=tal001 >/tmp/q.log 2>&1 & echo $!
}
if [ -f /img/disk_pool.img ]; then
  q=$(bootnuma "talaria.pci=1 talaria.mode=cell talaria.smp=4" /img/disk_pool.img); sleep 3
  assert_slow "acpi: SRAT: the NUMA nodes 2" "numa: two nodes have been parsed from the SRAT (the map core -> node)"
  assert_slow "mem:   node 1: 0x1[0-9a-f]+\.\.0x[0-9a-f]+ \(3072 MB\)" "numa: the node 1 has ITS OWN area of memory, and it is above 4 GB"
  assert_slow "mem: the high area 0x100000000\.\.0x140000000 \(1024 MB\)" "numa: node 0's own gigabyte above 4 GB is its high area — node 1's memory is not taken for it"
  assert_slow "sched-plan: the NUMA nodes 2 .the mask 0x3." "numa: the scheduler sees both nodes"
  assert_slow "sched-plan: the workload -> the NUMA node 1" "numa: node=1 from the manifest has been applied (the node is named by the operator)"
  assert_slow "cell: the heap 0x1[0-9a-f]+ is allocated \(node 1\)" "numa: the workload's heap is ACTUALLY allocated on the node 1 (the frames above 4 GB are mapped)"
  assert_slow "SPREAD: all the 8 threads reached the end" "numa: the workload on the memory of a foreign node ran to the end"
  assert_slow "sched-plan:   the pool bg = the cores 0xe" "numa: the pool from the manifest has been parsed on this same machine"
  # The workload's memory goes onto the node IN FULL rather than only the heap: the FS's
  # block cache is allocated at the mount — earlier than the kernel learns about the nodes
  # at all (the manifest lies on the same disk), so it is moved after the node is chosen.
  # The cache's size is no longer a build constant (it is chosen by the free memory), and
  # what has to be checked is the MOVE onto the node rather than how many megabytes moved.
  assert_slow "ext2: the block cache has been moved onto the node 1 .0x[0-9a-f]+ -> 0x1[0-9a-f]+, [0-9]+ MB." "numa: the FS's block cache has been moved onto the workload's node"
  stopq $q
  # A node the machine does not have must be rejected LOUDLY rather than silently.
  q=$(bootnuma "talaria.pci=1 talaria.mode=cell talaria.smp=4 talaria.sched=node=7" /img/disk_sched.img); sleep 3
  assert_slow "SPREAD: all the 8 threads reached the end" "numa-bad: the workload ran (a non-existent node does not break the launch)"
  stopq $q
fi

# A rule onto a core the machine does not have must be LOUD: a silent success is worse than
# a refusal (the pitfall of session 10, sched_setaffinity -> EINVAL).
q=$(boot2 "talaria.test=schedspread talaria.pin=main:9" "-smp 4 -overcommit cpu-pm=on")
assert "sched-plan: ATTENTION — the core 9 is not among the scheduler.s cores" "schedbad: a pin= onto a non-existent core -> a warning into the log (rather than a quiet substitution)"
stopq $q
# A10: SPARSE APIC ids — the topology of real hardware. On a c6i.metal the MADT gives
# 0, 2, 4, 6, … (the HT twins are between them), and until A10 the kernel confused the two
# roles of one number: an AP came up as "the core 2" while the workload asked for the core
# 1 — sched_setaffinity silently returned success, the thread stayed on the BSP, and there
# was no SMP on the metal at all. The test's own metric did not catch this: the worker,
# staying on the BSP, took half of main's slices, t1 doubled, and the speedup = 2*t1/both
# still gave ~2.0 — "ALL OK" without a single AP. So there are two checks: the parallelism
# itself and getcpu ("WHICH core the thread ended up on").
# The topology is assembled exactly as on the metal: one boot core + cores added one per
# socket — QEMU gives them the APIC ids 2, 4, 6.
# The gate is here because on a metal one such run costs about 2 $.
bootsparse() {
  rm -f /tmp/o.log
  qemu-system-x86_64 -M q35 -enable-kvm -cpu "$CPU" -m 1024 -overcommit cpu-pm=on $1 \
    -kernel "/w/$K" -append "console=ttyS0 $2" -display none -serial file:/tmp/o.log -no-reboot \
    -drive id=d0,file=/img/disk.img,format=raw,if=none -device virtio-blk-pci,drive=d0 >/tmp/q.log 2>&1 & echo $!
}
SPARSE2="-smp 1,maxcpus=4,sockets=2,cores=1,threads=2 -device host-x86_64-cpu,socket-id=1,core-id=0,thread-id=0,id=c1"
SPARSE4="-smp 1,maxcpus=8,sockets=4,cores=1,threads=2 -device host-x86_64-cpu,socket-id=1,core-id=0,thread-id=0,id=c1 -device host-x86_64-cpu,socket-id=2,core-id=0,thread-id=0,id=c2 -device host-x86_64-cpu,socket-id=3,core-id=0,thread-id=0,id=c3"
# A12: the LAPIC timer's ABILITY to interrupt the core in itself. The check takes seconds
# and depends neither on the workload nor on the scheduler — and on a `c6i.metal` it was
# exactly here that the limit turned out to be: the production mode reached ring 3 and fell
# silent because the preemption did not start at all (the timer counts, the interrupts are zero).
# A12: AN ABSENT port must not be taken for a source of data. On a port that is not there
# the bus floats and all the registers read as 0xFF — that is, "a byte is ready" always.
# The console's poll without a bound was called from the tick's handler, and on a
# `c6i.metal` (there is no COM1 there) the very first tick went into an infinite loop
# INSIDE the interrupt: ring 3 is alive, the syscalls go, the timer ticks, and there is no
# preemption. The project's most expensive pitfall — six runs at 3 $ each.
q=$(boot "talaria.test=serial")
assert "serial-test: a floating bus .0xFF. was NOT taken for data" "serial: the LSR of an absent port is not counted as data (0xFF != a byte)"
assert "serial-test: the console.s poll returns" "serial: the console's poll loop is bounded (the body of an interrupt handler does not spin for ever)"
assert "serial-test: ALL OK" "serial: both checks of a dead port have passed"
stopq $q
# Where the devices' interrupts arrive. What is checked is the MECHANICS: the message's
# address is written into the MSI-X table and READ BACK — "it has been written" and "the
# device will see it" are different statements. A live card (ENA) stays for EC2; here is
# what can be checked at home, and a wrong address means an interrupt quietly arriving at a
# foreign core — including at a dedicated one, for whose silence everything was done.
q=$(bootsparse "$SPARSE2" "talaria.pci=1 talaria.test=msix")
assert_slow "msix: ALL CHECKS PASSED" "msix: the address of an MSI-X message is written and read back; the interrupts are not given to a dedicated core"
assert_slow "msix: OK   ALL the table.s vectors have been moved rather than the first one" "msix: the redirection is applied to the whole table of the vectors"
stopq $q
q=$(bootsparse "$SPARSE2" "talaria.test=timer")
assert_slow "timer-test: ALL OK — the timer interrupts the core" "timer: the periodic LAPIC timer interrupts the core (without it there is no preemption)"
stopq $q
q=$(bootsparse "$SPARSE2" "talaria.test=smp2")
assert_slow "smp: the topology: 2 logical cores .the BSP.s apic 0. out of 2 by the MADT, apic_of.0\.\.2.=.0, 2." "smp2-sparse: the map of the logical cores has been built by the MADT (the APIC ids 0, 2 — as on a metal)"
assert_slow "SMP2: .* cpu=1" "smp2-sparse: the thread REALLY ended up on the logical core 1 (getcpu already from the AP)"
assert_slow "SMP2: the parallel speedup [12]\.[0-9]+ .* ALL OK" "smp2-sparse: the parallelism on sparse APIC ids (the topology of bare metal)"
stopq $q
# A10: EIGHT cores with sparse ids — a check that the ceiling of 4 cores has been removed
# for real (percpu::MAXCPU 4 -> 64), and on the PRODUCTION path (mode=cell +
# talaria.smp=N) rather than only in a test mode. The topology is like a metal's.
SPARSE8="-smp 1,maxcpus=16,sockets=8,cores=1,threads=2"
for _s in 1 2 3 4 5 6 7; do SPARSE8="$SPARSE8 -device host-x86_64-cpu,socket-id=$_s,core-id=0,thread-id=0,id=c$_s"; done
# The remainder of A12: SIXTEEN cores with sparse ids — and a check that the ONLINE mask is
# full RIGHT AFTER the bring-up. "A core is alive" and "a core is ready" are different
# events: an AP is marked alive before it gets into the mask, and on a c6i.metal with
# talaria.smp=16 the mask was printed as 0x7fff (the last AP did not manage it).
# The gate catches exactly the mask's completeness rather than merely "something came up".
SPARSE16="-smp 1,maxcpus=32,sockets=16,cores=1,threads=2"
for _s in $(seq 1 15); do SPARSE16="$SPARSE16 -device host-x86_64-cpu,socket-id=$_s,core-id=0,thread-id=0,id=c$_s"; done
q=$(bootsparse "$SPARSE16" "talaria.smp=16 talaria.mode=cell")
assert_slow "smp: the topology: 16 logical cores" "smp16-sparse: sixteen cores with sparse APIC ids have been parsed from the MADT"
assert_slow "the SMP scheduler: the online cores mask=0xffff" "smp16-sparse: the ONLINE mask is FULL right after the bring-up (start_cpu waits for the readiness rather than for the life)"
stopq $q
q=$(bootsparse "$SPARSE8" "talaria.smp=8 talaria.mode=cell")
assert_slow "smp: the topology: 8 logical cores .the BSP.s apic 0. out of 8 by the MADT" "smp8-sparse: eight cores with the APIC ids 0, 2, .., 14 have been parsed from the MADT"
assert_slow "the SMP scheduler: the online cores mask=0xff" "smp8-sparse: all the 8 cores have been brought up on the production path (mode=cell + talaria.smp=8) — the ceiling of 4 cores is removed"
stopq $q
q=$(bootsparse "$SPARSE4" "talaria.test=smp4")
assert_slow "smp: the topology: 4 logical cores .the BSP.s apic 0. out of 4 by the MADT, apic_of.0\.\.4.=.0, 2, 4, 6." "smp4-sparse: four cores with the APIC ids 0, 2, 4, 6 (exactly like a c6i.metal's MADT)"
assert_slow "SMP4: the threads are on the cores 1/2/3" "smp4-sparse: three threads on the logical cores 1..3 rather than on the APIC ids 2/4/6"
assert_slow "SMP4: the parallel speedup [234]\.[0-9]+ on 4 cores .* ALL OK" "smp4-sparse: a speedup > 2.5x on four sparse cores"
stopq $q
# A10: a core that is not among the online ones must not be silent. Linux gives an EINVAL —
# so do we; the former "soft success" made a failure of the migration invisible.
q=$(boot2 "talaria.test=smp2" "-smp 1")
assert_slow "sched_setaffinity: the core 1 is not among the online ones .0x1. — EINVAL" "smp2-noap: a sched_setaffinity onto a non-existent core -> EINVAL (rather than a quiet success)"
assert_slow "SMP2: FAIL .aff=-1" "smp2-noap: the test HONESTLY fails without an AP (earlier it printed ALL OK with a speedup of ~2 without a single AP)"
stopq $q
q=$(boot2 "talaria.test=tsync talaria.futexhang=3" "-overcommit cpu-pm=on"); sleep "${TAL_SYNC_SLEEP:-12}"  # an EPYC ~14 s; WAITPKG/SPR: the tier 1 in the SPSC phases ~120 s (the perf-tuning roadmap) — TAL_SYNC_SLEEP=120
assert "TSYNC: ALL OK" "tsync: libtsync — a mutex/MCS/a condvar(+broadcast)/SPSC(+cap2,portable)/MPSC(+portable)/a timedwait on 2 cores (12 checks)"
stopq $q
# THE SAME SET ON A SHORT SLICE. The tier of the waiting must be correct with ANY length of
# the quantum, and it was not: before the fix of `sync::ready_now` this run did not pass A
# SINGLE time (0 out of 4 with 0.5 ms), while with 8 ms it passed twice out of four — that
# is, the gate was not checking correctness but catching luck. It is EXACTLY the short
# slice that has to be checked: the more often the preemption, the more often you land in
# the parking's window, and 0.2 ms is the cheapest way to make a rare race constant.
q=$(boot2 "talaria.test=tsync talaria.slice=200000 talaria.futexhang=3" "-overcommit cpu-pm=on"); sleep "${TAL_SYNC_SLEEP:-12}"
assert "TSYNC: ALL OK" "tsync-slice: the same set with a slice of 0.2 ms — a waiter whose condition is fulfilled is visible to the scheduler (it used to be 0 out of 4)"
stopq $q
q=$(boot2 "talaria.test=syncbench talaria.futexhang=3" "-overcommit cpu-pm=on")
assert "SYNCBENCH the tsync-SPSC .a cv sleep. push->pop latency .*: p50=[0-9]{2,4} ns" "syncbench: an SPSC through a cv sleep — the latency of the wake-up p50 < 10 us"
assert "SYNCBENCH: ALL OK" "syncbench: all the phases (the RTT, the mutex, the spin/tsync SPSC) finished"
stopq $q
# dynsync: its own boot (the dynsync disk + smp 2 + cpu-pm); a SKIP without a patched ld-musl
if [ -f /img/disk_dynsync.img ]; then
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 2 -overcommit cpu-pm=on -m 512 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.test=dynsync" -display none -serial file:/tmp/o.log -no-reboot \
  -drive id=d0,file=/img/disk_dynsync.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 &
q=$!
sleep "${TAL_SYNC_SLEEP:-20}"  # the dynamics: an EPYC ~17 s; for an SPR see TAL_SYNC_SLEEP above
assert "TSYNC: ALL OK" "dynsync: a DYNAMIC tsynctest on a patched ld-musl (the automatic pickup of the PT_INTERP, 12 checks)"
stopq $q
else echo "  SKIP  dynsync: there is no disk_dynsync.img (a patched ld-musl — user/tsync/build-musl.sh)"; fi
q=$(boot "talaria.test=fiber")
assert "FIBER: ALL OK" "fiber: the green threads — a switch/a condvar handoff/an SPSC on one core + swtchto (4 checks)"
assert "FIBER the switch .a yield ping-pong.: [0-9]{1,2} ns/switch" "fiber: a switch at the level of a coroutine (< 100 ns)"
stopq $q
q=$(boot2 "talaria.test=futexlat talaria.futexhang=3" "-overcommit cpu-pm=on")
assert "FUTEXLAT futex cross-core: RTT p50=[0-9]{2,4} ns" "futexlat: a cross-core futex ping-pong (musl) — an mwait wake by a write, p50 < 10 us"
assert "FUTEXLAT: ALL OK" "futexlat: all the 4 phases (futex/cond, same/cross) finished"
stopq $q

echo "== PLAN-SYNC stage 5: an inter-VM wake-up through ivshmem (without a virtio kick/a vmexit) =="
rm -f /dev/shm/talshmwake /tmp/pong.log
python3 -c "import os; fd=os.open('/dev/shm/talshmwake', os.O_CREAT|os.O_RDWR, 0o600); os.ftruncate(fd, 1<<20); os.close(fd)"
qemu-system-x86_64 -M microvm,pcie=on,rtc=off -enable-kvm -cpu "$CPU" -smp 1 -overcommit cpu-pm=on -m 256 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.test=shmwake talaria.shmrole=pong talaria.pci=1" \
  -display none -serial file:/tmp/pong.log -no-reboot \
  -object memory-backend-file,id=hm,mem-path=/dev/shm/talshmwake,size=1M,share=on -device ivshmem-plain,memdev=hm >/tmp/q.log 2>&1 &
QPONG=$!
sleep 2
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,pcie=on,rtc=off -enable-kvm -cpu "$CPU" -smp 1 -overcommit cpu-pm=on -m 256 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.test=shmwake talaria.shmrole=ping talaria.pci=1" \
  -display none -serial file:/tmp/o.log -no-reboot \
  -object memory-backend-file,id=hm,mem-path=/dev/shm/talshmwake,size=1M,share=on -device ivshmem-plain,memdev=hm >/tmp/q2.log 2>&1 &
q=$!
assert "SHMWAKE: the inter-VM RTT through ivshmem .*p50=[0-9]{2,5} ns.* -> OK" "shmwake: two VMs wake each other by a write into ivshmem (an mwait, without a kick), the RTT p50 < 100 us"
kill $q $QPONG 2>/dev/null; wait $q $QPONG 2>/dev/null
rm -f /dev/shm/talshmwake

echo "== the batching of the virtio notify: fastflood (DEVELOPMENT-GUIDE §6b) =="
rm -f /tmp/o.log
qemu-system-x86_64 -M microvm,rtc=off -enable-kvm -cpu "$CPU" -smp 2 -overcommit cpu-pm=on -m 512 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=fastflood talaria.fast=1" -display none -serial file:/tmp/o.log -no-reboot \
  -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
  -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
  -drive id=d0,file=/img/disk.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 &
q=$!
assert "cell-diag: tx=100000 [^k]*kicks=[0-9]{1,4} " "fastflood: the kick is amortised by the batch (<10% of the frames)"
assert "fastflood: 100000 frames" "fastflood: 100k frames went through the cell's rings"
stopq $q

echo "== the outgoing network: DNS + connect (blocking/nonblocking) + ECONNREFUSED + nanosleep + the wall clock =="
python3 /w/kernel/tools/minidns.py 0.0.0.0 10.0.0.1 53 >/tmp/dns.log 2>&1 & DNSP=$!
socat -T5 TCP-LISTEN:5555,bind=10.0.0.1,reuseaddr,fork EXEC:cat >/dev/null 2>&1 & ECHOP=$!
# A dead peer for close-dead: it answers "GO" and HIDES its address — the guest's FIN
# stays without an ACK (see connect_test.c).
ip addr add 10.0.0.9/24 dev tap0 2>/dev/null
python3 -c "
import socket, os, time
l=socket.socket(); l.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); l.bind(('10.0.0.9',5557)); l.listen(4)
c,_=l.accept(); c.send(b'GO'); os.system('ip addr del 10.0.0.9/24 dev tap0'); time.sleep(8)" >/dev/null 2>&1 & DEADP=$!
# A slow peer for close-slow: a receive window of 4 KB, it starts reading after 0.6 s (the
# Internet with an RTT of 0.25 s looks the same); over :5559 it reports how much arrived.
# The orphan's deadline BY THE CLOCK (200 ms) cut it off with an RST at 4 KB out of 48.
python3 -c "
import socket, time, threading
got = [-1]
l = socket.socket(); l.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
l.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096); l.bind(('10.0.0.1', 5558)); l.listen(4)
r = socket.socket(); r.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); r.bind(('10.0.0.1', 5559)); r.listen(4)
def slow():
    c, _ = l.accept(); time.sleep(0.6); n = 0
    try:
        while True:
            d = c.recv(65536)
            if not d: break
            n += len(d)
    except OSError:
        n = -n - 1
    got[0] = n
th = threading.Thread(target=slow, daemon=True); th.start()
r.settimeout(30)
try:
    c, _ = r.accept(); th.join(5); c.send(str(got[0]).encode()); c.close()
except OSError: pass" >/dev/null 2>&1 & SLOWP=$!
sleep 0.5
q=$(boot "talaria.test=connect" net)
assert "wall: 20[2-9][0-9]-.*\(ok\)" "wall-clock: CLOCK_REALTIME from kvmclock (the date >= 2025)"
assert "dns: talaria.test -> 10.0.0.1" "DNS: getaddrinfo through musl's resolver (UDP :53 -> the mini-DNS)"
assert "echo-blocking: 6 PING-B" "connect (blocking) + send/recv"
assert "echo-nonblock: 6 PING-N" "connect (O_NONBLOCK) + poll(POLLOUT) + SO_ERROR"
assert "refused: .*\(ok\)" "a connect to a closed port -> ECONNREFUSED"
assert "refused-nb: .*\(ok\)" "a refusal on a NON-BLOCKING connect: SO_ERROR gives out +ECONNREFUSED (on -111 musl printed \"No error information\"), read gives -ECONNREFUSED (it used to return +111, that is, \"111 bytes have been read\")"
assert "close-dead: .*\(ok\)" "close() with a dead peer returns at once (< 20 ms) rather than waiting for a FIN for 200 ms under the stack's lock; the next connection answers"
assert "close-slow: sent 49152, arrived 49152 \(ok\)" "close() with 48 KB unsent to a slow peer: the orphan's deadline is counted from the last PROGRESS rather than from the close() — the data arrives in full (it used to be an RST by the clock after 200 ms, and on EC2 the generator's report was torn in the middle)"
assert "sleep: .*\(ok\)" "nanosleep of 20 ms (the monotonic clock)"
assert "connect-test: ALL OK" "the outgoing network: the summary ALL OK"
grep -q "connect-test: ALL OK" /tmp/o.log || { echo "  --- the diagnostics of connect-test:"; grep -E "wall:|dns:|echo-|refused|close-|sleep:|connect-test|net:" /tmp/o.log | sed 's/^/  | /'; echo "  --- minidns:"; tail -5 /tmp/dns.log | sed 's/^/  | /'; }
kill $q $DNSP $ECHOP $DEADP $SLOWP 2>/dev/null; wait $q 2>/dev/null; ip addr del 10.0.0.9/24 dev tap0 2>/dev/null

# LIVE DEBUGGING ON SMP. The debug registers are each core's own, and a breakpoint was
# armed only where the stub stood: a thread on an AP went straight through it while the
# other cores ran on as gdb stepped over the breakpoint — and gdb crashed itself
# (`finish_step_over`). Now the registers are the machine's state and the other cores park
# (all-stop). The check: a stop before the first instruction (`talaria.gdb=break`), an
# hbreak on the spinning thread, continue -> a stop on an AP.
if [ -f /img/disk_gdbsmp.img ] && [ -s /img/spinner.addr ]; then
  cp /img/disk_gdbsmp.img /tmp/gdbsmp.img
  rm -f /tmp/o.log
  qemu-system-x86_64 -M pc -enable-kvm -cpu "$CPU" -smp 4 -m 512 \
    -kernel "/w/$K" -append "console=ttyS0 talaria.mode=cell talaria.pci=1 talaria.smp=4 talaria.gdb=break" -display none \
    -serial file:/tmp/o.log -serial tcp:127.0.0.1:1235,server,nowait -no-reboot \
    -drive id=d0,file=/tmp/gdbsmp.img,format=raw,if=none -device nvme,drive=d0,serial=tal0 >/tmp/q.log 2>&1 &
  QG=$!; sleep 6
  GS=$(python3 - "$(cat /img/spinner.addr)" "$(cat /img/ktext.addr 2>/dev/null)" <<'PYRSP' 2>&1
import socket, sys, re
spin = int(sys.argv[1], 16)
ktext = int(sys.argv[2], 16) if len(sys.argv) > 2 and sys.argv[2] else 0
s = socket.create_connection(("127.0.0.1", 1235), timeout=15)
buf = b""
def pkt():
    global buf
    while True:
        m = re.search(rb"\$([^#]*)#..", buf)
        if m:
            buf = buf[m.end():]; s.sendall(b"+"); return m.group(1).decode()
        d = s.recv(4096)
        if not d: raise SystemExit("eof")
        buf += d
def cmd(c):
    s.sendall(("$%s#%02x" % (c, sum(c.encode()) % 256)).encode()); return pkt()
s.sendall(b"+")      # as gdb does: acknowledge what the stub could already have sent,
first = cmd("?")     # and ask for the reason of the stop anew
sw = ""
if ktext:   # a software breakpoint in the kernel's read-only code: written, seen, removed
    orig = cmd("m%x,1" % ktext); z0 = cmd("Z0,%x,1" % ktext); on = cmd("m%x,1" % ktext)
    zz = cmd("z0,%x,1" % ktext); off = cmd("m%x,1" % ktext)
    sw = " Z0=%s m=%s z0=%s back=%s" % (z0, on, zz, "yes" if off == orig else "NO(%s/%s)" % (off, orig))
ok1 = cmd("Z1,%x,1" % spin)
stop = cmd("c")
ok2 = cmd("z1,%x,1" % spin)
det = cmd("D")
print("first=%s Z1=%s stop=%s z1=%s D=%s%s" % (first, ok1, stop, ok2, det, sw))
PYRSP
)
  echo "  | $GS"
  case "$GS" in *"first=T05thread:1;core:0;"*"Z1=OK stop=T05thread:"*"core:"[123]";"*"z1=OK D=OK"*)
    echo "  PASS  gdb-smp: a stop before the first instruction, an hbreak on the spinning thread catches it on an AP, a detach"; PASS=$((PASS+1));;
    *) echo "  FAIL  gdb-smp: $GS"; FAIL=$((FAIL+1));; esac
  case "$GS" in *"Z0=OK m=cc z0=OK back=yes"*)
    echo "  PASS  gdb-smp: a software breakpoint in the kernel's read-only code (W^X) — 0xCC written through the direct map, read back, removed, the byte restored"; PASS=$((PASS+1));;
    *) echo "  FAIL  gdb-smp: the software breakpoint in the kernel's code: $GS"; FAIL=$((FAIL+1));; esac
  sleep 1
  assert "crashme: a live instance" "gdb-smp: after the detach the workload goes on working (the cores have been released from the parking)"
  kill $QG 2>/dev/null; wait $QG 2>/dev/null
else echo "  SKIP  gdb-smp: there is no /img/disk_gdbsmp.img or no address of spinner"; fi

echo "== the gdb stub (the GDB Remote Serial Protocol over COM2, -M pc) =="
qemu-system-x86_64 -M pc -enable-kvm -cpu "$CPU" -smp 1 -m 512 \
  -kernel "/w/$K" -append "console=ttyS0 talaria.mode=gdb" -display none \
  -serial file:/tmp/con.log -serial tcp:127.0.0.1:1234,server,nowait -no-reboot >/tmp/q.log 2>&1 &
QG=$!; sleep 3
exec 3<>/dev/tcp/127.0.0.1/1234
( timeout 4 cat <&3 > /tmp/rsp.out ) &
sleep 0.5; printf '+' >&3           # ack S05
printf '$g#67' >&3                  # a request for the registers
sleep 0.6; printf '+' >&3           # an ack of the g answer
printf '$qfThreadInfo#bb' >&3       # the list of the threads
sleep 0.6; printf '+' >&3
# hbreak (Z1: a hardware breakpoint on execution — the workload's code is R-X, an int3
# cannot be written there) and detach (D: without an answer gdb kept the machine stopped).
printf '$Z1,200000,1#36' >&3
sleep 0.6; printf '+' >&3
printf '$z1,200000,1#56' >&3
sleep 0.6; printf '+' >&3
printf '$D#44' >&3
sleep 0.8; exec 3<&-; kill $QG 2>/dev/null; wait $QG 2>/dev/null
HEXN=$(tr -cd '0-9a-f' </tmp/rsp.out | wc -c)
if grep -q 'm1' /tmp/rsp.out && [ "$HEXN" -ge 256 ]; then
  echo "  PASS  gdb-stub: g (the registers, ${HEXN} hex) + qfThreadInfo (m1) over the RSP"; PASS=$((PASS+1))
else echo "  FAIL  gdb-stub: RSP (hex=$HEXN, m1?)"; FAIL=$((FAIL+1)); fi
NOK=$(grep -o '\$OK#9a' /tmp/rsp.out | wc -l)
if [ "$NOK" -ge 3 ]; then
  echo "  PASS  gdb-stub: hbreak (Z1/z1) and detach (D) answer OK — the live debugging of a workload is available"; PASS=$((PASS+1))
else echo "  FAIL  gdb-stub: Z1/z1/D answered OK $NOK times out of 3"; FAIL=$((FAIL+1)); fi

summary
INNER
# ── THE HOST CHECKS ──────────────────────────────────────────────────────────
# They go AFTER the container and therefore do not get into its TOTAL — they have their own
# count and their own summary. Earlier their result was not counted anywhere at all: a
# failure of a host check was printed as the word FAIL and affected neither the summary nor the exit code.
HP=0; HF=0
hpass() { echo "  PASS  $1"; HP=$((HP+1)); }
hfail() { echo "  FAIL  $1"; HF=$((HF+1)); }

# the host: the ext2 image after all the kernel's writes must be consistent (e2fsprogs)
if command -v e2fsck >/dev/null 2>&1; then
  if e2fsck -fn "$IMG/disk_ext2.img" >/dev/null 2>&1; then hpass "host: e2fsck -fn disk_ext2.img is clean after the ext2/sigtest/proc tests"; else hfail "host: e2fsck -fn disk_ext2.img found errors"; fi
  # §8.4: the journal must be clear to a FOREIGN instrument — e2fsck must recognise the FS
  # as clean after Talaria has replayed its journal (the phase 2 above).
  if e2fsck -fn "$IMG/disk_jrn.img" >/dev/null 2>&1; then hpass "host: e2fsck -fn disk_jrn.img is clean after the replay of Talaria's journal"; else hfail "host: e2fsck -fn disk_jrn.img found errors after the journal"; fi
  # §3.1: the main external check of the htree — e2fsck knows the index's format and
  # complains about a wrong order of the hashes in the leaves, broken nodes and lost records.
  if [ -f "$IMG/disk_dx.img" ]; then
    if e2fsck -fn "$IMG/disk_dx.img" >/dev/null 2>&1; then hpass "host: e2fsck -fn disk_dx.img is clean — the htree after the kernel's splits of the leaves is correct"; else hfail "host: e2fsck -fn disk_dx.img found errors in the directory's index"; fi
    if [ "$(debugfs -R 'stat /big' "$IMG/disk_dx.img" 2>/dev/null | sed -n 's/.*Flags: 0x\([0-9a-f]*\).*/\1/p')" = 1000 ]; then
      hpass "host: the directory /big stayed INDEXED after 3000 insertions by the kernel (the EXT2_INDEX_FL is intact)"
    else hfail "host: /big lost the EXT2_INDEX_FL (the kernel de-indexed the directory)"; fi
  fi
  # A5: an index BUILT BY THE KERNEL from scratch (/fresh) must be clear to e2fsck — it
  # checks the order of the hashes, the nodes' limit/count and the leaves matching the tree.
  if [ -f "$IMG/disk_dx.img" ]; then
    if [ "$(debugfs -R 'stat /fresh' "$IMG/disk_dx.img" 2>/dev/null | sed -n 's/.*Flags: 0x\([0-9a-f]*\).*/\1/p')" = 1000 ]; then
      hpass "host: the directory /fresh has been indexed BY THE KERNEL ITSELF (the EXT2_INDEX_FL is set, e2fsck above is clean)"
    else hfail "host: /fresh did not get the EXT2_INDEX_FL — the index from scratch has not been built"; fi
  fi
  # A5: a tree that has GROWN in depth — the same external judge: e2fsck parses the nodes
  # of the second level, and the index must stay in place (unlike in dx3 below).
  if [ -f "$IMG/disk_dx2.img" ]; then
    if e2fsck -fn "$IMG/disk_dx2.img" >/dev/null 2>&1; then hpass "host: e2fsck -fn disk_dx2.img is clean — the TWO-level tree grown by the kernel is correct"; else hfail "host: e2fsck -fn disk_dx2.img found errors in the grown tree"; fi
    if [ "$(debugfs -R 'stat /big' "$IMG/disk_dx2.img" 2>/dev/null | sed -n 's/.*Flags: 0x\([0-9a-f]*\).*/\1/p')" = 1000 ]; then
      hpass "host: /big kept its index having grown a level (earlier it de-indexed itself at this point)"
    else hfail "host: /big lost the EXT2_INDEX_FL instead of growing the tree"; fi
  fi
  # A de-indexed directory must stay a CORRECT ext2: e2fsck is clean and the EXT2_INDEX_FL
  # is cleared (otherwise e2fsck would complain about an inconsistent index).
  if [ -f "$IMG/disk_dx3.img" ]; then
    if e2fsck -fn "$IMG/disk_dx3.img" >/dev/null 2>&1; then hpass "host: e2fsck -fn disk_dx3.img is clean after the kernel's DE-INDEXING of the directory"; else hfail "host: e2fsck -fn disk_dx3.img found errors after the de-indexing"; fi
    if [ "$(debugfs -R 'stat /big' "$IMG/disk_dx3.img" 2>/dev/null | sed -n 's/.*Flags: 0x\([0-9a-f]*\).*/\1/p')" = 0 ]; then
      hpass "host: the de-indexed /big has its EXT2_INDEX_FL cleared (a lawful linear state)"
    else hfail "host: /big kept the EXT2_INDEX_FL although the index is no longer maintained"; fi
  fi
fi

# ── talaria-prof: parsing the profile ON THE HOST ────────────────────────────
# The tool lives outside the kernel, and it breaks outside the kernel too — at
# the moment the operator has collected a profile and wants to read it. Run it
# on fixtures: the sample format is fixed, so it is testable without any VM.
echo "== talaria-prof (profile parsing on the host) =="
PROF=$REPO/kernel/tools/talaria-prof
FIX=$REPO/kernel/tools/testdata/prof-sample.txt
if [ -x "$PROF" ] && [ -f "$FIX" ]; then
  # folded: ROOT first (the kernel gives the leaf first — the tool must reverse)
  if "$PROF" folded "$FIX" 2>/dev/null | grep -q '^workload;worker(2);0x401400;0x401300;0x401200 2$'; then
    hpass "talaria-prof: folded reverses the stack root-first and merges identical ones"
  else hfail "talaria-prof: folded gives the wrong frame order or does not count repeats"; fi
  # raw: the whole stack, leaf first, sorted by frequency
  if "$PROF" raw "$FIX" 2>/dev/null | grep -q '0x401200 <- 0x401300 <- 0x401400'; then
    hpass "talaria-prof: raw prints the STACK, not a single address"
  else hfail "talaria-prof: raw lost the stack"; fi
  # report: the most-frequent-stacks section and the average depth
  if "$PROF" report "$FIX" 2>/dev/null | grep -q 'average depth'; then
    hpass "talaria-prof: report computes the average stack depth"
  else hfail "talaria-prof: report does not show the depth"; fi
  # a wrapped ring must be named: otherwise the profile is read as complete
  if "$PROF" raw "$FIX" 2>&1 >/dev/null | grep -q 'the ring wrapped'; then
    hpass "talaria-prof: lost samples are named (the ring wrapped)"
  else hfail "talaria-prof: silent about lost samples"; fi
  # the old stackless format is still readable (logs of past runs are still around)
  if "$PROF" folded "$REPO/kernel/tools/testdata/prof-sample-v1.txt" 2>/dev/null | grep -q '^workload;worker(2);0x401200 1$'; then
    hpass "talaria-prof: format v1 (no stacks) is still readable"
  else hfail "talaria-prof: compatibility with the old format is broken"; fi
  # argument order is free
  if "$PROF" report --kernel /nonexistent "$FIX" >/dev/null 2>&1; then
    hpass "talaria-prof: a file after an option is parsed (argument order is free)"
  else hfail "talaria-prof: does not accept a file after --kernel"; fi
  # garbage input — a clear refusal, not a Python traceback
  if ! echo garbage | "$PROF" report >/dev/null 2>&1; then
    hpass "talaria-prof: input without samples — refused with a non-zero code, no panic"
  else hfail "talaria-prof: returned success on input without samples"; fi
else
  echo "  SKIP  talaria-prof: tool or fixtures missing"
fi

# ── the core dumps: they are read by a REAL gdb ──────────────────────────────
# That is exactly the point of the gate. We may believe as much as we like that we have
# written an ET_CORE, but only the one who is going to open it checks that. The very first
# attempt showed why: the `prpsinfo` note had lost four bytes of alignment, readelf
# complained about the NEXT note, and our own checks would never have noticed that.
# In the CI subset the workload that CRASHES (and so writes the dump) lies past the
# boundary, so there is nothing on the volume to read: the check is skipped rather
# than failed on an input that was never produced.
if [ "${TAL_CI:-0}" = 1 ]; then
  echo "== the core dumps: SKIPPED (TAL_CI=1 — the crashing workload is past the boundary) =="
else
echo "== the core dumps (read by a real gdb) =="
CORE=$(mktemp -d)
if command -v debugfs >/dev/null 2>&1 && [ -f "$IMG/disk_core.img" ]; then
  debugfs -R "dump /core.elf $CORE/core.elf" "$IMG/disk_core.img" >/dev/null 2>&1
  if [ -s "$CORE/core.elf" ]; then
    hpass "core: the file /core.elf is on the volume after the crash"
    if readelf -h "$CORE/core.elf" 2>/dev/null | grep -qE 'CORE|Core'; then
      hpass "core: it is an ELF of the type CORE (readelf)"
    else hfail "core: readelf did not recognise the file as a core dump"; fi
    if readelf -n "$CORE/core.elf" 2>/dev/null | grep -q NT_PRSTATUS; then
      hpass "core: the note NT_PRSTATUS is in place (gdb takes the registers from it)"
    else hfail "core: there is no NT_PRSTATUS — gdb will not see the registers"; fi
    if ! LC_ALL=C readelf -n "$CORE/core.elf" 2>&1 | grep -qi 'corrupt'; then
      hpass "core: the notes are not damaged (the descriptors' sizes agree)"
    else hfail "core: readelf reports a damaged note — the sizes have diverged"; fi
    if command -v gdb >/dev/null 2>&1; then
      # The last one run was the workload's gate, which means ITS dump lies on the volume:
      # gdb must show the frame of main with the file's name and the line.
      if gdb -batch -nx -ex bt "$REPO/kernel/user/crashme" "$CORE/core.elf" 2>/dev/null | grep -qE 'main \(.*\) at crashme.c'; then
        hpass "core: gdb opens the WORKLOAD's dump and prints a bt with the names (main () at crashme.c)"
      else hfail "core: gdb could not show the workload's stack by the dump"; fi
      if gdb -batch -nx -ex 'info registers rip' "$REPO/kernel/user/crashme" "$CORE/core.elf" 2>/dev/null | grep -q '^rip '; then
        hpass "core: gdb reads the registers from the dump"
      else hfail "core: gdb does not read the registers"; fi
    else echo "  SKIP  core: there is no gdb on the host"; fi
  else hfail "core: /core.elf was not found on the volume (the dump has not been written)"; fi
else
  echo "  SKIP  core: there is no debugfs or no disk_core.img image"
fi
rm -rf "$CORE"
fi   # TAL_CI

echo ""
echo "TOTAL (host): PASS=$HP FAIL=$HF"
[ "$HF" = 0 ] || { echo "THERE ARE FAILURES ON THE HOST"; exit 1; }
