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
          rustPackage = pkgs.rustPlatform.buildRustPackage {
            pname = "nix-compute";
            version = "0.2.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.pkg-config ];
          };
        in
        {
          formatter = pkgs.nixfmt;
          checks = {
            modules = import ./nix/tests.nix { inherit pkgs; };
            rust = rustPackage;
          }
          // nixpkgs.lib.optionalAttrs (system == "x86_64-linux") {
            oneapi-probe = (import ./nix/lib.nix { inherit (pkgs) lib; }).mkProbe {
              inherit pkgs;
              backend = "oneapi";
            };
          };
          packages.default = rustPackage;
          apps.default = {
            type = "app";
            program = "${rustPackage}/bin/nix-compute";
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
