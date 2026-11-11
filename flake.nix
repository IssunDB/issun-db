{
  description = "IssunDB: a fast embedded graph database in Rust";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems (system:
          let
            pkgs = import nixpkgs {
              inherit system;
              overlays = [ (import rust-overlay) ];
            };
          in
          f pkgs
        );
    in
    {
      devShells = forAllSystems (pkgs:
        let
          msrv = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [ "rustfmt" "clippy" "rust-analyzer" "llvm-tools-preview" ];
            targets = [ "wasm32-unknown-unknown" ];
          };
          benchmarkToolchain = pkgs.rust-bin.fromRustupToolchainFile
            ./benchmarks/ladybugdb-compare/rust-toolchain.toml;
          wasmBindgenSrc = pkgs.fetchCrate {
            pname = "wasm-bindgen-cli";
            version = "0.2.122";
            hash = "sha256-vO4RSxi/sMWxmsEs3GuljdMfIRSu75A+Q+c5wgYToRU=";
          };
          wasmBindgen = pkgs.buildWasmBindgenCli {
            src = wasmBindgenSrc;
            cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
              src = wasmBindgenSrc;
              inherit (wasmBindgenSrc) pname version;
              hash = "sha256-Inup6vvJSG5ghNyeDPyZbfZo4d0LsMG2OJfStoaeDBs=";
            };
          };
          mkDevShell = toolchain: pkgs.mkShell {
            name = "issundb-dev";
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.stdenv.cc.cc pkgs.openssl ];
            packages = [
              toolchain
              pkgs.pkg-config
              pkgs.gnumake
              pkgs.graphviz
              pkgs.python313
              pkgs.uv
              pkgs.maturin
              pkgs.openssl
              pkgs.pre-commit
              pkgs.zig
              pkgs.cargo-zigbuild
              pkgs.nodejs
              wasmBindgen
              pkgs.actionlint
              pkgs.cargo-llvm-cov
              pkgs.cargo-audit
              pkgs.cargo-careful
              pkgs.cargo-nextest
              pkgs.cargo-deny
            ];
          };
        in
        {
          default = mkDevShell (msrv.override {
            targets = [ "wasm32-unknown-unknown" ];
          });
          stable = mkDevShell rustToolchain;
          msrv = mkDevShell msrv;
          ladybugdb = pkgs.mkShell {
            inputsFrom = [ (mkDevShell benchmarkToolchain) ];
            packages = [ pkgs.cmake pkgs.curl ];
            # lbug's OpenSSL fallback adds host library paths that can override Nix's libc.
            OPENSSL_DIR = "${pkgs.lib.getLib pkgs.openssl}";
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.stdenv.cc.cc pkgs.openssl ];
          };
          # Release artifacts must link against the runner's libraries, not /nix/store.
          release = pkgs.mkShellNoCC {
            packages = [ rustToolchain pkgs.pkg-config pkgs.maturin ];
            CC = "/usr/bin/cc";
            CXX = "/usr/bin/c++";
            "CARGO_TARGET_${pkgs.lib.toUpper (builtins.replaceStrings [ "-" ] [ "_" ] pkgs.stdenv.hostPlatform.rust.rustcTarget)}_LINKER" = "/usr/bin/cc";
          };
        });

      formatter = forAllSystems (pkgs: pkgs.nixpkgs-fmt);
    };
}
