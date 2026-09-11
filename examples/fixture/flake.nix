{
  description = "Portable CPU Nix Compute fixture";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    nix-compute.url = "path:../..";
  };
  outputs =
    inputs@{ flake-parts, nix-compute, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" ];
      imports = [ nix-compute.flakeModule ];
      compute.jobs.train = {
        artifacts.inputs.dataset = {
          uri = "file://${./samples.txt}";
          sha256 = builtins.hashFile "sha256" ./samples.txt;
          path = "dataset.txt";
        };
        artifacts.outputs.checkpoint.path = "checkpoint";
        parameters.steps = 1;
        targets.local = {
          system = "x86_64-linux";
          imports = [ nix-compute.acceleratorModules.cpu ];
          executor = "native";
          execution = {
            network = "host";
            isolation = "none";
            timeout_seconds = 30;
          };
          resources.enforce = false;
        };
        targets.container = {
          system = "x86_64-linux";
          imports = [ nix-compute.acceleratorModules.cpu ];
          resources = {
            cpu_cores = 1;
            memory_mib = 256;
          };
        };
        targets.apple = {
          system = "aarch64-darwin";
          imports = [ nix-compute.acceleratorModules.cpu ];
          executor = "native";
          execution = {
            network = "host";
            isolation = "none";
          };
          resources.enforce = false;
        };
        perTarget = { pkgs, ... }: {
          program = pkgs.writeShellApplication {
            name = "trainer";
            text = ''
              read -r sample < "$NIX_COMPUTE_INPUTS/dataset.txt"
              printf 'nix-compute fixture: %s\n' "$sample" > "$NIX_COMPUTE_OUTPUTS/checkpoint"
            '';
          };
        };
      };
      compute.jobs.missing-output = {
        artifacts.outputs.checkpoint.path = "missing";
        targets.local = {
          system = "x86_64-linux";
          imports = [ nix-compute.acceleratorModules.cpu ];
          executor = "native";
          execution = {
            network = "host";
            isolation = "none";
            timeout_seconds = 10;
          };
          resources.enforce = false;
        };
        perTarget = { pkgs, ... }: {
          program = pkgs.writeShellApplication {
            name = "empty-trainer";
            text = "exit 0";
          };
        };
      };
    };
}
