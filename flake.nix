{
  description = "Nix Compute: Flake-native reproducible jobs for distributed compute centers";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    # The Rust bindings use the C API shipped by devenv's Nix 2.35 branch.
    nix-bindings-rust.url = "github:cachix/nix-bindings-rust/2.35";
    nix-bindings-rust.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    inputs@{
      self,
      nixpkgs,
      flake-parts,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      flake = {
        flakeModule = import ./nix/flake-module.nix;
        acceleratorModules = import ./nix/accelerators.nix;
        lib = import ./nix/lib.nix { inherit (nixpkgs) lib; };
      };

      perSystem =
        { pkgs, system, ... }:
        let
          nixPackage = inputs.nix-bindings-rust.inputs.nix.packages.${system}.default;
          nixLibs = builtins.attrValues nixPackage.libs;
          nixDevLibs = builtins.concatMap (lib: [ lib (lib.dev or lib) ]) nixLibs;
          nixBuildInputs = nixDevLibs ++ [ pkgs.boehmgc pkgs.boehmgc.dev ];
          rustPackage =
            pname:
            pkgs.rustPlatform.buildRustPackage {
              inherit pname;
              version = "0.3.0";
              src = ./.;
              cargoLock = {
                lockFile = ./Cargo.lock;
                outputHashes."nix-bindings-bindgen-raw-0.1.0" = "sha256-+gHM1s1fLH9OpEufBFbxBM94iz5DIOoADM1bUfpc6gI=";
              };
              nativeBuildInputs = [ pkgs.pkg-config ];
              buildInputs = [ pkgs.stdenv.cc ] ++ nixLibs;
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              BINDGEN_EXTRA_CLANG_ARGS = "-x c++ -std=c++2a -isystem ${pkgs.stdenv.cc.libc.dev}/include";
              PKG_CONFIG_PATH = pkgs.lib.makeSearchPath "lib/pkgconfig" nixBuildInputs;
              cargoBuildFlags = [
                "-p"
                pname
              ];
              cargoTestFlags = [ "--workspace" ];
              meta.homepage = "https://github.com/kai-compute/nix-compute";
            };
        in
        {
          formatter = pkgs.nixfmt;
          checks = {
            modules = import ./nix/tests.nix { inherit pkgs; };
            rust = rustPackage "nix-compute";
            provider = rustPackage "nix-compute-provider-reference";
          }
          // nixpkgs.lib.optionalAttrs (system == "x86_64-linux") {
            oneapi-probe = (import ./nix/lib.nix { inherit (pkgs) lib; }).mkProbe {
              inherit pkgs;
              backend = "oneapi";
            };
          };
          packages.default = rustPackage "nix-compute";
          packages.provider-reference = rustPackage "nix-compute-provider-reference";
          apps.default = {
            type = "app";
            program = "${rustPackage "nix-compute"}/bin/nix-compute";
          };
          apps.provider-reference = {
            type = "app";
            program = "${rustPackage "nix-compute-provider-reference"}/bin/nix-compute-provider-reference";
          };
          devShells.default = pkgs.mkShell {
            buildInputs = [ pkgs.stdenv.cc ] ++ nixBuildInputs;
            packages = [
              pkgs.rustc
              pkgs.cargo
              pkgs.rustfmt
              pkgs.clippy
              # Use the same Nix build as nix-bindings-rust; mixing nixpkgs' Nix
              # with the 2.35 C headers makes pkg-config select incompatible APIs.
              nixPackage
              pkgs.nixd
              pkgs.nixfmt
              pkgs.jq
              pkgs.podman
              pkgs.llvmPackages.libclang
            ];
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            BINDGEN_EXTRA_CLANG_ARGS = "-x c++ -std=c++2a -isystem ${pkgs.stdenv.cc.libc.dev}/include";
            PKG_CONFIG_PATH = pkgs.lib.makeSearchPath "lib/pkgconfig" nixBuildInputs;
          };
        };
    };
}
