# Contributing

Keep the default setup small and useful. Changes should include installation,
first use, failure recovery and removal when those behaviors are affected.
Use ordinary language in the UI and avoid exposing infrastructure choices to
someone who only wants to submit a coding task.

Keep deployment overrides, credentials and evaluation output outside the
checkout. Toolkit and Dispatch are separate products; follow
[ECOSYSTEM.md](ECOSYSTEM.md) for changes that cross them: change the owning
contract first, then the consumers.

## Build and check

Use Rust 1.98, Python 3.11 or newer with `jsonschema` (`pip install -r scripts/requirements-ci.txt`) and Node.js 22. From the repository root:

```sh
bash scripts/build-binary.sh
bash scripts/check-source.sh
```

The native output is under `dist/<platform>/`, with dependency license texts.
For container changes, use a disposable Linux Docker Engine 28+ host:

```sh
docker build -t poundlock:candidate .
bash scripts/test-image.sh poundlock:candidate
```

Those checks create restricted workers and exercise inference routing, independent
verification, protected files, repair limits, cancellation, recovery and networking.
Do not run destructive acceptance fixtures against a production deployment.

## Review

Describe the user-visible problem, the resulting behavior and the checks performed.
Keep implementation changes focused. Add tests for meaningful failure modes;
passing an agent's own tests alone is not independent acceptance evidence.
Review credential handling, filesystem paths, network boundaries and cleanup for
changes to execution or enrollment. See [SECURITY.md](SECURITY.md).

## Continuous integration

Every pull request, forks included, runs `.github/workflows/ci.yml` on GitHub-hosted
runners with a read-only token and no secrets: secret, dependency and licence scans,
Semgrep with this repository's rules, native builds and tests on Linux and macOS for
x86_64 and ARM64, the browser suite, the image's isolation, install and restore
checks, notices inside the built artifacts, and CycloneDX SBOMs.
`.github/workflows/e2e.yml` runs `tests/e2e/run.py`: the Compose install against a
mock model server on every pull request; nightly it adds a tiny llama.cpp model and
a LiteLLM gateway with its database, used with a shared key and with managed
per-application keys. The gateway's keys are generated per run. Run it locally on a
disposable Docker host:

```sh
docker build -t poundlock:candidate .
python3 tests/e2e/run.py poundlock:candidate           # --full adds the tiny model and LiteLLM
```

## Release builds

A version tag reruns CI and the full end-to-end suite on the tagged commit. The
publish job then waits for a maintainer's approval in the `release` environment
before it pushes the tested image, attests it and the archives, and creates a draft
release. Nothing is rebuilt between testing and publishing. Keep the tag equal to
`Cargo.toml`'s version, and record which platforms were actually validated; a
workflow definition alone is not evidence of a successful build.

Release binaries are unsigned; see [node update signing](docs/node-update-signing.md).
Models and model weights are not release assets. Check dependency advisories and
redistribution notices when changing the runtime or dependency lockfiles, and give
every scanner exception a reason and an expiry date.

## Contribution terms

Contributions are accepted under the Apache License 2.0, as its section 5
describes; there is no separate agreement to sign. Patches written with an AI
assistant are welcome and reviewed like any other: the contributor is responsible
for them, and tests and provenance matter more than how a patch was written.
Do not submit code you do not have the right to contribute.

Never put real endpoints, private addresses, personal paths, credentials or raw
model output in a fixture, test or issue. Use synthetic values and reserved example
domains and address ranges.
