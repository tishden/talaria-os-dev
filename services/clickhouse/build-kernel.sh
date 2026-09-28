#!/bin/sh
# A kernel with a HIGH link base — for ClickHouse.
#
# The official ClickHouse binary is ET_EXEC (not PIE): its segments must land
# exactly at 0x200000..0x20e8b1e8 (2 MB..526 MB). That is exactly where the
# Talaria image lies by default (2 MB..~32 MB). The binary cannot be moved — an
# ET_EXEC has no relocations — so the kernel moves. The base is a link parameter
# (linker.ld, KBASE); the rest of the kernel derives the image bounds from the
# __kernel_start/__kernel_end symbols, so the build differs by ONE linker argument.
#
# The normal kernel (2 MB) is untouched: all four boot loaders and the regression use it.
# This build is for QEMU/PVH only (`-kernel`), where the loader places segments at
# p_paddr and memory is certainly larger than the base.
set -eu
cd "$(dirname "$0")/../../kernel"
KBASE=${KBASE:-0x21000000}   # 528 MB — right after the ClickHouse image
export CARGO_TARGET_DIR=target-hi
export RUSTFLAGS="-C link-arg=-Tlinker.ld -C relocation-model=static -C link-arg=--defsym=KBASE=$KBASE"
rustup run nightly cargo -Z json-target-spec build --release 2>&1 | tail -3
readelf -lW "$CARGO_TARGET_DIR/x86_64-talaria/release/talaria" | grep -a LOAD
echo "kernel with base $KBASE: $CARGO_TARGET_DIR/x86_64-talaria/release/talaria"
