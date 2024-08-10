{
  description = "Rust web server with command execution";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rustVersion = pkgs.rust-bin.stable.latest.default;

        darwin = pkgs.darwin.apple_sdk.frameworks;

        commonEnv = {
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
          BINDGEN_EXTRA_CLANG_ARGS =
            pkgs.lib.optionalString pkgs.stdenv.isDarwin
            "-I${darwin.CoreServices}/include -I${darwin.CoreFoundation}/include";
        };
      in {
        devShells.default = pkgs.mkShell ({
          buildInputs = with pkgs;
            [ rustVersion pkg-config openssl zstd cmake libiconv ]
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
              darwin.Security
              darwin.SystemConfiguration
              darwin.CoreServices
              darwin.CoreFoundation
              darwin.Foundation
            ];

          RUST_SRC_PATH = "${rustVersion}/lib/rustlib/src/rust/library";
        } // commonEnv);

        packages.default = pkgs.rustPlatform.buildRustPackage ({
          pname = "rust-web-server";
          version = "0.1.0";
          src = ./.;
          cargoLock = { lockFile = ./Cargo.lock; };

          nativeBuildInputs = with pkgs; [ pkg-config cmake ];
          buildInputs = with pkgs;
            [ openssl zstd libiconv ]
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
              darwin.Security
              darwin.SystemConfiguration
              darwin.CoreServices
              darwin.CoreFoundation
              darwin.Foundation
            ];

          LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.libiconv ];
        } // commonEnv);
      });
}
