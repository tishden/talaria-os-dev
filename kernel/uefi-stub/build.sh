#!/bin/sh
# Build the UEFI stub: a PE application with the kernel ELF inside.
#   kernel/uefi-stub/build.sh            the x86-64 stub (target x86_64-unknown-uefi)
#   kernel/uefi-stub/build.sh aarch64    the arm64 stub (target aarch64-unknown-uefi)
# Needs the target in the pinned toolchain (once):
#   rustup target add x86_64-unknown-uefi aarch64-unknown-uefi --toolchain nightly-2026-08-20
# A rebuild is needed after every kernel rebuild (include_bytes! — cargo tracks
# freshness itself). Running it: see the uefi blocks in kernel/test.sh (OVMF q35) and
# kernel/test-aarch64.sh (edk2 on `virt`).
set -eu
cd "$(dirname "$0")"
ARCH=${1:-x86_64}
[ -f ../target/$ARCH-talaria/release/talaria ] || { echo "build the $ARCH kernel first (kernel/, BUILD.md)"; exit 1; }
cargo build --release --target $ARCH-unknown-uefi
ls -la target/$ARCH-unknown-uefi/release/talaria-uefi-stub.efi
