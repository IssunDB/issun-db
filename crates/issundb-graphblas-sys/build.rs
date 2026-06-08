// A build script panics on a misconfigured environment (missing submodule,
// failed cmake, unwritable OUT_DIR); `unwrap`/`expect` are the idiomatic way to
// surface those as build failures, so the workspace bans on them do not apply.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};

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

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    // Build GraphBLAS as a position-independent static library. PIC lets the
    // archive (including GB_Context.c's thread-local global) link into the
    // binding cdylibs; COMPACT skips the FactoryKernels for a faster build; JIT
    // is disabled so the runtime never dlopen()s a compiler. OpenMP stays a
    // dynamic dependency (resolved below), not a bundled static archive.
    let mut cfg = cmake::Config::new(&graphblas_src);
    cfg.define("CMAKE_POSITION_INDEPENDENT_CODE", "ON")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("BUILD_STATIC_LIBS", "ON")
        .define("GRAPHBLAS_COMPACT", "ON")
        .define("GRAPHBLAS_USE_JIT", "OFF")
        .define("CMAKE_BUILD_TYPE", "Release");

    // Apple Clang does not ship an OpenMP runtime, so `find_package(OpenMP)`
    // fails unless pointed at the Homebrew (or MacPorts) `libomp`. Without these
    // hints GraphBLAS silently builds single-threaded and the `-lomp` link line
    // below has no library to resolve. Locate the prefix once and reuse it for
    // both the cmake hints and the link search path.
    let macos_libomp = (target_os == "macos").then(find_libomp_prefix).flatten();
    if let Some(prefix) = &macos_libomp {
        cfg.define(
            "OpenMP_C_FLAGS",
            format!("-Xclang -fopenmp -I{prefix}/include"),
        )
        .define("OpenMP_C_LIB_NAMES", "omp")
        .define("OpenMP_omp_LIBRARY", format!("{prefix}/lib/libomp.dylib"));
    }

    let dst = cfg.build();

    // The install tree puts the static library under `lib` (Debian/Ubuntu) or
    // `lib64` (Fedora-like); search both.
    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    println!("cargo:rustc-link-search=native={}/lib64", dst.display());
    println!("cargo:rustc-link-lib=static=graphblas");

    // GraphBLAS links its OpenMP runtime dynamically rather than bundling it in
    // the static archive, so the consuming binary must pull it in. The runtime
    // name tracks the OpenMP implementation cmake's `find_package(OpenMP)`
    // selected, which follows the C compiler for the target: GCC links libgomp,
    // LLVM and Apple Clang link libomp, and MSVC links vcomp.
    match target_os.as_str() {
        "macos" => {
            if let Some(prefix) = &macos_libomp {
                println!("cargo:rustc-link-search=native={prefix}/lib");
            }
            println!("cargo:rustc-link-lib=dylib=omp");
        }
        "windows" if target_env == "msvc" => {
            // MSVC compiles the `/openmp` objects with an embedded autolink
            // directive for vcomp; name it explicitly so the link does not
            // depend on that directive surviving in the static archive.
            println!("cargo:rustc-link-lib=dylib=vcomp");
        }
        // Linux, the GNU/MinGW Windows toolchain, and the BSDs build GraphBLAS
        // with GCC, whose OpenMP runtime is libgomp.
        _ => println!("cargo:rustc-link-lib=dylib=gomp"),
    }

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

/// Locate the macOS `libomp` install prefix so cmake can resolve OpenMP.
///
/// Honors an explicit `LIBOMP_PREFIX` override first, then asks Homebrew, then
/// falls back to the default Homebrew prefixes for Apple Silicon and Intel and
/// the MacPorts prefix. Returns the prefix only when `lib/libomp.dylib` is
/// present under it, so a stale path never reaches the link line.
fn find_libomp_prefix() -> Option<String> {
    let has_libomp = |prefix: &str| Path::new(prefix).join("lib/libomp.dylib").exists();

    if let Ok(prefix) = std::env::var("LIBOMP_PREFIX") {
        if has_libomp(&prefix) {
            return Some(prefix);
        }
    }

    if let Ok(out) = std::process::Command::new("brew")
        .args(["--prefix", "libomp"])
        .output()
    {
        if out.status.success() {
            let prefix = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !prefix.is_empty() && has_libomp(&prefix) {
                return Some(prefix);
            }
        }
    }

    [
        "/opt/homebrew/opt/libomp",
        "/usr/local/opt/libomp",
        "/opt/local",
    ]
    .into_iter()
    .find(|prefix| has_libomp(prefix))
    .map(str::to_string)
}
