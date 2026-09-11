{ lib, ... }:
let
  inherit (lib) mkOption types;
in
{
  options = {
    resources = {
      nodes = mkOption {
        type = types.ints.positive;
        default = 1;
        description = "Number of homogeneous nodes; CPU, memory and accelerator counts are per node.";
      };
      cpu_cores = mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
      };
      memory_mib = mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
      };
      enforce = mkOption {
        type = types.bool;
        default = true;
        description = "Require resource limits, not just admission checks.";
      };
    };
    execution = {
      network = mkOption {
        type = types.enum [
          "none"
          "host"
        ];
        default = "none";
      };
      isolation = mkOption {
        type = types.enum [
          "container"
          "none"
        ];
        default = "container";
      };
      timeout_seconds = mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
      };
      env = mkOption {
        type = types.attrsOf types.str;
        default = { };
      };
    };
  };
}
