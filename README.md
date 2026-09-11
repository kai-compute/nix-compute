# Nix Compute

Nix Compute defines reproducible compute jobs as outputs of a user-owned Nix Flake.
The user Flake imports this repository as `nix-compute`, imports `nix-compute.flakeModule`,
and declares jobs under `compute.jobs`.

```nix
inputs.nix-compute.url = "github:KaiArtificialIntelligence/nix-compute";

imports = [ nix-compute.flakeModule ];

compute.jobs.train = {
  artifacts.outputs.checkpoint.path = "checkpoint";
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

Programs read `NIX_COMPUTE_INPUTS` and write `NIX_COMPUTE_OUTPUTS`; artifact paths
are relative to those directories. Parameters are JSON in
`NIX_COMPUTE_PARAMETERS`, and a declared seed is available as `NIX_COMPUTE_SEED`.
Required outputs must exist as files inside the output directory.

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

## CLI

```sh
nix run . -- inspect ./examples/fixture#train
nix run . -- inspect ./examples/fixture#train --target local
nix run . -- build ./examples/fixture#train --target local
nix run . -- capabilities
nix run . -- keygen .nix-compute/center-key.json
nix run . -- run ./examples/fixture#train --target local \
  --signing-key .nix-compute/center-key.json
```

`inspect` lists target summaries without evaluating programs or SDKs. `inspect
--target` resolves the selected target. `build --target` delegates cross-system
builds to Nix. `run` can select automatically only when exactly one declared target
matches the host and device requirements; zero or multiple matches are errors.
A CPU target must be explicitly declared.

Runs archive the submitted source, require `flake.lock`, and evaluate/build from
that immutable snapshot with lock updates disabled. Attestations bind the common
job to that source and bind the target to its backend, requirements, program and
NAR closure digests. They also record allocated devices, driver/runtime versions,
image digests, artifacts, timestamps and failures. Ed25519 signatures authenticate
the center's report; they are not hardware remote attestation or evidence that an
untrusted center performed the reported work. Numerical results need not be
bitwise identical across runs or hardware.

Verify offline with `nix-compute verify <attestation.json> <trust.json>`. The trust
store format is documented in [the protocol](docs/protocol.md). Existing v1
signature envelopes remain verifiable; v1 job metadata must migrate to `targets`
and `perTarget`, and absolute artifact paths must become relative paths.

## Verification

```sh
devenv shell -- cargo fmt --check
devenv shell -- cargo clippy --all-targets -- -D warnings
devenv shell -- cargo test
devenv shell -- python3 -m unittest discover -s tests -p 'test_*.py'
devenv shell -- cargo build
nix build .#checks.x86_64-linux.modules --no-link
devenv shell -- bash tests/e2e.sh
bash tests/hardware-smoke.sh
```

Routine CI is configured for CPU native/OCI execution and module, adapter,
lifecycle and signature tests.
The hardware smoke script explicitly reports skipped backends unless a hardware
fixture and signing key are configured. Vendor probes and device launch arguments
require validation on the actual center's hardware and driver stack before use.
This repository provides a local execution MVP; scheduling, billing, provisioning
and multi-node orchestration are outside its scope.
