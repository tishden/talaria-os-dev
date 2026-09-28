# Building the kernel

The toolchain is pinned in `rust-toolchain.toml` (nightly, `rust-src`); rustup
installs it on the first cargo call. The kernel uses a custom target
(`x86_64-talaria.json`) and `build-std`, hence `-Z json-target-spec`.

```sh
# Kernel only (production modes load workloads from disk). No docker needed:
# the vDSO is assembled by build.rs with the host C compiler.
cargo -Z json-target-spec build --release --no-default-features

# Kernel with the embedded ring-3 test binaries (default `selftest` feature),
# required by the regression kernel/test.sh and the demo modes:
make -C user all            # docker images talaria-musl / talaria-alpine-build
python3 mkfs.py             # regression disk image
cargo -Z json-target-spec build --release

# Lint gate (same as CI)
cargo clippy -Zjson-target-spec --release -- -D warnings
```

Output: `target/x86_64-talaria/release/talaria` (PVH ELF, boots in QEMU with
`-kernel`). For EC2 the UEFI stub wraps it: `sh uefi-stub/build.sh` after every
kernel build.
