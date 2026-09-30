# Step 17: Release 1.0

Status: todo
Milestone: M4 Production 1.0
Depends on: steps 14, 15, 16

## Goal

Ship 1.0 with documented guarantees and a full verification suite.

## Tasks

- [ ] First: run the [upstream check](upstream-check.md). Issues this step depends on: all open ones (the release notes list any that remain, with their workarounds).
- [ ] Compatibility fixtures: data directories (WAL + checkpoints) written by every released version open in the new one.
- [ ] Continuous fuzzing (nightly or OSS-Fuzz): WAL reader, checkpoint loader, proto and JSON decoding, pattern and filter inputs.
- [ ] 24h soak test with mixed load; runbook for running it before each minor release.
- [ ] Docs site: concepts, guarantees (`documentation/guarantees.md`: durability per fsync policy, isolation, limits, failure behaviour), operations, gRPC and REST reference.
- [ ] Release pipeline: semver, crates.io, PyPI wheels (embedded and client), multi-arch Docker image, optional Helm chart, CHANGELOG.

## Acceptance criteria

- Crash, soak, fuzz and compatibility suites green.
- 1.0 released. M4 is done: update [README.md](README.md).
