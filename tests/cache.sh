#!/usr/bin/env bash
set -euo pipefail
compute="${NIX_COMPUTE_BIN:-target/debug/nix-compute}"
provider="${NIX_COMPUTE_PROVIDER_BIN:-target/debug/nix-compute-provider-reference}"
test_root="$(mktemp -d)"
server_pid=""
cleanup() {
  if [[ -n "$server_pid" ]]; then kill "$server_pid" 2>/dev/null || true; wait "$server_pid" 2>/dev/null || true; fi
  if [[ -d "$test_root/imported" ]]; then find "$test_root/imported" -type d -exec chmod u+w {} +; fi
  rm -rf "$test_root"
}
trap cleanup EXIT
mkdir -p "$test_root/http" "$test_root/flake"
printf 'cached dataset %s\n' "$test_root" > "$test_root/http/dataset.txt"
digest="$(sha256sum "$test_root/http/dataset.txt" | cut -d ' ' -f 1)"
port="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$test_root/http" > "$test_root/http.log" 2>&1 &
server_pid="$!"
for attempt in {1..50}; do
  if curl -fsS "http://127.0.0.1:$port/dataset.txt" > /dev/null 2>&1; then break; fi
  sleep 0.1
done
source_root="$(nix flake archive --json . | jq -r .path)"
sed "s|@COMPUTE_SOURCE@|$source_root|" tests/cache-fixture/flake.nix > "$test_root/flake/flake.nix"
jq -n --arg url "http://127.0.0.1:$port/dataset.txt" --arg sha256 "$digest" '{url: $url, sha256: $sha256}' > "$test_root/flake/source.json"
nix flake lock "path:$test_root/flake"
task="$("$compute" build "path:$test_root/flake#cached" --target cpu)"
input="$(jq -r '.job.artifacts.inputs.dataset.source' "$task/task.json")"
nix copy --to "file://$test_root/cache" "$task"
kill "$server_pid"
wait "$server_pid" 2>/dev/null || true
server_pid=""
rm -rf "$test_root/http" "$test_root/flake"
nix copy --no-check-sigs --from "file://$test_root/cache" --to "local?root=$test_root/imported" "$task"
nix store verify --store "local?root=$test_root/imported" --no-trust --recursive "$task"
cmp "$task/task.json" "$test_root/imported$task/task.json"
test "$(sha256sum "$test_root/imported$input" | cut -d ' ' -f 1)" == "$digest"
"$provider" keygen "$test_root/key.json"
"$provider" run "$task" --signing-key "$test_root/key.json" --state-dir "$test_root/runs"
for output in "$test_root"/runs/*/node-0/outputs/checkpoint; do
  test "$(sha256sum "$output" | cut -d ' ' -f 1)" == "$digest"
done
printf 'Binary cache restore, closure verification and source-offline execution passed\n'
