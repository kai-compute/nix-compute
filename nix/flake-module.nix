{
  lib,
  config,
  inputs,
  ...
}:
let
  inherit (lib) mkOption types;
  commonModule = import ./job-options.nix;
  jobModule = {
    imports = [ commonModule ];
    options = {
      targets = mkOption {
        type = types.attrsOf types.deferredModule;
        default = { };
        description = "Explicit, independently evaluated execution targets.";
      };
      perTarget = mkOption {
        type = types.deferredModule;
        default = { };
        description = "A module shared by every target, with system, pkgs and accelerator arguments.";
      };
    };
  };
  common = job: {
    inherit (job) parameters reproducibility;
    artifacts = {
      inherit (job.artifacts) outputs;
      inputs = lib.mapAttrs (_: input: {
        inherit (input) path;
        source = toString (
          if builtins.isPath input.source then builtins.path { path = input.source; } else input.source
        );
      }) job.artifacts.inputs;
    };
  };
  summary = target: {
    inherit (target)
      name
      system
      executor
      accelerator
      resources
      execution
      ;
  };
  evaluated = lib.mapAttrs (
    jobName: job:
    lib.mapAttrs (
      targetName: target:
      (lib.evalModules {
        specialArgs = { inherit jobName targetName; };
        modules = [
          ./target-module.nix
          ({ config, ... }: {
            _module.args = {
              system = config.system;
              accelerator = config.accelerator;
              pkgs = import inputs.nixpkgs {
                system = config.system;
                config.allowUnfree = true;
              };
            };
          })
          {
            resources = lib.mapAttrs (_: lib.mkDefault) job.resources;
            execution = (lib.mapAttrs (_: lib.mkDefault) (builtins.removeAttrs job.execution [ "env" ])) // {
              env = lib.mapAttrs (_: lib.mkDefault) job.execution.env;
            };
          }
          job.perTarget
          target
        ];
      }).config
    ) job.targets
  ) config.compute.jobs;
  checked =
    target:
    assert lib.assertMsg (lib.all (a: a.assertion) target.assertions) (
      lib.concatStringsSep "; " (map (a: a.message) (builtins.filter (a: !a.assertion) target.assertions))
    );
    target;
  metadata =
    jobName: targetName: raw:
    let
      target = checked raw;
    in
    {
      name = targetName;
      inherit (target)
        system
        executor
        resources
        execution
        ;
      accelerator = {
        inherit (target.accelerator)
          id
          count
          min_memory_mib
          architectures
          driver_range
          runtime_version
          topology
          ;
      };
      program_output = "computePrograms.\"${jobName}\".\"${targetName}\"";
      program = toString target.program;
      entrypoint = target.entrypoint;
      probe_output =
        if target.accelerator.probe == null then null else "computeProbes.\"${jobName}\".\"${targetName}\"";
      probe = if target.accelerator.probe == null then null else lib.getExe target.accelerator.probe;
      image_output =
        if target.executor == "oci" then "computeImages.\"${jobName}\".\"${targetName}\"" else null;
      image_name = lib.toLower "nix-compute-${jobName}-${targetName}:job";
      runtime_paths = map toString target.accelerator.runtimePackages;
      runtime_output =
        if target.accelerator.runtimePackages == [ ] then
          null
        else
          "computeRuntimes.\"${jobName}\".\"${targetName}\"";
    };
in
{
  options.compute.jobs = mkOption {
    type = types.attrsOf (types.submodule jobModule);
    default = { };
    description = "Compute jobs submitted as a locked user Flake.";
  };
  config.flake = {
    computeJobs = lib.mapAttrs (
      name: job:
      assert lib.assertMsg (builtins.match "[A-Za-z0-9_-]+" name != null) "invalid compute job name";
      assert lib.assertMsg (job.targets != { }) "compute.jobs.${name} must declare targets";
      common job
      // {
        schema_version = 3;
        inherit name;
        targets = lib.mapAttrs (metadata name) evaluated.${name};
      }
    ) config.compute.jobs;
    computePrograms = lib.mapAttrs (_: lib.mapAttrs (_: target: (checked target).program)) evaluated;
    computeTasks = lib.mapAttrs (
      jobName: targets:
      lib.mapAttrs (
        targetName: raw:
        let
          target = checked raw;
          pkgs = import inputs.nixpkgs {
            system = target.system;
            config.allowUnfree = true;
          };
          resolved = metadata jobName targetName target;
          image =
            if target.executor == "oci" then config.flake.computeImages.${jobName}.${targetName} else null;
          manifest = {
            schema_version = 3;
            source = toString inputs.self.outPath;
            job = common config.compute.jobs.${jobName} // {
              schema_version = 3;
              name = jobName;
              targets.${targetName} = summary resolved;
            };
            target = resolved;
            image = if image == null then null else toString image;
          };
          json = pkgs.writeText "compute-task.json" (builtins.toJSON manifest);
        in
        pkgs.runCommand "compute-task-${jobName}-${targetName}" { } ''
          mkdir -p "$out"
          cp ${json} "$out/task.json"
          ln -s ${target.program} "$out/program"
          ${lib.optionalString (image != null) "ln -s ${image} \"$out/image\""}
        ''
      ) targets
    ) evaluated;
    computeRuntimes = lib.mapAttrs (
      _: targets:
      lib.mapAttrs (
        _: target:
        let
          pkgs = import inputs.nixpkgs {
            system = target.system;
            config.allowUnfree = true;
          };
        in
        pkgs.buildEnv {
          name = "compute-runtime";
          paths = target.accelerator.runtimePackages;
        }
      ) (lib.filterAttrs (_: target: target.accelerator.runtimePackages != [ ]) targets)
    ) evaluated;
    computeProbes = lib.mapAttrs (
      _: targets:
      lib.mapAttrs (_: target: target.accelerator.probe) (
        lib.filterAttrs (_: target: target.accelerator.probe != null) targets
      )
    ) evaluated;
    computeImages = lib.mapAttrs (
      jobName: targets:
      lib.mapAttrs (
        targetName: raw:
        let
          target = checked raw;
          pkgs = import inputs.nixpkgs {
            system = target.system;
            config.allowUnfree = true;
          };
        in
        pkgs.dockerTools.buildLayeredImage {
          name = lib.toLower "nix-compute-${jobName}-${targetName}";
          tag = "job";
          contents = [
            target.program
          ]
          ++ lib.optional (target.accelerator.probe != null) target.accelerator.probe
          ++ target.accelerator.runtimePackages
          ++ target.image.extraPackages;
          config = {
            Entrypoint = target.entrypoint;
            WorkingDir = "/workspace";
            User = "65532:65532";
            Env = lib.mapAttrsToList (key: value: "${key}=${value}") target.execution.env;
          };
        }
      ) (lib.filterAttrs (_: target: target.executor == "oci") targets)
    ) evaluated;
  };
}
