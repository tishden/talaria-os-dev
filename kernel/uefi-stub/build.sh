#!/bin/sh
# Build the UEFI stub: a PE application with the kernel ELF inside.
# Needs the target in the pinned toolchain (once):
#   rustup target add x86_64-unknown-uefi --toolchain nightly-2026-08-20
# A rebuild is needed after every kernel rebuild (include_bytes! — cargo tracks
# freshness itself). Running it: see the uefi block in kernel/test.sh (OVMF q35).
set -eu
cd "$(dirname "$0")"
[ -f ../target/x86_64-talaria/release/talaria ] || { echo "build the kernel first (kernel/, BUILD.md)"; exit 1; }
cargo build --release --target x86_64-unknown-uefi
ls -la target/x86_64-unknown-uefi/release/talaria-uefi-stub.efi
