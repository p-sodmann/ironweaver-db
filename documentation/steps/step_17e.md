# Step 17e: Release pipeline on GitHub

Status: done (the rehearsal tag is the owner's: see Outcome)
Milestone: M4 Production 1.0
Depends on: steps 17a and 17d

Split out of step 17 on 2026-10-11 (see its "Plan change").

## Goal

Pushing a version tag produces a GitHub Release with everything a user installs, built and tested by CI.

## Tasks

- [x] A versioning policy (semver for the APIs, the on-disk formats and the wire contract; what 1.x promises), in `releasing.md` or an ADR.
- [x] `release.yml`: on a `v*` tag, build and test the Python wheels (embedded and client) and the sdist, the `iwdb-server` and `iwctl` binaries for Linux, macOS and Windows, and the multi-arch Docker image (linux/amd64 and arm64, from the `Dockerfile` of step 13a) saved as a file; attach them with checksums to a GitHub Release whose notes are the CHANGELOG's section. Remove the PyPI publish job.
- [x] The tag must match the workspace version, and its release fixture (17a) must exist, or the workflow fails before building.
- [x] `releasing.md`: the steps, from the CHANGELOG and the release fixture to the tag and the checks after it.

## Acceptance criteria

- A rehearsal tag (a pre-release, e.g. `v1.0.0-rc.1`) produces a complete GitHub Release, and its wheel and image work on a clean machine.

## Non-goals

- crates.io, PyPI, Docker registries and a Helm chart (step 17's Plan change).

## Outcome

Done on 2026-10-12, except what only the owner can do: push the rehearsal tag.

- **Versioning policy: [ADR 0060](../adr/0060-versioning-policy.md).** Semver, with one version for everything (crates, binaries, wheel, image, tag). It says what 1.x promises, and what breaks it, for the Rust API (the `iwdb` crate and the Rust clients; the other crates are internal), the Python API, the on-disk formats (every 1.x opens what every earlier release wrote, through the release fixtures, which is stronger than N-1), the wire contract (`v1` changes only compatibly; `buf breaking`), error codes, configuration keys, metric names, `iwctl` and the image. Pre-releases are `-alpha.N`, `-beta.N` and `-rc.N` only, because those have a PEP 440 spelling (`1.0.0rc1`; maturin's conversion, checked locally for all three). They are tagged on a `release/X.Y.Z-rc.N` branch that isn't merged, so `main` keeps the fixtures of final releases only. A tag is never moved or reused.
- **The gate** is `scripts/release-gate.sh`, a POSIX shell script with no toolchain. It checks the version's form, `Cargo.toml`, `Cargo.lock` (a version bump without the lock file would otherwise fail later, in every `--locked` build), the fixture's `expected.txt` and a non-empty CHANGELOG section. It reports every failure, not just the first. `scripts/release-gate-test.sh` tests it, and the notes extractor `scripts/release-notes.sh`, on small trees: 21 cases, run by CI (`ci.yml#release-gate`) and by the release workflow before its gate. Against this repository, a fake version (`v1.0.0-rc.1`) fails on the version, the lock file, the fixture and the CHANGELOG; `v0.1.0` fails on the missing fixture and CHANGELOG section.
- **[release.yml](../../.github/workflows/release.yml)**: gate, then wheels, sdist, binaries and image in parallel, then *assemble* (exactly 13 files for this version, `SHA256SUMS`, the notes), then *publish*. Only *publish* has `contents: write`, and it runs only on a tag. It creates the release as a draft, uploads, then publishes it, so a failed upload leaves nothing public. A pre-release is marked as one. No secret but `GITHUB_TOKEN`, and no cache in any job: what is released is built from the tagged tree alone. `workflow_dispatch` runs everything but *publish* for the workspace version: the gate only warns, the notes are `[Unreleased]`, and the files are the run's `release` artifact.
- **Deviations, and why:**
  - **One image file per architecture, not one multi-arch file.** `docker load` loads one platform. And building arm64 under QEMU on an amd64 runner would take hours for a Rust release build. So each architecture is built with buildx on its own runner (`ubuntu-latest`, `ubuntu-24.04-arm`), smoke-tested, saved with `docker save | gzip`, and loaded back to check the file.
  - **The Linux binaries are built in `manylinux_2_28` containers.** Built on the runner, they would need glibc 2.39, which rules out Debian 12 and RHEL 9. Built in the container, they need glibc 2.28, and the smoke test runs on that glibc.
  - **The binaries have every feature** (`rest postgres console otel`), as the image has; there is no gRPC-only archive. `docker build --build-arg FEATURES=""` and `cargo build` still make one.
  - **CI's docker smoke test moved to `docker/smoke.sh`**, so the release runs the same checks on the image it ships, plus `--version` against the tag. On the way: `! curl ...` never failed under `set -e`, so the "plaintext gets no answer" check couldn't fail. It is now an explicit `if`.
  - **The notes** are the CHANGELOG section plus one line on how to install. Notes over 120,000 characters are cut, with a link to the full section (GitHub keeps 125,000; `[Unreleased]` is 45,000 today).
  - The wheels' test also checks `iwdb.__version__` (semver) and the package metadata's version (PEP 440) against the tag.
- **No Rust or Python change**: both binaries already printed `--version`.
- **Docs**: `releasing.md` is the procedure (checks, upstream check, release commit through a PR, tag, checks after, what to do when something fails, the exact commands for the `v1.0.0-rc.1` rehearsal). The README, `operations.md` ("Install", now with "From a release"), the wheel's README, the design doc and `guarantees.md` no longer point at PyPI. `python-api.md` and `console/README.md` didn't mention PyPI.

**Acceptance criterion**: "a rehearsal tag produces a complete GitHub Release, and its wheel and image work on a clean machine". The pipeline's part is shown by a `workflow_dispatch` run on this branch, where every build and test job passed: [run 38288741834](https://github.com/p-sodmann/ironweaver-db/actions/runs/38288741834), for 0.1.0. Its `release` artifact has the 13 files and `SHA256SUMS`, all of which verify. The Linux binaries need at most `GLIBC_2.28`. The macOS arm64 archive and wheel, installed from the downloaded files, ran (`--version`, a store opened and written). Two earlier runs failed in the workflow itself, and both are fixed: the build records that build-push-action uploads as artifacts broke the merged download, and `ldd --version | head -1` got SIGPIPE under `pipefail`. Pushing `v1.0.0-rc.1` publishes, so the owner pushes it, with the commands in [releasing.md](../releasing.md#pre-releases-and-the-rehearsal-tag-for-10). Checking the wheel and the image on a clean machine is also the owner's, with the checks in [After the release](../releasing.md#after-the-release).

## Follow-ups

- **The owner**: push the rehearsal tag `v1.0.0-rc.1` and check its release on a clean machine (above). Optionally, protect `v*` tags with a ruleset ([releasing.md](../releasing.md#once-protect-the-tags-recommended)).
- The binaries and the image aren't signed, and there is no SBOM or build provenance. `actions/attest-build-provenance` would add provenance without a registry. Left out because nobody asked for it, and it needs `id-token: write`.
- The Linux binaries are glibc builds; there is no static musl build. The image covers older or other distributions.
- For 17f: set the version to 1.0.0, move `[Unreleased]` to `## [1.0.0]`, write the fixture, run the gate, merge, then tag (releasing.md, steps 1 to 6). The workflow needs no change.
