{
  description = "Portable CPU Nix Compute fixture";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    nix-compute.url = "path:../..";
  };
  outputs =
    inputs@{
      flake-parts,
      nix-compute,
      nixpkgs,
      ...
    }:
    let
      pkgs = import nixpkgs { system = "x86_64-linux"; };
      dataset = pkgs.runCommand "training-dataset" { } ''
        mkdir -p "$out"
        cp ${./training-data.json} "$out/samples.json"
      '';
      linkedDataset = pkgs.runCommand "linked-dataset" { } ''
        mkdir -p "$out/inner"
        cp ${./samples.txt} "$out/data"
        ln -s ../data "$out/inner/sample"
      '';
      linkedInput = pkgs.runCommand "linked-input" { } ''
        ln -s ${linkedDataset}/inner "$out"
      '';
      distributed = failRank: {
        artifacts.inputs.dataset = {
          source = dataset;
          path = "dataset";
        };
        artifacts.outputs.model = {
          path = "model";
          kind = "directory";
        };
        artifacts.outputs.metrics = {
          path = "metrics.json";
          scope = "per-node";
        };
        parameters = {
          steps = 50;
          fail_rank = failRank;
        };
        reproducibility.seed = 42;
        targets.cpu = {
          system = "x86_64-linux";
          imports = [ nix-compute.acceleratorModules.cpu ];
          executor = "native";
          resources = {
            nodes = 2;
            enforce = false;
          };
          execution = {
            network = "host";
            isolation = "none";
            timeout_seconds = 90;
          };
        };
        perTarget = { pkgs, ... }: {
          program = pkgs.writeShellApplication {
            name = "distributed-trainer";
            text = ''
              export OMP_NUM_THREADS=1
              exec ${pkgs.python3.withPackages (ps: [ ps.torch ])}/bin/python -m torch.distributed.run \
                --nnodes="$NIX_COMPUTE_NODE_COUNT" --nproc-per-node=1 \
                --node-rank="$NIX_COMPUTE_NODE_RANK" \
                --master-addr="$NIX_COMPUTE_MASTER_ADDR" --master-port="$NIX_COMPUTE_MASTER_PORT" \
                ${./distributed.py}
            '';
          };
        };
      };
    in
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" ];
      imports = [ nix-compute.flakeModule ];
      compute.jobs.distributed = distributed null;
      compute.jobs.distributed-failure = distributed 1;
      compute.jobs.linked-input = {
        artifacts.inputs.dataset = {
          source = linkedInput;
          path = "dataset";
        };
        artifacts.outputs.checkpoint.path = "checkpoint";
        targets.local = {
          system = "x86_64-linux";
          imports = [ nix-compute.acceleratorModules.cpu ];
          executor = "native";
          execution = {
            network = "host";
            isolation = "none";
          };
        };
        targets.container = {
          system = "x86_64-linux";
          imports = [ nix-compute.acceleratorModules.cpu ];
        };
        perTarget = { pkgs, ... }: {
          program = pkgs.writeShellApplication {
            name = "linked-input-trainer";
            runtimeInputs = [ pkgs.coreutils ];
            text = ''
              cat "$NIX_COMPUTE_INPUTS/dataset/sample" > "$NIX_COMPUTE_OUTPUTS/checkpoint"
            '';
          };
        };
      };
      compute.jobs.train = {
        artifacts.inputs.dataset = {
          source = ./samples.txt;
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
