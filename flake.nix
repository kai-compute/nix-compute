{
  description = "Nix Compute: Flake-native reproducible jobs for distributed compute centers";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
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
          rustPackage =
            pname:
            pkgs.rustPlatform.buildRustPackage {
              inherit pname;
              version = "0.3.0";
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;
              nativeBuildInputs = [ pkgs.pkg-config ];
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
            packages = [
              pkgs.rustc
              pkgs.cargo
              pkgs.rustfmt
              pkgs.clippy
              pkgs.nix
              pkgs.nixd
              pkgs.nixfmt
              pkgs.jq
              pkgs.podman
            ];
          };
        };
    };
}
