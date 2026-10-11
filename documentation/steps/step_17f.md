# Step 17f: 1.0

Status: todo
Milestone: M4 Production 1.0
Depends on: steps 15d and 17a to 17e

Split out of step 17 on 2026-10-11 (see its "Plan change").

## Goal

Ironweaver DB 1.0 is released on GitHub.

## Tasks

- [ ] Run the upstream check; the release notes list any open issue with its workaround.
- [ ] The suites are green on the release commit: CI, the nightly crash suite, a week of nightly fuzzing without a finding (17b), and a 24 h soak run (17c).
- [ ] Set the workspace version to 1.0.0, move the CHANGELOG's `[Unreleased]` section under 1.0.0, and write and commit the release fixture `tests/fixtures/releases/1.0.0/` (17a).
- [ ] The owner tags `v1.0.0`; the release workflow (17e) publishes the GitHub Release.
- [ ] M4 is done: update [README.md](README.md) and step 17.

## Acceptance criteria

- The GitHub Release for `v1.0.0` exists with every file 17e lists, and its fixture is in the repository.

## Non-goals

- Publishing anywhere but GitHub (step 17's Plan change).
