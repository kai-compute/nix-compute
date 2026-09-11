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
              uri = mkOption { type = types.str; };
              sha256 = mkOption { type = types.strMatching "[a-fA-F0-9]{64}"; };
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
            };
          }
        );
      };
    };
  };
}
