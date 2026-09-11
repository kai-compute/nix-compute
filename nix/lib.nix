{ lib }:
{
  apiVersion = 2;
  # The interpreter must include the selected, locked vendor SDK Python bindings.
  mkProbe =
    {
      pkgs,
      python ? null,
      backend,
    }:
    assert lib.assertMsg (builtins.elem backend [
      "cuda"
      "rocm"
      "tpu"
      "metal"
      "cann"
      "oneapi"
    ]) "unknown accelerator backend";
    assert lib.assertMsg (
      backend == "oneapi" || python != null
    ) "this probe requires a locked Python interpreter with vendor SDK bindings";
    if backend == "oneapi" then
      pkgs.stdenv.mkDerivation {
        pname = "nix-compute-probe-oneapi";
        version = "0.2.0";
        src = ../probes/oneapi.cpp;
        dontUnpack = true;
        buildInputs = [
          pkgs.level-zero
          pkgs.nlohmann_json
        ];
        buildPhase = ''
          $CXX -std=c++17 -Wall -Wextra -Werror "$src" -lze_loader -o nix-compute-probe-oneapi
        '';
        installPhase = ''
          install -Dm755 nix-compute-probe-oneapi "$out/bin/nix-compute-probe-oneapi"
        '';
        meta.mainProgram = "nix-compute-probe-oneapi";
      }
    else
      pkgs.writeShellApplication {
        name = "nix-compute-probe-${backend}";
        text = ''
          exec ${lib.getExe python} ${../probes/inventory.py} ${lib.escapeShellArg backend}
        '';
      };
}
