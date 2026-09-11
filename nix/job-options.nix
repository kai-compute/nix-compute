{ lib, ... }:
let
  inherit (lib) mkOption types;
in
{
  imports = [ ./execution-options.nix ];
  options = {
    parameters = mkOption {
      type = types.attrsOf types.anything;
      default = { };
    };
    reproducibility = {
      contract = mkOption {
        type = types.enum [ "environment-inputs" ];
        default = "environment-inputs";
      };
      seed = mkOption {
        type = types.nullOr types.ints.unsigned;
        default = null;
      };
    };
    artifacts = {
      inputs = mkOption {
        default = { };
        type = types.attrsOf (
          types.submodule {
            options = {
              source = mkOption {
                type = types.either types.package types.path;
                description = "Immutable input file or directory, realized and cached by Nix.";
              };
              path = mkOption {
                type = types.str;
                description = "Path relative to NIX_COMPUTE_INPUTS.";
              };
            };
          }
        );
      };
      outputs = mkOption {
        default = { };
        type = types.attrsOf (
          types.submodule {
            options = {
              path = mkOption {
                type = types.str;
                description = "Path relative to NIX_COMPUTE_OUTPUTS.";
              };
              destination = mkOption {
                type = types.nullOr types.str;
                default = null;
              };
              required = mkOption {
                type = types.bool;
                default = true;
              };
              kind = mkOption {
                type = types.enum [
                  "file"
                  "directory"
                ];
                default = "file";
              };
              scope = mkOption {
                type = types.enum [
                  "leader"
                  "per-node"
                ];
                default = "leader";
              };
            };
          }
        );
      };
    };
  };
}
