# ADR 0002: Depend on `ironweaver-core` through a pinned git revision

Status: accepted
Date: 2026-09-30

## Context

Ironweaver DB builds on `ironweaver-core` (see the [core review](../ironweaver-core-review.md)). Version 0.2.0 of the crate exists in the [Ironweaver repository](https://github.com/p-sodmann/Ironweaver) (`crates/ironweaver-core`) but is not published on crates.io yet. The database's durability depends on details of the core (op semantics, file format, edge id assignment), so an unnoticed change in the core can break recovery or compatibility.

## Decision

- The workspace depends on `ironweaver-core` through git, pinned to a full commit hash, in `[workspace.dependencies]` of the root `Cargo.toml`:

  ```toml
  ironweaver-core = { git = "https://github.com/p-sodmann/Ironweaver", rev = "02cefabaaf99c796e828fef616a3e792945aca17" }
  ```

  `02cefab` ("Merge pull request #24 from p-sodmann/refactor/rust-core-split") was `origin/main` HEAD on 2026-09-30 and is the revision the core review describes.
- Crates use `ironweaver-core.workspace = true`, so there is exactly one place to change.
- `Cargo.lock` is committed, and CI builds with `--locked`.
- `cargo deny` only allows this one git source.
- When 0.2.0 (or later) is on crates.io, we switch to the registry release with an exact version requirement (`=0.2.x`) and record that here or in a superseding ADR.

### Bump policy

A bump is a deliberate change in its own commit or PR, never part of unrelated work. It must:

1. update the `rev` (or version) and `Cargo.lock`, and state the old and new revision and the upstream changes in between;
2. pass `crates/iwdb-engine/tests/core_smoke.rs`; a failing smoke test means an assumption in the core review no longer holds, and the review is updated first;
3. once they exist, pass the crash, recovery and format-compatibility suites (steps 5–7);
4. pass `cargo deny check` and the MSRV build (the core's `rust-version` must not exceed ours);
5. update the "Reviewed" line of the core review if the review was re-checked against the new revision.

## Consequences

- Builds are reproducible and a core change can't reach us unnoticed.
- Upstream fixes (see [upstream-issues.md](../upstream-issues.md)) only arrive by an explicit bump.
- Cargo fetches the whole Ironweaver repository (Python bindings included) to build the core crate. Only `ironweaver-core` is compiled; pyo3 is not in our dependency graph, and CI checks that.
- We can't publish crates to crates.io while depending on a git revision. That is acceptable until the release step (step 17).
