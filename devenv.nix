{ pkgs, ... }:

{
  packages = with pkgs; [
    jq
    nix
    nixfmt
    podman
    python3
    git
  ];

  languages.nix = {
    enable = true;
    lsp = {
      enable = true;
      package = pkgs.nixd;
    };
  };

  scripts.nix-lsp.exec = ''
    set -euo pipefail
    cd "$DEVENV_ROOT"
    # nixd loads package and option indexes through workspace/configuration.
    nixd_expr="$(devenv --quiet lsp --print-config | ${pkgs.jq}/bin/jq -er '
      .nixd | "{ pkgs = \(.nixpkgs.expr); options = \(.options.devenv.expr); }"
    ')"
    printf '%s\n' "$nixd_expr" > .devenv/nixd.nix
    exec ${pkgs.nixd}/bin/nixd "$@"
  '';

  languages.rust = {
    enable = true;
    channel = "stable";
  };

  env.RUST_BACKTRACE = "1";

  enterShell = ''
    echo "nix-compute development shell"
    echo "Run: cargo test"
  '';
}
