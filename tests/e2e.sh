#!/usr/bin/env bash
set -euo pipefail
export RUST_BACKTRACE=0
compute="${NIX_COMPUTE_BIN:-target/debug/nix-compute}"
test_root="$(mktemp -d)"
trap 'rm -rf "$test_root"' EXIT
"$compute" inspect ./examples/fixture#train | jq -e '.schema_version == 2 and (.targets | keys == ["apple", "container", "local"])'
"$compute" inspect ./examples/fixture#train --target local | jq -e '.entrypoint[0] | startswith("/nix/store/")'
"$compute" keygen "$test_root/key.json" --center-id fixture-center
"$compute" run ./examples/fixture#train --target local --signing-key "$test_root/key.json" --state-dir "$test_root/runs"
if [[ "${NIX_COMPUTE_TEST_OCI:-0}" == 1 ]]; then
  "$compute" run ./examples/fixture#train --target container --signing-key "$test_root/key.json" --state-dir "$test_root/runs"
else
  printf 'SKIP CPU OCI E2E: NIX_COMPUTE_TEST_OCI is not enabled\n'
fi
if "$compute" run ./examples/fixture#missing-output --target local --signing-key "$test_root/key.json" --state-dir "$test_root/runs"; then
  printf 'A missing required output was incorrectly accepted\n' >&2
  exit 1
fi
jq '{version: 1, centers: [{center_id, key_id, public_key, revoked: false}]}' "$test_root/key.json" > "$test_root/trust.json"
proof=("$test_root"/runs/*/attestation.json)
for report in "${proof[@]}"; do
  "$compute" verify "$report" "$test_root/trust.json"
  jq -e '.version == 2 and .payload.exit_code == 0 and (.payload.target_identity.closure | length > 0) and
    (if .payload.job_identity.job.name == "missing-output" then
      .payload.status == "failed" and (.payload.error | contains("required output"))
    else
      .payload.status == "succeeded" and (.payload.inputs | length == 1) and (.payload.outputs | length == 1)
    end)' "$report"
done
jq '.payload.target_id = "tampered"' "${proof[0]}" > "$test_root/tampered.json"
if "$compute" verify "$test_root/tampered.json" "$test_root/trust.json"; then
  exit 1
fi
printf 'CPU E2E passed\n'
