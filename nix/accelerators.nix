let
  module =
    id: executor:
    {
      lib,
      pkgs,
      config,
      ...
    }:
    {
      accelerator.id = lib.mkDefault id;
      executor = lib.mkDefault executor;
      accelerator.probe =
        lib.mkIf (id == "oneapi" || (id != "cpu" && config.accelerator.python != null))
          (
            lib.mkDefault (
              (import ./lib.nix { inherit lib; }).mkProbe {
                inherit pkgs;
                python = config.accelerator.python;
                backend = id;
              }
            )
          );
    };
in
{
  cpu = module "cpu" "oci";
  cuda = module "cuda" "oci";
  rocm = module "rocm" "oci";
  tpu = module "tpu" "native";
  metal = module "metal" "native";
  cann = module "cann" "oci";
  oneapi = module "oneapi" "oci";
}
