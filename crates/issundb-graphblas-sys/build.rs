// A build script panics on a misconfigured environment (missing submodule,
// failed cmake, unwritable OUT_DIR); `unwrap`/`expect` are the idiomatic way to
// surface those as build failures, so the workspace bans on them do not apply.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::path::PathBuf;

/// These are macros that their definitions confuse bindgen (they expand to floating-point
/// classification constants); ignore them so binding generation succeeds.
#[derive(Debug)]
struct IgnoreMacros(HashSet<String>);

impl bindgen::callbacks::ParseCallbacks for IgnoreMacros {
    fn will_parse_macro(&self, name: &str) -> bindgen::callbacks::MacroParsingBehavior {
        if self.0.contains(name) {
            bindgen::callbacks::MacroParsingBehavior::Ignore
        } else {
            bindgen::callbacks::MacroParsingBehavior::Default
        }
    }
}

fn main() {
    // The GraphBLAS C source is the `external/GraphBLAS` submodule at the
    // workspace root, two levels up from this crate's manifest.
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let graphblas_src = manifest_dir
        .join("../../external/GraphBLAS")
        .canonicalize()
        .expect(
            "external/GraphBLAS submodule not found; run `git submodule update --init external/GraphBLAS`",
        );
    let header = graphblas_src.join("Include/GraphBLAS.h");
    assert!(
        header.exists(),
        "GraphBLAS.h missing at {}; the submodule is not checked out",
        header.display()
    );

    // Build GraphBLAS as a position-independent static library. PIC lets the
    // archive (including GB_Context.c's thread-local global) link into the
    // binding cdylibs; COMPACT skips the FactoryKernels for a faster build; JIT
    // is disabled so the runtime never dlopen()s a compiler. OpenMP stays a
    // dynamic dependency (resolved below), not a bundled static archive.
    let dst = cmake::Config::new(&graphblas_src)
        .define("CMAKE_POSITION_INDEPENDENT_CODE", "ON")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("BUILD_STATIC_LIBS", "ON")
        .define("GRAPHBLAS_COMPACT", "ON")
        .define("GRAPHBLAS_USE_JIT", "OFF")
        .define("CMAKE_BUILD_TYPE", "Release")
        .build();

    // The install tree puts the static library under `lib` (Debian/Ubuntu) or
    // `lib64` (Fedora-like); search both.
    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    println!("cargo:rustc-link-search=native={}/lib64", dst.display());
    println!("cargo:rustc-link-lib=static=graphblas");
    // GraphBLAS is compiled with GCC OpenMP; link the system shared libgomp
    // (position-independent) rather than bundling a static archive.
    println!("cargo:rustc-link-lib=dylib=gomp");

    // Regenerate bindings if the pinned header changes.
    println!("cargo:rerun-if-changed={}", header.display());
    println!("cargo:rerun-if-changed=build.rs");

    let ignored = IgnoreMacros(
        [
            "FP_NAN",
            "FP_INFINITE",
            "FP_ZERO",
            "FP_SUBNORMAL",
            "FP_NORMAL",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
    );

    let bindings = bindgen::Builder::default()
        .header(header.to_str().unwrap())
        .parse_callbacks(Box::new(ignored))
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("failed to generate GraphBLAS bindings");

    let out_path = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("bindings.rs");
    bindings
        .write_to_file(&out_path)
        .expect("failed to write GraphBLAS bindings");
}
