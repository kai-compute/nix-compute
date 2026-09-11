{
  inputs.nix-compute.url = "path:@COMPUTE_SOURCE@";
  inputs.nixpkgs.follows = "nix-compute/nixpkgs";
  inputs.flake-parts.follows = "nix-compute/flake-parts";
  outputs =
    inputs@{
      self,
      nixpkgs,
      flake-parts,
      nix-compute,
      ...
    }:
    let
      source = builtins.fromJSON (builtins.readFile ./source.json);
      pkgs = import nixpkgs { system = "x86_64-linux"; };
    in
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" ];
      imports = [ nix-compute.flakeModule ];
      compute.jobs.cached = {
        artifacts.inputs.dataset = {
          source = pkgs.fetchurl { inherit (source) url sha256; };
          path = "dataset.txt";
        };
        artifacts.outputs.checkpoint.path = "checkpoint";
        targets.cpu = {
          system = "x86_64-linux";
          imports = [ nix-compute.acceleratorModules.cpu ];
          executor = "native";
          resources.enforce = false;
          execution = {
            network = "host";
            isolation = "none";
            timeout_seconds = 10;
          };
          program = pkgs.writeShellApplication {
            name = "cached-trainer";
            runtimeInputs = [ pkgs.coreutils ];
            text = ''cp "$NIX_COMPUTE_INPUTS/dataset.txt" "$NIX_COMPUTE_OUTPUTS/checkpoint"'';
          };
        };
      };
    };
}
