#!/usr/bin/env bash
set -euo pipefail
export RUST_BACKTRACE=0
compute="${NIX_COMPUTE_BIN:-target/debug/nix-compute}"
provider="${NIX_COMPUTE_PROVIDER_BIN:-target/debug/nix-compute-provider-reference}"
test_root="$(mktemp -d)"
trap 'rm -rf "$test_root"' EXIT
if [[ "${NIX_COMPUTE_TEST_DOCKER:-0}" == 1 ]]; then
  docker info > /dev/null
  mkdir "$test_root/bin"
  printf '#!/bin/sh\nexit 1\n' > "$test_root/bin/podman"
  chmod +x "$test_root/bin/podman"
  export PATH="$test_root/bin:$PATH"
  export NIX_COMPUTE_TEST_OCI=1
fi
"$compute" inspect ./examples/fixture#train | jq -e '.schema_version == 3 and (.targets | keys == ["apple", "container", "local"])'
"$compute" inspect ./examples/fixture#train --target local | jq -e '.entrypoint[0] | startswith("/nix/store/")'
"$provider" keygen "$test_root/key.json" --center-id fixture-center
task="$("$compute" build ./examples/fixture#train --target local)"
jq -e '.schema_version == 3 and .target.resources.nodes == 1 and .job.artifacts.inputs.dataset.source' "$task/task.json"
"$provider" run "$task" --signing-key "$test_root/key.json" --state-dir "$test_root/runs"
task="$("$compute" build ./examples/fixture#linked-input --target local)"
"$provider" run "$task" --signing-key "$test_root/key.json" --state-dir "$test_root/runs"
if [[ "${NIX_COMPUTE_TEST_OCI:-0}" == 1 ]]; then
  task="$("$compute" build ./examples/fixture#train --target container)"
  "$provider" run "$task" --signing-key "$test_root/key.json" --state-dir "$test_root/runs"
  task="$("$compute" build ./examples/fixture#linked-input --target container)"
  "$provider" run "$task" --signing-key "$test_root/key.json" --state-dir "$test_root/runs"
else
  printf 'SKIP CPU OCI E2E: NIX_COMPUTE_TEST_OCI is not enabled\n'
fi
task="$("$compute" build ./examples/fixture#missing-output --target local)"
if "$provider" run "$task" --signing-key "$test_root/key.json" --state-dir "$test_root/runs"; then
  printf 'A missing required output was incorrectly accepted\n' >&2
  exit 1
fi
jq '{version: 1, centers: [{center_id, key_id, public_key, revoked: false}]}' "$test_root/key.json" > "$test_root/trust.json"
proof=("$test_root"/runs/*/node-*/attestation.json)
for report in "${proof[@]}"; do
  "$provider" verify "$report" "$test_root/trust.json"
  if [[ "${NIX_COMPUTE_TEST_DOCKER:-0}" == 1 ]] && jq -e '.payload.target_identity.target.executor == "oci"' "$report" > /dev/null; then
    jq -e '.payload.host.container_runtime == "docker"' "$report"
  fi
  if jq -e '.payload.job_identity.job.name == "linked-input"' "$report" > /dev/null; then
    cmp examples/fixture/samples.txt "$(dirname "$report")/outputs/checkpoint"
  fi
  jq -e '.version == 2 and .payload.schema_version == 3 and .payload.exit_code == 0 and (.payload.target_identity.closure | length > 0) and
    (if .payload.job_identity.job.name == "missing-output" then
      .payload.status == "failed" and (.payload.error | contains("required output"))
    else
      .payload.status == "succeeded" and (.payload.inputs | length == 1) and (.payload.outputs | length == 1)
    end)' "$report"
done
jq '.payload.target_id = "tampered"' "${proof[0]}" > "$test_root/tampered.json"
if "$provider" verify "$test_root/tampered.json" "$test_root/trust.json"; then
  exit 1
fi
printf 'CPU E2E passed\n'
