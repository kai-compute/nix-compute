#!/usr/bin/env bash
set -euo pipefail
compute="${NIX_COMPUTE_BIN:-target/debug/nix-compute}"
provider="${NIX_COMPUTE_PROVIDER_BIN:-target/debug/nix-compute-provider-reference}"
for backend in cuda rocm tpu metal cann oneapi; do
  variable="NIX_COMPUTE_SMOKE_${backend^^}"
  selector="${!variable:-}"
  if [[ -z "$selector" ]]; then
    printf 'SKIP %s: no hardware fixture configured\n' "$backend"
    continue
  fi
  : "${NIX_COMPUTE_SMOKE_KEY:?set a center signing key for hardware smoke runs}"
  task="$("$compute" build "$selector" --target "$backend")"
  "$provider" run "$task" --signing-key "$NIX_COMPUTE_SMOKE_KEY"
done
