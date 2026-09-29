#!/bin/sh
# Build the arm64 kernel (docs/ARM64-PLAN.md): the ELF for gdb and the raw Linux-format
# `Image` that `qemu-system-aarch64 -kernel` loads at RAM base + text_offset.
#   kernel/build-aarch64.sh            -> target/aarch64-talaria/release/talaria{,.img}
# The raw image needs an objcopy that understands AArch64: `aarch64-linux-gnu-objcopy`
# (binutils), `llvm-objcopy` or `rust-objcopy` (the llvm-tools component) — the first
# one found is used. AARCH64_OBJCOPY= overrides.
set -eu
cd "$(dirname "$0")"
cargo -Z json-target-spec build --release --target aarch64-talaria.json
ELF=target/aarch64-talaria/release/talaria
OBJCOPY=${AARCH64_OBJCOPY:-}
for c in aarch64-linux-gnu-objcopy llvm-objcopy rust-objcopy; do
  [ -n "$OBJCOPY" ] && break
  command -v "$c" >/dev/null 2>&1 && OBJCOPY=$c
done
[ -n "$OBJCOPY" ] || { echo "build-aarch64: no objcopy for AArch64 found (set AARCH64_OBJCOPY=)"; exit 1; }
"$OBJCOPY" -O binary "$ELF" "$ELF.img"
# The header check: the loader keys on these bytes, a wrong section order breaks the
# boot silently (the image loads and nothing prints).
MAGIC=$(dd if="$ELF.img" bs=1 skip=56 count=4 2>/dev/null)
[ "$MAGIC" = "ARMd" ] || { echo "build-aarch64: the Image header is not first in the file (magic '$MAGIC')"; exit 1; }
ls -la "$ELF" "$ELF.img"
