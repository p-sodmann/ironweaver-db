# Step 17f: 1.0

Status: todo
Milestone: M4 Production 1.0
Depends on: steps 15d, 17a, 17d and 17e (17b and 17c were dropped)

Split out of step 17 on 2026-10-11 (see its "Plan change").

## Goal

Ironweaver DB 1.0 is released on GitHub.

## Tasks

- [ ] Run the upstream check; the release notes list any open issue with its workaround.
- [ ] The suites are green on the release commit: CI (with its short fuzz run) and the nightly crash suite.
- [ ] Set the workspace version to 1.0.0, move the CHANGELOG's `[Unreleased]` section under 1.0.0, and write and commit the release fixture `tests/fixtures/releases/1.0.0/` (17a).
- [ ] The owner tags `v1.0.0`; the release workflow (17e) publishes the GitHub Release. The procedure is [releasing.md](../releasing.md) (steps 1 to 6); run `sh scripts/release-gate.sh v1.0.0` before tagging.
- [ ] M4 is done: update [README.md](README.md) and step 17.

## Acceptance criteria

- The GitHub Release for `v1.0.0` exists with every file 17e lists, and its fixture is in the repository.

## Non-goals

- Publishing anywhere but GitHub (step 17's Plan change).
