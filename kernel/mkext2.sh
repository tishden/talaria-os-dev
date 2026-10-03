#!/bin/sh
# Build an ext2 image for Talaria from a directory: mkext2.sh <dir> <img> <size, e.g. 64M>
# (no dir_index/journal — the kernel writes linear directories; check: e2fsck -fn <img>)
set -eu
DIR=$1; IMG=$2; SZ=${3:-64M}
rm -f "$IMG"
# §8.4: the JBD2 journal is on (has_journal) — the Talaria kernel runs metadata
# transactions and replays the journal at mount. Features that change the
# format of journal tags/directories are off: dir_index (htree), metadata_csum, 64bit.
# §3.1: DIRIDX=1 — the opposite, TURN ON dir_index and build an htree (mke2fs with
# -d lays directories out linearly, e2fsck -D builds the index). Such an image is
# needed by talaria.test=htree: the kernel must both read and extend a foreign index.
# HASHU=1 — the volume's directory hash is UNSIGNED (s_flags bit 1), as mkfs leaves it on a
# machine where `char` is unsigned (every arm64 host): the kernel's hashing then adds 3 to
# the version, and its own index must still carry the superblock's raw one (the first arm64
# stand found a 4 in the kernel's dx_root). The flag goes in before e2fsck -D builds the tree.
DX=^dir_index
[ "${DIRIDX:-0}" = 1 ] && DX=dir_index
mkfs.ext2 -q -F -b 4096 -O "$DX",^resize_inode,^ext_attr,^metadata_csum,^64bit,has_journal -d "$DIR" "$IMG" "$SZ"
[ "${HASHU:-0}" = 1 ] && debugfs -w -R "set_super_value flags 2" "$IMG" >/dev/null 2>&1
[ "${DIRIDX:-0}" = 1 ] && e2fsck -fyD "$IMG" >/dev/null 2>&1
# THE IMAGE GOES TO DISK, NOT INTO THE HOST CACHE. A fresh image (512-1500 MB)
# stays dirty in the page cache, and the GUEST pays for it: its very first
# barrier is QEMU's `fdatasync` of the image, i.e. waiting until everything
# written lands on the platters. On the runner's spinning disk that is 5-10 s,
# and the kernel's command limit is 10 s: the command is declared lost, the FS
# goes read-only, and WiredTiger fails with WT_PANIC. That is how
# `kernel/validate.sh` failed THREE gates in a row (mongodb, jvm, clickhouse) —
# the very ones that are green alone: alone the image was already on disk from
# last time, while in a back-to-back run each gate rebuilds its image right
# before boot. The one who wrote the image pays the seconds.
sync "$IMG" 2>/dev/null || sync
exit 0
