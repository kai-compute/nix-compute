# Task Build and Provider Contract

## Responsibilities

Nix Compute is a build tool. It evaluates a user-owned Flake and builds immutable
task outputs. A compute platform selects a Target and routes it to a Provider.
The Provider imports the closure, allocates nodes and devices, runs the task and
returns outputs and signed execution reports. Company names are not component
names. No platform service or cloud API adapter is part of the builder.

## Flake API (Schema v3)

```nix
compute.jobs.train = {
  artifacts.inputs.dataset = {
    source = self.packages.x86_64-linux.dataset;
    path = "dataset";
  };
  artifacts.outputs.model = {
    path = "model";
    kind = "directory";
    scope = "leader";
  };
  artifacts.outputs.metrics = {
    path = "metrics.json";
    scope = "per-node";
  };
  parameters = { epochs = 10; learning_rate = 0.001; };
  reproducibility.seed = 42;
  targets.cuda = {
    system = "x86_64-linux";
    imports = [ nix-compute.acceleratorModules.cuda ];
    resources.nodes = 2;
    execution.network = "host";
    accelerator = {
      count = 8;
      architectures = [ "sm_90" ];
      min_memory_mib = 80000;
      driver_range = ">=550.54.15, <600.0.0";
      runtime_version = "12.4";
    };
  };
  perTarget = { system, accelerator, ... }: {
    program = self.packages.${system}."trainer-${accelerator.id}";
    accelerator.python = self.packages.${system}."probe-python-${accelerator.id}";
    accelerator.runtimePackages = [ self.packages.${system}."runtime-${accelerator.id}" ];
  };
};
```

The referenced trainer, dataset, runtime and interpreter are user-defined Nix
derivations. Input `source` accepts a derivation or a Nix path and can contain a
file or directory. Define fetching with fixed hashes and preprocessing as ordinary
Nix derivations. Inputs are realized during task construction and reused through
binary caches. The Provider does not fetch the original dataset URL at runtime.
Nix store access and binary-cache credentials belong to the build/delivery
infrastructure; do not put credentials into a task or its store contents.

Job-level artifacts, parameters and reproducibility identify the common job.
Job-level resources and execution settings supply defaults to each Target.
`targets.<name>` and `perTarget` merge as ordinary Nix modules, with backend
defaults supplied using `mkDefault`. Different Targets are alternative ways to
execute a job, not cluster members. The platform must select one Target.

`resources.nodes` is a positive count, defaulting to 1. CPU cores, host memory and
`accelerator.count` apply independently to each node. Accelerator minimum memory
is per device. All nodes of a Target must meet the same system, driver, runtime
and resource requirements. Multi-node tasks require `execution.network = "host"`
and peer connectivity supplied by the Provider. Elastic membership, heterogeneous
worker roles and interconnect bandwidth guarantees are not modeled in v3.

## Built Task

`computeJobs.<job>.targets` exposes routing summaries; requesting a summary must
not force programs or SDKs for other Targets. Resolving a selected Target exposes
its program and execution metadata. Neither operation probes the build machine.

Build a complete task with either:

```sh
nix-compute build <locked-flake>#<job> --target <target>
nix build <locked-flake>#computeTasks.<job>.<target>
```

The output is a Nix store directory containing `task.json`, a `program` symlink,
and an `image` symlink for OCI Targets. Its complete closure includes the source,
program, inputs, runtime packages, probe and applicable OCI archive. Data remains
independent of OCI image construction. Existing `computePrograms`, `computeImages`,
`computeRuntimes` and `computeProbes` outputs remain available.

`task.json` contains these top-level fields:

| Field | Meaning |
| --- | --- |
| `schema_version` | `3`; unknown and older versions require rebuilding |
| `source` | Immutable source Flake store path |
| `job` | Common job metadata and exactly one selected Target summary |
| `target` | Resolved Target, entrypoint and dependency store paths |
| `image` | OCI archive store path, or `null` for native execution |

Job input metadata contains `source` and `path`; source values are realized store
paths. Output metadata contains `path`, `kind`, `scope`, `required`, and optional
`destination`. No cluster addresses, run IDs, credentials or provider signing
keys are embedded in the task. Training itself never runs as part of this build.

The CLI requires `flake.lock` and evaluates/builds from one archived source
snapshot with lock updates disabled. `--target` is required when multiple Targets
exist. Transfer the resulting closure with standard `nix copy`:

```sh
nix copy --to file:///path/to/binary-cache /nix/store/...-compute-task-train-cuda
nix copy --from https://cache.example.org /nix/store/...-compute-task-train-cuda
```

Providers must enforce their Nix store trust policy when importing artifacts.
They consume a top-level task output, validate its schema and verify that declared
dependencies belong to its closure. The reference task identity is the SHA256 of
canonical JSON containing the task store `path` and its `nar_hash`. Its report also
binds the normalized recursive closure (NAR hashes, sizes and references).

## Node Execution

The Provider reserves the complete node group, checks every node, prepares inputs
and agrees on one run ID and rendezvous endpoint before starting any workload.
It launches the declared entrypoint once per node with only allocated devices
visible. The entrypoint is responsible for starting framework processes, such as
using its locked `torchrun` or DeepSpeed launcher. Device count does not change
the training algorithm automatically.

The environment contract is:

| Variable | Meaning |
| --- | --- |
| `NIX_COMPUTE_INPUTS` | Read-only input tree, with declared relative paths |
| `NIX_COMPUTE_OUTPUTS` | Writable node-local output directory |
| `NIX_COMPUTE_PARAMETERS` | Job parameters encoded as JSON |
| `NIX_COMPUTE_SEED` | Declared seed, when present |
| `NIX_COMPUTE_NODE_RANK` | Node index, from 0 to node count minus 1 |
| `NIX_COMPUTE_NODE_COUNT` | Declared number of nodes |
| `NIX_COMPUTE_MASTER_ADDR` | Rendezvous host reachable by every node |
| `NIX_COMPUTE_MASTER_PORT` | Rendezvous port reserved by the Provider |

Node rank is not framework process rank. For example, `torchrun` uses node rank
and processes per node to assign `RANK`, `LOCAL_RANK` and `WORLD_SIZE`. The Provider
must not assume one training process per node or one GPU per process.

All nodes must become ready before launch. A failure, cancellation, timeout or
lost node must stop the remaining group. The group is successful only after all
nodes finish successfully and required outputs are collected. A new attempt uses
a new run ID; checkpoint recovery must be supported by the training program and
explicitly supplied input state.

While waiting for group completion, a node may persist a signed report with
`status = "prepared"`. This is provisional and does not establish task success.
Final `succeeded` reports are published only after every node has completed its
workload, outputs, and final report. A stalled prepared member still
requires live heartbeats; a terminated provider can leave only its provisional
report behind.

For multi-node reference runs, `attestation.json` is a symlink into the shared
coordination directory. Final reports are staged there and become visible
together through one atomic directory-reference switch after all nodes are
ready to publish. Directory access and synchronization are checked before this
switch. This committed outcome cannot be changed by a late node error. If
synchronization fails after the switch, the Provider retries and waits for storage
to recover before acknowledging success; it does not replace the published
outcome with a failure. Directory handles remain open throughout publication.
Retain the coordination directory with the run evidence, or export reports by
copying the contents of those symlinks after the providers finish.

## Reference Provider

`nix-compute-provider-reference` is a separate binary/package in the Cargo
workspace. It accepts built tasks, never selects a Target or builds dependencies:

```sh
nix-compute-provider-reference keygen provider-key.json
nix-compute-provider-reference run /nix/store/...-compute-task-train-local \
  --signing-key provider-key.json
nix-compute-provider-reference capabilities /nix/store/...-compute-task-train-local
```

For multi-node execution, each node receives a context JSON file:

```json
{
  "run_id": "training-attempt-001",
  "node_id": "worker-a",
  "node_rank": 0,
  "node_count": 2,
  "master_addr": "10.0.0.10",
  "master_port": 29500
}
```

Run the same built task on each node with its own context:

```sh
nix-compute-provider-reference run /nix/store/...-compute-task-train-cuda \
  --context node.json --coordination-dir /provider/shared/coordination \
  --coordination-timeout 60 --signing-key provider-key.json \
  --state-dir /provider/runs
```

The coordination directory must be shared by these Provider processes, support
atomic file creation/rename and be inaccessible to OCI workloads. It is a
reference coordination mechanism, not a scheduler. Rank claims prevent duplicate
members and reuse of stale attempts. Readiness, heartbeat and completion records
bind the task identity and rendezvous context. The timeout bounds readiness and
missing/stalled peer heartbeats; heartbeats use changing sequence numbers, not
synchronized wall clocks. Completion waits while other live members finish.
Failures are propagated to local process/container cancellation. Providers may
implement the same lifecycle using another coordination mechanism.

Run files live under `<state-dir>/<run-id>/node-<rank>/`, including `stdout.log`,
`stderr.log`, `outputs/` and `attestation.json`. Single-node contexts are generated
automatically. The reference implementation requires Nix on each node to inspect
the imported store; it does not require the original Flake working directory.

## Accelerators and Isolation

| Module | Systems | Executor | Probe runtime version |
| --- | --- | --- | --- |
| `cpu` | x86_64 Linux, aarch64 Linux/Darwin | OCI on Linux or explicit native | No probe |
| `cuda` | x86_64/aarch64 Linux | OCI | CUDA PyTorch `torch.version.cuda`; NVML driver |
| `rocm` | x86_64 Linux | OCI | `torch.version.hip`; AMD SMI driver |
| `tpu` | x86_64/aarch64 Linux, subject to SDK availability | Native TPU VM | jaxlib runtime, libtpu driver |
| `metal` | aarch64 Darwin | Native | Host macOS version |
| `cann` | x86_64/aarch64 Linux | OCI | ACL runtime |
| `oneapi` | x86_64 Linux | OCI | Level Zero API version (`major.minor.0`) |

Non-CPU Targets require explicit architectures, a semver `driver_range`, an exact
`runtime_version`, locked runtime packages and a locked probe. Driver versions
with two numeric components gain `.0`, and leading zeros are normalized. Runtime
and architecture matching is exact. Memory checks use total device capacity, not
current free memory. `accelerator.topology` matches exact per-device key/value
pairs; it is not a cross-node interconnect specification.

`accelerator.python` enables the bundled vendor probe; a custom `probe` derivation
can implement the same inventory protocol. oneAPI builds its compiled probe
without Python. Runtime packages do not automatically configure SDK search paths:
the program and probe must supply wrappers, including host driver library paths
when needed. Probes run with a cleared environment and a 30-second timeout.

The inventory format remains version 1:

```json
{
  "version": 1,
  "backend": "cuda",
  "devices": [{
    "id": "GPU-00000000-0000-0000-0000-000000000000",
    "index": 0,
    "architecture": "sm_90",
    "memory_mib": 81920,
    "driver_version": "550.54.15",
    "runtime_version": "12.4",
    "topology": {"pci_bus_id": "0000:01:00.0"},
    "device_nodes": [],
    "driver_mounts": {}
  }]
}
```

IDs and indexes must be unique. CUDA MIG requires a custom partition-aware probe;
the bundled probe rejects MIG. SDK releases and hardware require validation on
the actual Provider; these interfaces do not guarantee every vendor release works.

The reference Provider retains per-device advisory locks. All its processes on
one node must share a service account and `NIX_COMPUTE_DEVICE_LOCK_DIR`. These locks
do not coordinate unrelated schedulers. Providers own CPU/memory reservations;
`resources.enforce = true` additionally requests supported OCI limits.

OCI uses a local Podman/Docker runtime with an operator-supplied image trust policy.
Containers use a non-root user, read-only root filesystem, no capabilities and
explicit network/resource settings. Declared input closures are mounted read-only
at their Nix store paths, independently of the image. Top-level input symlinks
are preserved in a deterministic metadata-only layer added before image loading;
the report records the resulting image ID as well as the original archive hash.
Allocated accelerators are
probed again inside the restricted container and must expose exactly the allocated
stable device IDs. CUDA uses NVIDIA CDI; ROCm uses KFD and selected DRM nodes;
oneAPI uses selected DRM nodes; CANN includes its declared read-only host driver
mount. Native TPU allocates the complete local topology; Metal uses one device.

Container cleanup failure quarantines the local device-lock directory until an
operator verifies cleanup. Native execution requires `network = "host"`,
`isolation = "none"` and no enforced CPU/memory limits. It clears the inherited
environment and terminates the workload process group on timeout/cancellation,
but provides no filesystem sandbox. Native workloads and the reference shared
coordination directory therefore require trusted execution. SIGKILL/container
runtime failure still requires Provider-level supervision and recovery.

## Outputs and Reports

Output paths are relative to the node-local output directory. `kind` is `file`
(default) or `directory`; `scope` is `leader` (default, node 0) or `per-node`.
Required leader outputs are checked only on node 0. Other required outputs are
checked independently on each node. Symlinks and special files in outputs are
rejected, including directory descendants.

Directory outputs are encoded as an uncompressed tar with sorted entries,
normalized owner/group/mtime and normalized file/directory modes. Empty directories
are retained. The reported SHA256 hashes the tar bytes, with `encoding = "tar"`;
regular files use `encoding = "identity"`. Original output directories remain in
the run workspace. Optional destinations use `file://` or the reference Provider's
`s3://` HTTP gateway configured with `NIX_COMPUTE_S3_ENDPOINT` (not AWS signing).
Published paths include `leader/<name>` or `nodes/<rank>/<name>` before the
content-addressed `sha256/<digest>` object.

Each node signs a schema-v3 payload binding task store identity, recursive closure,
source and job identity, selected Target, supplied node context, actual devices,
driver/runtime versions, image identity, input sources, outputs and execution
status. Nix store registration timestamps and store-local signatures are excluded
from closure identity. The envelope remains version 2 with Ed25519 signatures;
legacy version-1 envelopes remain verifiable.

```sh
nix-compute-provider-reference verify attestation.json trust.json
```

```json
{
  "version": 1,
  "centers": [{
    "center_id": "center-a",
    "key_id": "key-2026-01",
    "public_key": "BASE64URL_ED25519_PUBLIC_KEY_WITHOUT_PADDING",
    "revoked": false
  }]
}
```

Keys are created exclusively with Unix mode 0600. Verification is offline and
authenticates the Provider's report; it is not hardware remote attestation or proof
that an untrusted Provider did the reported work. Numerical equivalence across
hardware is not guaranteed. Trust distribution, revocation and replay protection
belong to the platform and Provider.

## Migration and Verification

- Replace input `uri`/`sha256` declarations with `source = pkgs.fetchurl { ...; }`,
  another derivation, or a local Nix path. Preserve relative input `path` values.
- Existing single-node resource declarations retain their meaning through
  `resources.nodes = 1`. Output defaults remain required files from the leader.
- Rebuild old JSON metadata as `computeTasks` outputs; v1/v2 task schemas are not
  accepted by the v3 Provider. Existing signed reports remain verifiable.
- Move execution/keygen/verify invocations to `nix-compute-provider-reference`.
  Pass a built task path to `run`, not a Flake selector.

`tests/e2e.sh` covers CPU native and optional OCI execution. `tests/distributed.sh`
runs actual two-node PyTorch/Gloo training and verifies group failure, model
directories and node outputs. `tests/cache.sh` restores a complete task into an
isolated store from a binary cache, verifies its closure and runs the task after
removing the original Flake directory and stopping the dataset server. Rust tests
cover contract rejection, rank/context mismatch, coordination failures, heartbeat
expiry, artifact containment, device matching, cancellation and signatures.
