{ pkgs }:
let
  inherit (pkgs) lib;
  backends = import ./accelerators.nix;
  evaluate =
    jobs:
    (lib.evalModules {
      specialArgs.inputs.nixpkgs = pkgs.path;
      specialArgs.inputs.self.outPath = ../.;
      modules = [
        ./flake-module.nix
        {
          options.flake = lib.mkOption { type = lib.types.attrs; };
          config.compute.jobs = jobs;
        }
      ];
    }).config.flake;
  target = {
    system = "x86_64-linux";
    program = pkgs.hello;
    imports = [ backends.cpu ];
  };
  flake = evaluate {
    train = {
      resources.cpu_cores = 2;
      execution.env.COMMON = "yes";
      perTarget =
        {
          lib,
          jobName,
          targetName,
          system,
          accelerator,
          config,
          pkgs,
          ...
        }:
        {
          program = lib.mkDefault pkgs.hello;
          execution.env.IDENTITY = "${jobName}/${targetName}/${system}/${accelerator.id}/${config.executor}";
        };
      targets = {
        local = target // {
          resources.cpu_cores = lib.mkForce 4;
        };
        apple = {
          system = "aarch64-darwin";
          executor = "native";
          # Neither the program nor SDK for this foreign target may be forced by a summary.
          program = throw "foreign program was evaluated";
          accelerator.runtimePackages = throw "foreign SDK was evaluated";
        };
      };
    };
  };
  summaries = builtins.mapAttrs (_: t: {
    inherit (t) system executor;
  }) flake.computeJobs.train.targets;
  bad = evaluate {
    train.targets.bad = target // {
      system = "aarch64-darwin";
    };
  };
  badType = evaluate {
    train.targets.bad = target // {
      program = _: pkgs.hello;
    };
  };
  badCommon = evaluate {
    train.targets.bad = target // {
      parameters.steps = 2;
    };
  };
  metal = evaluate {
    train.targets.metal = {
      imports = [ backends.metal ];
      system = "x86_64-linux";
      program = pkgs.hello;
    };
  };
  distributed = evaluate {
    train = {
      artifacts.inputs.dataset = {
        source = pkgs.writeText "dataset" "cached data";
        path = "data/train.txt";
      };
      artifacts.outputs.model = {
        path = "model";
        kind = "directory";
        scope = "leader";
      };
      targets.cuda = {
        imports = [ backends.cuda ];
        system = "x86_64-linux";
        resources.nodes = 2;
        execution.network = "host";
        accelerator = {
          count = 8;
          min_memory_mib = 81920;
          architectures = [ "sm_90" ];
          driver_range = ">=550.54.15";
          runtime_version = "12.4";
        };
        program = throw "routing summary forced training program";
      };
    };
  };
  badNetwork = evaluate {
    train.targets.local = target // {
      resources.nodes = 2;
    };
  };
  task = evaluate { train.targets.local = target; };
  backendSummary =
    id:
    let
      output = evaluate {
        train.targets.test = {
          imports = [ backends.${id} ];
          system = if id == "metal" then "aarch64-darwin" else "x86_64-linux";
          program = throw "summary forced program";
        };
      };
    in
    output.computeJobs.train.targets.test.accelerator.id;
  checks = [
    (flake.computeJobs.train.schema_version == 3)
    (flake.computeJobs.train.targets.local.resources.nodes == 1)
    (distributed.computeJobs.train.targets.cuda.resources.nodes == 2)
    (distributed.computeJobs.train.targets.cuda.accelerator.count == 8)
    (distributed.computeJobs.train.targets.cuda.accelerator.min_memory_mib == 81920)
    (distributed.computeJobs.train.targets.cuda.accelerator.runtime_version == "12.4")
    (distributed.computeJobs.train.artifacts.outputs.model.kind == "directory")
    (lib.hasPrefix builtins.storeDir distributed.computeJobs.train.artifacts.inputs.dataset.source)
    (!(builtins.tryEval badNetwork.computeJobs.train.targets.local.system).success)
    (lib.isDerivation task.computeTasks.train.local)
    (summaries.apple.system == "aarch64-darwin")
    (flake.computeJobs.train.targets.local.resources.cpu_cores == 4)
    (flake.computeJobs.train.targets.local.execution.env.COMMON == "yes")
    (flake.computeJobs.train.targets.local.execution.env.IDENTITY == "train/local/x86_64-linux/cpu/oci")
    (flake.computeJobs.train.targets.local.entrypoint == [ (lib.getExe pkgs.hello) ])
    (flake.computePrograms.train.local == pkgs.hello)
    (builtins.attrNames flake.computeImages.train == [ "local" ])
    (!(builtins.tryEval bad.computeJobs.train.targets.bad.system).success)
    (!(builtins.tryEval badType.computePrograms.train.bad).success)
    (!(builtins.tryEval badCommon.computeJobs.train.targets.bad.system).success)
    (!(builtins.tryEval metal.computeJobs.train.targets.metal.system).success)
    (
      map backendSummary [
        "cuda"
        "rocm"
        "tpu"
        "metal"
        "cann"
        "oneapi"
      ] == [
        "cuda"
        "rocm"
        "tpu"
        "metal"
        "cann"
        "oneapi"
      ]
    )
  ];
in
assert lib.assertMsg (lib.all (value: value) checks) "Nix Compute module regression";
pkgs.runCommand "nix-compute-module-tests" { } "touch $out"
