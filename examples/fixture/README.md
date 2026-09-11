# Task and Provider Fixtures

`train` supplies native Linux, OCI Linux and native Apple Silicon CPU targets.
Its input is a cached Nix path. `missing-output` verifies that a successful process
which omits a required output still produces a failed signed report.

```sh
cargo build --workspace
bash tests/e2e.sh
NIX_COMPUTE_TEST_OCI=1 bash tests/e2e.sh
```

OCI requires a local Podman/Docker runtime and an operator-managed image trust
policy. The Apple target can be built using a Darwin builder and runs on Darwin.

## Two-Node Training

`distributed` trains a linear model with actual PyTorch DDP and Gloo on two CPU
nodes. Its dataset directory and PyTorch environment are built and cached by Nix.
The node entrypoint starts one `torchrun` process per node, using the Provider's
node count, rank and rendezvous environment. Node 0 writes a model directory; both
nodes write independent metrics. `distributed-failure` deliberately fails rank 1.

```sh
nix build ./examples/fixture#computeTasks.distributed.cpu
bash tests/distributed.sh
```

The test simulates two nodes using separate Provider processes and workspaces on
one host. It uses real framework communication and verifies synchronized model
weights and failure propagation; it does not benchmark a physical cluster.

For physical nodes, copy the task closure to each node and provide a context file
with the same run ID, task, node count and rendezvous endpoint, but a distinct node
ID and rank. Each Provider runs:

```sh
nix-compute-provider-reference run /nix/store/...-compute-task-distributed-cpu \
  --context node.json --coordination-dir /provider/shared/coordination \
  --signing-key provider-key.json --state-dir /provider/runs
```

See [the Provider contract](../../docs/protocol.md) for the context format and
shared-directory requirements. Different attempts must use different run IDs.

## Cache Delivery

```sh
bash tests/cache.sh
```

This builds a separate temporary fixture with a fixed-hash HTTP dataset, exports
the task closure to a binary cache, stops the dataset server and removes the
original Flake. It imports and verifies the closure in an isolated Nix store, then
executes the built task using cached inputs. No dataset URL is consulted by the
Provider.
