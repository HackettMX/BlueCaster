//! Builds the vendored L2HC encoder (L2HC-native-linux, MIT) as one static
//! library. Unlike the other codecs the sources live in this crate's csrc/,
//! so no git submodule is needed.
//!
//! MSVC needs C11 for the designated initializers, /fp:fast for the DSP's
//! floating point assumptions and /utf-8 for the sources; GCC/Clang get
//! -ffast-math as the reference Makefile does.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let dir = manifest.join("csrc");
    assert!(dir.join("l2hc_native.c").exists(), "L2HC encoder sources missing at {}", dir.display());

    let mut build = cc::Build::new();
    build
        .file(dir.join("l2hc_native.c"))
        .include(&dir)
        .std("c11")
        // Third-party code: keep its warnings out of the build log
        .warnings(false)
        .cargo_warnings(false);
    if build.get_compiler().is_like_msvc() {
        build.flag("/W1").flag("/utf-8").flag("/fp:fast");
    } else {
        build.flag("-w").flag("-ffast-math");
    }
    build.compile("l2hc_native");

    println!("cargo:rerun-if-changed={}", dir.display());
    println!("cargo:include={}", dir.display());
}
