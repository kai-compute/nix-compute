#!/usr/bin/env bash
set -euo pipefail
compute="${NIX_COMPUTE_BIN:-target/debug/nix-compute}"
provider="${NIX_COMPUTE_PROVIDER_BIN:-target/debug/nix-compute-provider-reference}"
test_root="$(mktemp -d)"
pids=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done
  rm -rf "$test_root"
}
trap cleanup EXIT
"$provider" keygen "$test_root/key.json" --center-id fixture-center
jq '{version: 1, centers: [{center_id, key_id, public_key, revoked: false}]}' "$test_root/key.json" > "$test_root/trust.json"
for scenario in distributed distributed-failure; do
  task="$("$compute" build "./examples/fixture#$scenario" --target cpu)"
  port="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
  for rank in 0 1; do
    jq -n --arg run "$scenario" --argjson rank "$rank" --argjson port "$port" '{run_id: $run, node_id: ("node-" + ($rank|tostring)), node_rank: $rank, node_count: 2, master_addr: "127.0.0.1", master_port: $port}' > "$test_root/context-$rank.json"
    "$provider" run "$task" --context "$test_root/context-$rank.json" --coordination-dir "$test_root/group" --signing-key "$test_root/key.json" --state-dir "$test_root/runs" > "$test_root/$scenario-$rank.log" 2>&1 &
    pids+=("$!")
  done
  failures=0
  for pid in "${pids[@]}"; do
    if ! wait "$pid"; then failures=$((failures + 1)); fi
  done
  pids=()
  if [[ "$scenario" == distributed ]]; then
    if [[ "$failures" != 0 ]]; then
      cat "$test_root"/distributed-*.log "$test_root"/runs/distributed/node-*/stderr.log
      exit 1
    fi
    for rank in 0 1; do
      jq -e --argjson rank "$rank" '.rank == $rank and .world_size == 2 and (.weight > 1.999 and .weight < 2.001)' "$test_root/runs/$scenario/node-$rank/outputs/metrics.json"
    done
    test -f "$test_root/runs/$scenario/node-0/outputs/model/weights.pt"
    jq -e '.payload.outputs | map(select(.name == "model")) | length == 1' "$test_root/runs/$scenario/node-0/attestation.json"
    jq -e '.payload.outputs | map(select(.name == "model")) | length == 0' "$test_root/runs/$scenario/node-1/attestation.json"
  else
    test "$failures" == 2
  fi
  for report in "$test_root/runs/$scenario"/node-*/attestation.json; do
    "$provider" verify "$report" "$test_root/trust.json" > /dev/null
    jq -e --arg scenario "$scenario" 'if $scenario == "distributed" then .payload.status == "succeeded" else .payload.status != "succeeded" end' "$report"
  done
done
printf 'Two-node PyTorch training and group failure tests passed\n'
python3 tests/provider-lifecycle.py
