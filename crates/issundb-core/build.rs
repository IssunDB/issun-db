use std::env;
use std::path::PathBuf;

fn main() {
    // Re-run the build script only if the submodule source code changes
    println!("cargo:rerun-if-changed=../../external/GraphBLAS");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let graphblas_dir = manifest_dir.join("../../external/GraphBLAS");

    // Configure and build GraphBLAS using the cmake crate
    let mut config = cmake::Config::new(&graphblas_dir);
    
    config
        .define("BUILD_STATIC_LIBS", "ON")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("GBCOMPACT", "ON") // Keep build times and binary size down
        .define("CMAKE_BUILD_TYPE", "Release");

    let dst = config.build();

    // Detect if the library is built in lib or lib64
    let lib_dir = dst.join("lib");
    let lib64_dir = dst.join("lib64");
    
    let active_lib_dir = if lib64_dir.exists() {
        lib64_dir
    } else {
        lib_dir
    };

    // Inform Cargo of the static library search path and linkage
    println!("cargo:rustc-link-search=native={}", active_lib_dir.display());
    println!("cargo:rustc-link-lib=static=graphblas");
}
