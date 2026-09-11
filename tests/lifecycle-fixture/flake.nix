{
  inputs.nix-compute.url = "path:@COMPUTE_SOURCE@";
  inputs.nixpkgs.follows = "nix-compute/nixpkgs";
  inputs.flake-parts.follows = "nix-compute/flake-parts";

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" ];
      imports = [ inputs.nix-compute.flakeModule ];
      compute.jobs = {
        provisional = {
          targets.cpu = {
            system = "x86_64-linux";
            imports = [ inputs.nix-compute.acceleratorModules.cpu ];
            executor = "native";
            resources.nodes = 2;
            execution = {
              network = "host";
              isolation = "none";
            };
          };
          perTarget = { pkgs, ... }: {
            program = pkgs.writeShellApplication {
              name = "provisional";
              runtimeInputs = [ pkgs.coreutils ];
              text = ''
                if [ "$NIX_COMPUTE_NODE_RANK" = 1 ]; then
                  sleep 4
                fi
              '';
            };
          };
        };
        automatic = {
          artifacts.outputs.port.path = "port";
          targets.cpu = {
            system = "x86_64-linux";
            imports = [ inputs.nix-compute.acceleratorModules.cpu ];
            executor = "native";
            execution = {
              network = "host";
              isolation = "none";
            };
          };
          perTarget = { pkgs, ... }: {
            program = pkgs.writeShellApplication {
              name = "automatic";
              runtimeInputs = [ pkgs.python3 ];
              text = ''
                python3 - <<'PY'
                import os, pathlib, socket, time
                port = int(os.environ["NIX_COMPUTE_MASTER_PORT"])
                with socket.socket() as listener:
                    listener.bind((os.environ["NIX_COMPUTE_MASTER_ADDR"], port))
                    listener.listen()
                    (pathlib.Path(os.environ["NIX_COMPUTE_OUTPUTS"]) / "port").write_text(str(port))
                    time.sleep(3)
                PY
              '';
            };
          };
        };
        reporting = {
          targets.cpu = {
            system = "x86_64-linux";
            imports = [ inputs.nix-compute.acceleratorModules.cpu ];
            executor = "native";
            resources.nodes = 2;
            execution = {
              network = "host";
              isolation = "none";
            };
          };
          perTarget = { pkgs, ... }: {
            program = pkgs.writeShellApplication {
              name = "reporting";
              text = "exit 0";
            };
          };
        };
        preparation = {
          targets.cpu = {
            system = "x86_64-linux";
            imports = [ inputs.nix-compute.acceleratorModules.cpu ];
            executor = "native";
            execution = {
              network = "host";
              isolation = "none";
            };
            resources = {
              nodes = 2;
              cpu_cores = 4294967295;
            };
          };
          perTarget = { pkgs, ... }: {
            program = pkgs.writeShellApplication {
              name = "never-started";
              text = "exit 0";
            };
          };
        };
        upload = {
          artifacts.outputs.checkpoint = {
            path = "checkpoint";
            required = false;
            scope = "per-node";
            destination = "s3://lifecycle";
          };
          targets.cpu = {
            system = "x86_64-linux";
            imports = [ inputs.nix-compute.acceleratorModules.cpu ];
            executor = "native";
            resources.nodes = 2;
            execution.network = "host";
            execution.isolation = "none";
            execution.timeout_seconds = 40;
          };
          perTarget = { pkgs, ... }: {
            program = pkgs.writeShellApplication {
              name = "failed-with-output";
              runtimeInputs = [ pkgs.coreutils ];
              text = ''
                if [ "$NIX_COMPUTE_NODE_RANK" = 0 ]; then
                  sleep 1
                  echo partial > "$NIX_COMPUTE_OUTPUTS/checkpoint"
                  exit 1
                fi
                echo peer-started
                sleep 30
              '';
            };
          };
        };
      };
    };
}
