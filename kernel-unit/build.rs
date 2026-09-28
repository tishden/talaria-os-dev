//! Marks the kernel sources compiled here as a host build (see src/lib.rs).
fn main() {
    println!("cargo::rustc-check-cfg=cfg(talaria_host)");
    println!("cargo::rustc-cfg=talaria_host");
}
