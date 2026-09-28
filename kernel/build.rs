//! Build script: assembles the vDSO page that the kernel maps into every
//! process (`src/vdso.rs`). It is a plain `-nostdlib` shared object, so the
//! host C compiler is enough — no musl toolchain or docker image is needed to
//! build the kernel itself.
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("user/vdso");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("vdso.so");
    for f in ["vdso.S", "vdso.lds", "layout.lds"] {
        println!("cargo:rerun-if-changed={}", dir.join(f).display());
    }
    println!("cargo:rerun-if-env-changed=HOST_CC");
    let cc = env::var("HOST_CC").unwrap_or_else(|_| "cc".into());
    let status = Command::new(&cc)
        .args(["-shared", "-fPIC", "-nostdlib", "-Wl,-soname=linux-vdso.so.1",
               "-Wl,--hash-style=both", "-Wl,-Bsymbolic"])
        .arg(format!("-Wl,--version-script={}", dir.join("vdso.lds").display()))
        .arg(format!("-Wl,-T,{}", dir.join("layout.lds").display()))
        .arg("-o").arg(&out)
        .arg(dir.join("vdso.S"))
        .status()
        .unwrap_or_else(|e| panic!("cannot run host C compiler `{cc}` (set HOST_CC): {e}"));
    assert!(status.success(), "vDSO build failed ({cc})");
}
