# CPU Fixture

This user Flake imports Nix Compute as an input and declares one job with explicit
Linux native, Linux OCI, and Apple Silicon native CPU targets. It writes a required
checkpoint through `NIX_COMPUTE_OUTPUTS`. A second `missing-output` job deliberately
omits its required checkpoint to exercise signed failure reports.

From the repository root:

```sh
devenv shell -- cargo build
devenv shell -- bash tests/e2e.sh
```

The E2E script runs the Linux native target and verifies its signed proof. To build
the container target use `nix-compute build ./examples/fixture#train --target
container`; running it additionally needs a working Podman or Docker service.
Selecting the Apple target delegates its build to Nix and requires Darwin for
execution. Native targets explicitly opt into host networking and no isolation.
