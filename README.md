# Nix Compute

Repository: [kai-compute/nix-compute](https://github.com/kai-compute/nix-compute).

Nix Compute builds reproducible compute tasks as outputs of a user-owned Nix Flake.
The user Flake imports this repository as `nix-compute`, imports `nix-compute.flakeModule`,
and declares jobs under `compute.jobs`. A compute platform selects a Target and
routes it to a Provider. The Provider allocates resources and executes the built
task. Nix Compute itself does not schedule machines or run training during builds.

```nix
inputs.nix-compute.url = "github:kai-compute/nix-compute";

imports = [ nix-compute.flakeModule ];

compute.jobs.train = {
  artifacts.inputs.dataset = { source = ./data; path = "dataset"; };
  artifacts.outputs.model = { path = "model"; kind = "directory"; };
  targets.cpu = {
    system = "x86_64-linux";
    imports = [ nix-compute.acceleratorModules.cpu ];
    resources = { cpu_cores = 16; memory_mib = 65536; };
  };
  perTarget = { system, pkgs, accelerator, ... }: {
    program = self.packages.${system}."trainer-${accelerator.id}";
  };
};
```

The module generates:

- `computeTasks.<job>.<target>`: a complete, cacheable task output containing
  `task.json`, a program reference, and an image reference for OCI targets.
- `computeJobs.<job>.targets.<target>`: execution metadata.
- `computePrograms.<job>.<target>`: the program derivation.
- `computeImages.<job>.<target>`: an image archive, only for OCI targets.
- `computeProbes` and `computeRuntimes`: locked probe and SDK derivations.

`targets` and `perTarget` are real Nix modules: `imports`, typed options,
`lib.mkDefault`, and `lib.mkForce` compose normally. Each target supplies its own
`system`; Flake Parts' global `systems` only controls this project's CLI packages
and checks. Target modules receive `system`, `pkgs`, `accelerator`, `jobName`,
`targetName`, and `config`. The default entrypoint is `lib.getExe program`.

The built-in accelerator modules are `cpu`, `cuda`, `rocm`, `tpu`, `metal`,
`cann`, and `oneapi`. Programs and SDKs must actually support the declared target;
importing a module does not translate CUDA code to another backend. See
[the target and probe contract](docs/protocol.md) for SDK configuration, supported
systems, device allocation, and native execution requirements.

Input `source` values are Nix derivations or paths, including directories. Declare
downloads with fixed hashes and preprocessing as derivations. Programs and inputs
are built once and distributed through ordinary Nix binary caches. Data remains
independent of OCI images and can be reused across targets.

`resources.nodes` defaults to one. CPU, memory, and `accelerator.count` are per
node; `accelerator.min_memory_mib` is per device. A target with two nodes and eight
accelerators requests two homogeneous eight-device nodes. Different targets are
alternative implementations, not separate nodes of one cluster.

Programs read `NIX_COMPUTE_INPUTS` and write `NIX_COMPUTE_OUTPUTS`; artifact paths
are relative to those directories. Parameters are JSON in
`NIX_COMPUTE_PARAMETERS`, and a declared seed is available as `NIX_COMPUTE_SEED`.
Outputs declare `kind = "file"` or `"directory"` and `scope = "leader"` or
`"per-node"`. Defaults are file and leader. The provider supplies node rank,
node count and rendezvous information; the locked entrypoint launches the chosen
distributed framework. See [the Provider contract](docs/protocol.md).

Development uses Devenv (2.0 or newer) and Direnv. Install both, then run
`direnv allow` in the repository. The development shell provides Rust, Nix, jq,
Git, Podman, the `nixd` language server, and the `nixfmt` formatter.

### VS Code / VSCodium

Open the repository root and install the recommended **Nix IDE** extension
(`jnoortheen.nix-ide`). With `devenv` and `direnv` on the editor's PATH, the
workspace configuration runs the `nix-lsp` development-shell command through
`direnv exec` to load the project environment, package completions, and Devenv
option documentation. At startup, the command uses `devenv lsp --print-config`
to generate `.devenv/nixd.nix`, which the editor's LSP settings reference. This
keeps completions aligned with the project's locked inputs without committing
machine-specific paths. Nix files are formatted with `nixfmt` on save.

After changing `devenv.nix`, `devenv.yaml`, or `devenv.lock`, run **Nix: Restart
The Nix Language Server Process** from the command palette. For Remote SSH,
WSL, or containers, install Nix IDE and the environment tools on the remote side.

The standalone `nix develop` shell also includes `nixd` and `nixfmt`;
`nix fmt` uses the same formatter. The VS Code configuration uses Devenv for its
project-specific option completions.

## Repository Migration

For an existing checkout, update the remote:

```sh
git remote set-url origin https://github.com/kai-compute/nix-compute
```

For downstream Flakes, use `github:kai-compute/nix-compute` for the
`nix-compute` input, then refresh its lock entry with
`nix flake update nix-compute`. Review the resulting revision change before
committing `flake.lock`. The fixtures in this repository use local path inputs
so they continue to test the current checkout.

## CLI

Run the CLI directly from the repository:

```sh
nix run github:kai-compute/nix-compute -- --help
nix run github:kai-compute/nix-compute#provider-reference -- --help
```

From a local checkout:

```sh
nix run . -- inspect ./examples/fixture#train
nix run . -- inspect ./examples/fixture#train --target local
nix run . -- build ./examples/fixture#train --target local
nix build ./examples/fixture#computeTasks.train.local
```

`inspect` lists target summaries without evaluating programs or SDKs. `inspect
--target` resolves the selected target. `validate` uses the same contract checks.
`build --target` builds the complete task closure and delegates cross-system builds
to Nix. Multiple targets require explicit selection. None of these commands
requires the requested accelerator on the build host.

The CLI archives submitted sources, requires `flake.lock`, and evaluates/builds
from the same immutable snapshot with lock updates disabled. Transfer a task with
`nix copy --to <store-or-cache> <task-store-path>`; its closure includes program,
data, SDKs, source and applicable image. Training outputs are produced at runtime,
not automatically reused as Nix build results.

## Provider Reference

The independent `nix-compute-provider-reference` package consumes an already built
task. It owns the former execution, device allocation and signing commands:

```sh
task=$(nix run . -- build ./examples/fixture#train --target local)
nix run .#provider-reference -- keygen .nix-compute/center-key.json
nix run .#provider-reference -- run "$task" \
  --signing-key .nix-compute/center-key.json
nix run .#provider-reference -- verify <attestation.json> <trust.json>
```

It does not select a Target, evaluate a user Flake or build dependencies. The
reference multi-node implementation uses a provider-owned shared coordination
directory for readiness, heartbeats and failure propagation. Production Providers
may implement the same lifecycle with their own cluster managers.

Task schema v3 replaces URI inputs with buildable `source` inputs. Rebuild older
task metadata after migrating inputs. Existing signature envelope versions remain
verifiable. See [migration and execution details](docs/protocol.md) and the
[two-node PyTorch example](examples/fixture/README.md).

## Verification

```sh
devenv shell -- cargo fmt --all --check
devenv shell -- cargo clippy --workspace --all-targets -- -D warnings
devenv shell -- cargo test --workspace
devenv shell -- python3 -m unittest discover -s tests -p 'test_*.py'
devenv shell -- cargo build --workspace
nix build .#checks.x86_64-linux.modules --no-link
devenv shell -- bash tests/e2e.sh
devenv shell -- bash tests/distributed.sh
devenv shell -- bash tests/cache.sh
bash tests/hardware-smoke.sh
```

Routine CI is configured for CPU native/OCI execution and module, adapter,
lifecycle and signature tests.
The hardware smoke script explicitly reports skipped backends unless a hardware
fixture and signing key are configured. Vendor probes and device launch arguments
require validation on the actual center's hardware and driver stack before use.
Scheduling, routing, billing and provisioning belong to the compute platform and
Providers. Company branding is not used as a component name.
