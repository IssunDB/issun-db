// A build script panics on a misconfigured environment (missing submodule,
// failed cmake, unwritable OUT_DIR); `unwrap`/`expect` are the idiomatic way to
// surface those as build failures, so the workspace bans on them do not apply.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned GraphBLAS release, used when the `external/GraphBLAS` submodule is not
/// present (a crate consumed from crates.io carries no submodule, because
/// `cargo package` never descends into submodules). The tarball is fetched and
/// checksum-verified at build time. The version tracks the submodule pin.
const GRAPHBLAS_VERSION: &str = "10.3.1";
const GRAPHBLAS_URL: &str =
    "https://github.com/DrTimothyAldenDavis/GraphBLAS/archive/refs/tags/v10.3.1.tar.gz";
const GRAPHBLAS_SHA256: &str = "a3c4de775f47d9b448d0f548234a6c321c45f9f6a54e32c9e3a41b28df55cd0a";

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
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // On docs.rs, skip compiling the C library and use pre-generated bindings.
    if std::env::var("DOCS_RS").is_ok() {
        let bindings_path = manifest_dir.join("bindings.rs");
        let out_path = out_dir.join("bindings.rs");
        std::fs::copy(&bindings_path, &out_path)
            .expect("failed to copy pre-generated bindings for docs.rs");
        return;
    }

    // Resolve the GraphBLAS C source: an explicit override, then the
    // `external/GraphBLAS` submodule (the in-repo path; no network), then the
    // pinned tarball downloaded into OUT_DIR (for a crate consumed from
    // crates.io, which carries no submodule).
    let graphblas_src = resolve_graphblas_src(&manifest_dir, &out_dir);
    let header = graphblas_src.join("Include/GraphBLAS.h");
    assert!(
        header.exists(),
        "GraphBLAS.h missing at {}",
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
    // GraphBLAS names the static library `graphblas` everywhere except MSVC,
    // where it appends `_static` (OUTPUT_NAME graphblas_static) so the static
    // archive does not collide with the DLL import library of the same base
    // name. We build static only, but the rename still applies, so the link
    // name must follow it on MSVC.
    if target_os == "windows" && target_env == "msvc" {
        println!("cargo:rustc-link-lib=static=graphblas_static");
    } else {
        println!("cargo:rustc-link-lib=static=graphblas");
    }

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
    println!("cargo:rerun-if-env-changed=ISSUNDB_GRAPHBLAS_SRC");

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

/// Resolve the GraphBLAS C source tree, in priority order:
///
/// 1. `ISSUNDB_GRAPHBLAS_SRC`: an explicit path to a checkout, for offline
///    builds or a custom source.
/// 2. The `external/GraphBLAS` submodule at the workspace root, two levels up
///    from this crate's manifest. This is the in-repo path (local development
///    and the wheel builds, which check out submodules), and uses no network.
/// 3. The pinned tarball, downloaded into `OUT_DIR` and checksum-verified. This
///    is the path for a crate consumed from crates.io, whose package tarball
///    carries no submodule.
fn resolve_graphblas_src(manifest_dir: &Path, out_dir: &Path) -> PathBuf {
    let has_header = |dir: &Path| dir.join("Include/GraphBLAS.h").exists();

    if let Ok(src) = std::env::var("ISSUNDB_GRAPHBLAS_SRC") {
        let src = PathBuf::from(src);
        assert!(
            has_header(&src),
            "ISSUNDB_GRAPHBLAS_SRC={} does not contain Include/GraphBLAS.h",
            src.display()
        );
        return clean_canonicalized_path(src.canonicalize().unwrap());
    }

    let submodule = manifest_dir.join("../../external/GraphBLAS");
    if has_header(&submodule) {
        return clean_canonicalized_path(submodule.canonicalize().unwrap());
    }

    download_graphblas(out_dir)
}

/// Download and extract the pinned GraphBLAS tarball into `OUT_DIR`, verifying
/// its SHA-256 before extraction. Uses the system `curl` and `tar`, which are
/// present on every platform that can already build the C library (a C
/// compiler, cmake, and clang are also required). Re-downloads are skipped when
/// the extracted tree is already present, so incremental builds pay nothing.
fn download_graphblas(out_dir: &Path) -> PathBuf {
    let extracted = out_dir.join(format!("GraphBLAS-{GRAPHBLAS_VERSION}"));
    if extracted.join("Include/GraphBLAS.h").exists() {
        return clean_canonicalized_path(extracted.canonicalize().unwrap());
    }

    let tarball = out_dir.join("graphblas.tar.gz");
    run(
        Command::new("curl")
            .args(["-sSfL", "--retry", "3", "-o"])
            .arg(&tarball)
            .arg(GRAPHBLAS_URL),
        "downloading GraphBLAS; set ISSUNDB_GRAPHBLAS_SRC to build offline",
    );

    let bytes = std::fs::read(&tarball).expect("failed to read the downloaded GraphBLAS tarball");
    let digest = sha256_hex(&bytes);
    assert_eq!(
        digest, GRAPHBLAS_SHA256,
        "GraphBLAS tarball checksum mismatch (expected {GRAPHBLAS_SHA256}, got {digest}); \
         the download is corrupt or the upstream archive changed"
    );

    run(
        Command::new("tar")
            .arg("xzf")
            .arg(&tarball)
            .arg("-C")
            .arg(out_dir),
        "extracting GraphBLAS tarball",
    );
    assert!(
        extracted.join("Include/GraphBLAS.h").exists(),
        "GraphBLAS tarball extracted to an unexpected layout at {}",
        extracted.display()
    );
    clean_canonicalized_path(extracted.canonicalize().unwrap())
}

/// Run a command, panicking with `context` if it cannot be spawned or exits
/// non-zero.
fn run(cmd: &mut Command, context: &str) {
    let status = cmd.status().unwrap_or_else(|e| {
        panic!(
            "failed to spawn `{:?}` while {context}: {e}",
            cmd.get_program()
        )
    });
    assert!(status.success(), "command failed while {context}: {status}");
}

/// Lowercase hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        write!(s, "{b:02x}").unwrap();
    }
    s
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

/// Helper function to clean UNC prefixes from canonicalized paths on Windows.
/// This prevents CMake and MSBuild build failures due to long-path syntax (`\\?\`).
fn clean_canonicalized_path<P: AsRef<Path>>(path: P) -> PathBuf {
    let path = path.as_ref();
    #[cfg(windows)]
    {
        if let Some(path_str) = path.to_str() {
            if let Some(stripped) = path_str.strip_prefix("\\\\?\\") {
                if let Some(unc_stripped) = stripped.strip_prefix("UNC\\") {
                    return PathBuf::from(format!("\\\\{}", unc_stripped));
                }
                return PathBuf::from(stripped);
            }
        }
    }
    path.to_path_buf()
}
