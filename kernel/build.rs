//! Build script: assembles the vDSO page that the kernel maps into every
//! process (`src/vdso.rs`). It is a plain `-nostdlib` shared object, so the
//! host C compiler is enough — no musl toolchain or docker image is needed to
//! build the kernel itself.
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // The linker scripts are inputs of the kernel, not of this script, but cargo does not know
    // that (they come in through `-T` in .cargo/config.toml): an edit to one relinked NOTHING —
    // stage 7 moved the ELF entry in linker.ld and the build "finished in 0.44 s" with the old
    // one. A changed script reruns this script, and that rebuilds the crate.
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    for f in ["linker.ld", "linker-aarch64.ld"] {
        println!("cargo:rerun-if-changed={}", root.join(f).display());
    }
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("user/vdso");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("vdso.so");
    for f in ["vdso.S", "vdso.lds", "vdso-aarch64.S", "vdso-aarch64.lds", "layout.lds"] {
        println!("cargo:rerun-if-changed={}", dir.join(f).display());
    }
    println!("cargo:rerun-if-env-changed=HOST_CC");
    println!("cargo:rerun-if-env-changed=VDSO_CC_AARCH64");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    // The arm64 ring-3 binaries (`user/aarch64/`, built by `make -C user aarch64`): the
    // `user_elf!` macro for that target is GENERATED from the directory, one arm per file
    // present and an empty slice for the rest — so a kernel builds whatever the set is, and a
    // mode whose binary is missing says so at boot instead of failing the build.
    if arch == "aarch64" {
        let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("user/aarch64");
        println!("cargo:rerun-if-changed={}", dir.display());
        let mut arms = String::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            // The files, and one level of subdirectories as `sub/name` (`tsync/fiberbench`,
            // as the x86 tree names them).
            let mut names: Vec<String> = Vec::new();
            for e in rd.filter_map(|e| e.ok()) {
                let Ok(n) = e.file_name().into_string() else { continue };
                if e.path().is_file() { names.push(n); }
                else if e.path().is_dir() {
                    println!("cargo:rerun-if-changed={}", e.path().display());
                    if let Ok(sub) = std::fs::read_dir(e.path()) {
                        for f in sub.filter_map(|f| f.ok()).filter(|f| f.path().is_file()) {
                            if let Ok(m) = f.file_name().into_string() { names.push(format!("{n}/{m}")); }
                        }
                    }
                }
            }
            names.sort();
            for n in names {
                println!("cargo:rerun-if-changed={}", dir.join(&n).display());
                arms.push_str(&format!("    ({n:?}) => {{ include_bytes!({:?}) }};\n", dir.join(&n).display().to_string()));
            }
        }
        let src = format!("macro_rules! user_elf {{\n{arms}    ($p:literal) => {{ &[] }};\n}}\n");
        std::fs::write(PathBuf::from(env::var("OUT_DIR").unwrap()).join("user_aarch64.rs"), src).expect("write user_aarch64.rs");
    }
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
