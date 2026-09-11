{
  lib,
  config,
  targetName,
  ...
}:
let
  inherit (lib) mkOption types;
in
{
  imports = [ ./execution-options.nix ];
  options = {
    system = mkOption {
      type = types.enum [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
    };
    executor = mkOption {
      type = types.enum [
        "oci"
        "native"
      ];
      default = "oci";
    };
    program = mkOption {
      type = types.package;
      description = "Locked program derivation.";
    };
    entrypoint = mkOption {
      type = types.nonEmptyListOf types.str;
      default = [ (lib.getExe config.program) ];
    };
    image.extraPackages = mkOption {
      type = types.listOf types.package;
      default = [ ];
    };
    assertions = mkOption {
      type = types.listOf types.attrs;
      default = [ ];
      internal = true;
    };
    accelerator = {
      id = mkOption {
        type = types.enum [
          "cpu"
          "cuda"
          "rocm"
          "tpu"
          "metal"
          "cann"
          "oneapi"
        ];
        default = "cpu";
      };
      count = mkOption {
        type = types.ints.positive;
        default = 1;
      };
      min_memory_mib = mkOption {
        type = types.ints.unsigned;
        default = 0;
      };
      architectures = mkOption {
        type = types.listOf types.str;
        default = [ ];
      };
      driver_range = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Semver range for the host driver; required for accelerators.";
      };
      runtime_version = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Exact expected probe runtime version.";
      };
      topology = mkOption {
        type = types.attrsOf types.str;
        default = { };
        description = "Exact per-device topology constraints.";
      };
      runtimePackages = mkOption {
        type = types.listOf types.package;
        default = [ ];
      };
      python = mkOption {
        type = types.nullOr types.package;
        default = null;
        description = "Locked interpreter with vendor SDK bindings for the bundled probe.";
      };
      probe = mkOption {
        type = types.nullOr types.package;
        default = null;
        description = "Locked probe executable emitting the device inventory protocol.";
      };
    };
  };
  config.assertions = [
    {
      assertion = config.resources.nodes == 1 || config.execution.network == "host";
      message = "multi-node tasks require network = host for peer connectivity";
    }
    {
      assertion = builtins.match "[A-Za-z0-9_-]+" targetName != null;
      message = "invalid target name ${targetName}";
    }
    {
      assertion = config.executor != "oci" || lib.hasSuffix "-linux" config.system;
      message = "OCI execution requires Linux";
    }
    {
      assertion =
        config.accelerator.id != "metal"
        || (config.system == "aarch64-darwin" && config.executor == "native");
      message = "Metal requires native Apple Silicon Darwin";
    }
    {
      assertion =
        config.accelerator.id != "tpu"
        || (lib.hasSuffix "-linux" config.system && config.executor == "native");
      message = "TPU requires native Linux on an existing TPU VM";
    }
    {
      assertion =
        !(builtins.elem config.accelerator.id [
          "cuda"
          "rocm"
          "cann"
          "oneapi"
        ])
        || (lib.hasSuffix "-linux" config.system && config.executor == "oci");
      message = "CUDA/ROCm/CANN/oneAPI require Linux OCI execution";
    }
    {
      assertion =
        !(builtins.elem config.accelerator.id [
          "rocm"
          "oneapi"
        ])
        || config.system == "x86_64-linux";
      message = "bundled ROCm/oneAPI adapters support x86_64-linux";
    }
  ];
}
