# Target and Fulfillment Contract

## Flake API

A user submits `<locked-flake>#<job>` and optionally a target name. Job-level
`artifacts`, `parameters`, and `reproducibility` define the common identity.
Job-level `resources` and `execution` supply defaults to each target. Target-level
definitions override these defaults; `perTarget` and `targets.<name>` otherwise
merge as ordinary Nix modules. Backend modules use `mkDefault`.

```nix
compute.jobs.train = {
  parameters = { epochs = 10; learning_rate = 0.001; };
  reproducibility.seed = 42;
  artifacts.outputs.weights.path = "model/weights.bin";
  targets.cuda = {
    system = "x86_64-linux";
    imports = [ nix-compute.acceleratorModules.cuda ];
    accelerator = {
      count = 2;
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

The referenced trainer, SDK environment and Python interpreter are user-defined
derivations. `accelerator.python` enables the bundled vendor probe; alternatively
set `accelerator.probe` to a custom executable derivation implementing the protocol
below. External libtpu or CANN archives/wheels must be packaged with explicit
sources and fixed hashes in the locked Flake. No adapter installs dependencies
with pip, apt, or another package manager during execution.

`runtimePackages` are built and included in the recorded closure, and in OCI image
contents. They do not implicitly change the trainer's library search path: the
trainer and probe derivations must provide their own SDK wrappers. The probe
environment is cleared before launch. On NixOS, those wrappers may need an
explicit host driver library path such as `/run/opengl-driver/lib`.

| Module | Systems | Executor | Probe dependencies and runtime version |
| --- | --- | --- | --- |
| `cpu` | x86_64 Linux, aarch64 Linux/Darwin | OCI on Linux or explicit native | No accelerator probe |
| `cuda` | x86_64/aarch64 Linux | OCI | CUDA PyTorch + pynvml; runtime is `torch.version.cuda` |
| `rocm` | x86_64 Linux | OCI | ROCm PyTorch + AMD SMI + libamdhip64; runtime is `torch.version.hip` |
| `tpu` | x86_64/aarch64 Linux, subject to libtpu wheel availability | Native on an existing TPU VM | JAX/PJRT + jaxlib + libtpu; runtime is jaxlib version, driver is libtpu version |
| `metal` | aarch64 Darwin | Native | PyObjC Metal bindings; host macOS version is the Metal driver/runtime version |
| `cann` | x86_64/aarch64 Linux | OCI | torch-npu + ACL; runtime is ACL version |
| `oneapi` | x86_64 Linux | OCI | Bundled compiled Level Zero/Sysman probe; runtime is the Level Zero API version (`major.minor.0`) |

These are adapter interfaces, not a guarantee that every release of each SDK
builds on every listed system. Missing APIs, unparseable versions, missing device
memory/topology or incompatible requirements cause rejection. Different vendor
API releases may require an updated locked probe. CUDA MIG needs a custom probe
that enumerates partitions with stable identities; the bundled probe rejects MIG.

## Device Inventory

The locked probe writes one JSON document to stdout and diagnostics to stderr.
It must return actual devices, never inferred counts from installed tools.
Probes run with a 30-second timeout. The bundled probes use NVML/CUDA, HIP/AMD SMI
with PCI-to-DRM mapping, JAX's local PJRT devices, Metal's unified-memory device,
ACL/Ascend device properties, and Level Zero/Sysman devices respectively. The
oneAPI probe is built automatically with nixpkgs' Level Zero headers and loader;
it does not require `accelerator.python`.

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

IDs and indexes must be unique. Driver requirements use semver syntax; two-part
driver versions gain a `.0` patch component, and numeric leading zeros are
normalized (for example, `535.104.05` becomes `535.104.5`). Runtime and architecture match
exactly. `accelerator.topology` constrains exact per-device key/value pairs;
collective bandwidth and arbitrary interconnect graph requirements are not modeled.
Minimum accelerator memory is per device. Host CPU and memory requirements are
admission checks; `resources.enforce = true` additionally requests OCI limits.

Device locks are held until execution and artifact capture complete. All runners
for a center must use the same Unix service account and shared
`NIX_COMPUTE_DEVICE_LOCK_DIR`; the default is a per-user directory in the system
temporary directory. These advisory locks coordinate Nix Compute runners, not
unrelated host processes or other schedulers. GPU admission uses total capacity,
not a guarantee that other host processes leave that capacity free.

OCI accelerators are probed again inside the restricted container before the
workload starts. The visible stable IDs must equal the allocated IDs. Local
runtime ordinals are then recomputed, avoiding accidental use of host ordinals
after device filtering. Rootless Podman device access requires a runtime that
supports `--group-add=keep-groups`, such as crun.

CUDA launches use exact CDI UUIDs; the center must configure NVIDIA CDI support in
Podman/Docker. ROCm passes KFD plus selected DRM render nodes. Intel passes only
selected render nodes. CANN passes selected davinci devices and required management
nodes, mounts `/usr/local/Ascend/driver` read-only and hashes that driver tree in the
proof. Native TPU requires the entire local topology; Metal requires one
unified-memory device. Native device visibility is an execution setting, not a
security boundary.

## Executors and Artifacts

OCI requires a local Docker or Podman service. Podman must have a center-managed
`policy.json` permitting the built local archive; the runner does not create or
override that policy. Requested limits require confirmed cgroup/controller support.
Remote container services are rejected because the inventory describes the local
host.

OCI runs as a non-root UID with a read-only image, no capabilities, no privilege
escalation, and explicit network/CPU/memory settings. Images contain Nix store
paths; no `/bin/trainer` convention is needed. The image config digest is read
from the archive and checked against the loaded runtime image ID. Container
creation is separate from start, and cleanup removes the named container even on
timeout or cancellation.
If container removal fails, a persistent `quarantine.json` in the device-lock
directory prevents further runs. An operator must confirm cleanup and remove that
marker before reusing the node.

Native execution requires `execution.network = "host"` and
`execution.isolation = "none"`. Requested CPU/memory limits must either be absent
or set `resources.enforce = false`. Native execution clears the inherited
environment and runs in a dedicated process group; timeout/SIGINT/SIGTERM kills
the group. It provides no filesystem sandbox, cgroup limits or protection against
hostile processes escaping that group. Use native jobs only under an appropriate
center trust policy. SIGKILL, machine failure and an unreachable container daemon
require external node supervision and recovery.

Inputs use `uri`, `sha256`, and relative `path`. Supported transports are local
`file://`, `cas://sha256/<digest>` with `NIX_COMPUTE_CAS_ROOT`, and HTTP(S), including
presigned URLs. `s3://` uses `NIX_COMPUTE_S3_ENDPOINT` as an anonymous HTTP gateway;
it does not implement AWS authentication or the complete S3 protocol.

Outputs use relative `path`, `required` (default true), and optional `destination`.
Without a destination they stay in the run directory. Destinations are `file://`
or the same S3 gateway. Outputs are regular files, not directories. Paths may not
escape the output root through symlinks. Logs remain in `stdout.log`/`stderr.log`.
Input staging and workload/artifact failures after run creation produce a signed
failure report. Preflight/build failures occur before a run is created.

## Proof and Trust

The envelope signs a domain-separated document containing the envelope
version, algorithm, center ID, key ID, and payload. The payload binds:

- `job_id`: the immutable source/lock and common job definition.
- `target_id`: the job ID, resolved target and normalized NAR closure inventory.
- Actual allocated device identities, runtime/driver versions and launch settings.
- OCI archive SHA256/config ID when applicable; staged input and produced output digests.
- Run ID, timestamps, exit code, status and any execution/artifact error.

Canonicalization is `nix-compute-sorted-json-v1`: recursively sorted JSON keys and
serde_json scalar encoding. It is explicitly not RFC 8785 JCS. NAR inventory hashes
exclude store-local registration timestamps, trust flags and cache signatures.

`verify` authenticates a center report against a local trust store without network
access. A trusted center's signature does not independently prove resource use,
honest execution, model quality or numerical equivalence. Native host services,
drivers and available hardware remain outside the Nix build closure and are
reported separately.

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

Private keys are created exclusively with Unix mode 0600; keygen refuses to
overwrite a file. Keep private keys outside submitted Flakes. Trust-store
distribution, revocation policy and replay prevention belong to the fulfillment
platform. Legacy envelope version 1 signatures remain verifiable.
