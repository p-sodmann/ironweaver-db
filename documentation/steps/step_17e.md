# Step 17e: Release pipeline on GitHub

Status: todo
Milestone: M4 Production 1.0
Depends on: steps 17a and 17d

Split out of step 17 on 2026-10-11 (see its "Plan change").

## Goal

Pushing a version tag produces a GitHub Release with everything a user installs, built and tested by CI.

## Tasks

- [ ] A versioning policy (semver for the APIs, the on-disk formats and the wire contract; what 1.x promises), in `releasing.md` or an ADR.
- [ ] `release.yml`: on a `v*` tag, build and test the Python wheels (embedded and client) and the sdist, the `iwdb-server` and `iwctl` binaries for Linux, macOS and Windows, and the multi-arch Docker image (linux/amd64 and arm64, from the `Dockerfile` of step 13a) saved as a file; attach them with checksums to a GitHub Release whose notes are the CHANGELOG's section. Remove the PyPI publish job.
- [ ] The tag must match the workspace version, and its release fixture (17a) must exist, or the workflow fails before building.
- [ ] `releasing.md`: the steps, from the CHANGELOG and the release fixture to the tag and the checks after it.

## Acceptance criteria

- A rehearsal tag (a pre-release, e.g. `v1.0.0-rc.1`) produces a complete GitHub Release, and its wheel and image work on a clean machine.

## Non-goals

- crates.io, PyPI, Docker registries and a Helm chart (step 17's Plan change).
