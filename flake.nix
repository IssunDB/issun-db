{
  description = "IssunDB: a fast embedded analytical graph database in Rust";

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
        "x86_64-darwin"
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
          # Retrieve the exact rust toolchain specified in rust-toolchain.toml
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

          # Runtime libraries required by eframe/egui GUI on Linux
          guiLibs = with pkgs; lib.optionals stdenv.isLinux [
            libxkbcommon
            libGL
            vulkan-loader
            xorg.libX11
            xorg.libXcursor
            xorg.libXi
            xorg.libXrandr
            wayland
          ];

          # Darwin-specific frameworks for macOS development
          darwinDeps = with pkgs; lib.optionals stdenv.isDarwin [
            darwin.apple_sdk.frameworks.AppKit
            darwin.apple_sdk.frameworks.CoreGraphics
            darwin.apple_sdk.frameworks.CoreVideo
            darwin.apple_sdk.frameworks.Foundation
            darwin.apple_sdk.frameworks.Metal
            darwin.apple_sdk.frameworks.Security
            darwin.apple_sdk.frameworks.Cocoa
          ];
        in
        {
          default = pkgs.mkShell {
            name = "issundb-dev";

            packages = [
              rustToolchain
              pkgs.pkg-config
              pkgs.cmake
              pkgs.gnumake
              pkgs.graphviz
              pkgs.python3
              pkgs.nodejs
              pkgs.openssl
              pkgs.llvmPackages.libclang
              pkgs.clang
              pkgs.pre-commit
              pkgs.zig
              pkgs.cargo-zigbuild

              # Dev tools used in Makefile
              pkgs.cargo-tarpaulin
              pkgs.cargo-audit
              pkgs.cargo-careful
              pkgs.cargo-nextest
            ] ++ guiLibs ++ darwinDeps;

            # We need to set the LD_LIBRARY_PATH so that the compiled GUI binary can find dynamic libraries on NixOS/Linux.
            # We also set LIBCLANG_PATH for rust-bindgen to work properly.
            shellHook = ''
              export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
              ${pkgs.lib.optionalString pkgs.stdenv.isLinux ''
                export LD_LIBRARY_PATH="$LD_LIBRARY_PATH:${pkgs.lib.makeLibraryPath guiLibs}"
              ''}
              echo "=========================================================="
              echo "  Welcome to the IssunDB development environment!        "
              echo "  Rust version:  $(rustc --version)                       "
              echo "  Python:        $(python3 --version 2>/dev/null || echo 'not found')"
              echo "  Node.js:       $(node --version 2>/dev/null || echo 'not found')"
              echo "=========================================================="
            '';
          };
        });

      formatter = forAllSystems (pkgs: pkgs.nixpkgs-fmt);
    };
}
