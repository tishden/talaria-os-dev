# kernel-unit — host unit tests for the kernel

```sh
cd kernel-unit && cargo test
```

The kernel (`kernel/`) is a `no_std` binary for a custom target; its
integration tests boot it in QEMU (`kernel/test.sh`). This crate complements
them with fast tests of the kernel's pure logic on the host: each module listed
in `src/lib.rs` is compiled from `kernel/src` as is, and its
`#[cfg(test)] mod tests` runs here. See `src/lib.rs` for the rules.
