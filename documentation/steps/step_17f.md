# Step 17f: 1.0

Status: done
Milestone: M4 Production 1.0
Depends on: steps 15d, 17a, 17d and 17e (17b and 17c were dropped)

Split out of step 17 on 2026-10-11 (see its "Plan change").

## Goal

Ironweaver DB 1.0 is released on GitHub.

## Tasks

- [x] Run the upstream check; the release notes list any open issue with its workaround.
- [x] The suites are green on the release commit: CI (with its short fuzz run) and the nightly crash suite.
- [x] Set the workspace version to 1.0.0, move the CHANGELOG's `[Unreleased]` section under 1.0.0, and write and commit the release fixture `tests/fixtures/releases/1.0.0/` (17a).
- [x] The owner tags `v1.0.0`; the release workflow (17e) publishes the GitHub Release. The procedure is [releasing.md](../releasing.md) (steps 1 to 6); run `sh scripts/release-gate.sh v1.0.0` before tagging.
- [x] M4 is done: update [README.md](README.md) and step 17.

## Acceptance criteria

- The GitHub Release for `v1.0.0` exists with every file 17e lists, and its fixture is in the repository.

## Outcome

1.0.0 was released on 2026-10-14: [GitHub Release `v1.0.0`](https://github.com/p-sodmann/ironweaver-db/releases/tag/v1.0.0), tagged on `fb03f4d`, the merge of the release commit ([PR #37](https://github.com/p-sodmann/ironweaver-db/pull/37)).

- **Upstream check (2026-10-14):** no issue of ours is open. `ironweaver-core` `main` was 8 commits past our pin `af3c441` (GitHub Actions bumps and #112, an opt-in seeded node iteration), with nothing to adopt, so there was no bump. The notes have no known issues.
- **Suites:** CI was green on `main` before the merge, and on the release PR (63 checks, including the 60 s fuzz run). The nightly crash suite was green on 2026-10-14. The local `cargo test --workspace --all-features` passed.
- **Rehearsal:** the `v1.0.0-rc.1` pre-release (step 17e) was published from the same pipeline earlier that day, with all 14 files.
- **Checked after publishing:** 14 files, marked *Latest* and not a pre-release. On macOS arm64, the wheel and the binary archive match `SHA256SUMS`. The wheel installs, reports `1.0.0`, and opens and writes a store. `iwdb-server --version` prints `1.0.0 (grpc, rest, postgres, console, otel)` and `iwctl --version` prints `1.0.0`. The Linux, Windows and image files are checked by the workflow's own jobs.
- **Fixture:** `crates/iwdb/tests/fixtures/releases/1.0.0/` is in the repository. Every later version is checked against it.

## Non-goals

- Publishing anywhere but GitHub (step 17's Plan change).
