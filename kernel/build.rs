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
    for f in ["vdso.S", "vdso.lds", "vdso-aarch64.S", "vdso-aarch64.lds", "layout.lds"] {
        println!("cargo:rerun-if-changed={}", dir.join(f).display());
    }
    println!("cargo:rerun-if-env-changed=HOST_CC");
    println!("cargo:rerun-if-env-changed=VDSO_CC_AARCH64");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    // The arm64 vDSO is assembled by a cross compiler (an aarch64 gcc or clang with a
    // target). Without one the kernel still builds: the image is left empty and vdso.rs
    // says so at boot ("the vDSO is switched off") — that is a degraded kernel, not a
    // broken build, and the message names what is missing.
    let (src, lds, cc) = if arch == "aarch64" {
        let cc = env::var("VDSO_CC_AARCH64").unwrap_or_else(|_| "aarch64-linux-gnu-gcc".into());
        ("vdso-aarch64.S", "vdso-aarch64.lds", cc)
    } else {
        ("vdso.S", "vdso.lds", env::var("HOST_CC").unwrap_or_else(|_| "cc".into()))
    };
    let status = Command::new(&cc)
        .args(["-shared", "-fPIC", "-nostdlib", "-Wl,-soname=linux-vdso.so.1",
               "-Wl,--hash-style=both", "-Wl,-Bsymbolic"])
        .arg(format!("-Wl,--version-script={}", dir.join(lds).display()))
        .arg(format!("-Wl,-T,{}", dir.join("layout.lds").display()))
        .arg("-o").arg(&out)
        .arg(dir.join(src))
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => panic!("vDSO build failed ({cc}): {s}"),
        Err(e) if arch == "aarch64" => {
            println!("cargo:warning=no aarch64 cross compiler `{cc}` (set VDSO_CC_AARCH64=): the arm64 vDSO image is EMPTY, the kernel will run without it ({e})");
            std::fs::write(&out, []).expect("write the empty vdso.so");
        }
        Err(e) => panic!("cannot run host C compiler `{cc}` (set HOST_CC): {e}"),
    }
}
