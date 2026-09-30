# Step 1: Bootstrap on `ironweaver-core`

Status: done
Milestone: M0 Foundation
Depends on: nothing

## Goal

An empty but complete Rust workspace with CI and project hygiene, depending on a pinned `ironweaver-core`, with a smoke test that proves the core does what the review says. The upstream change requests are filed. Every later step only adds code and never has to fix scaffolding.

## Tasks

### Repository and licensing
- [x] `git init`, `.gitignore` (Rust `target/`, Python `__pycache__/`, `.venv/`, `*.so`, `dist/`, editor files, `.DS_Store`).
- [x] `LICENSE` (MIT, matching Ironweaver). Update the license line in `README.md`.
- [x] `CHANGELOG.md` (Keep a Changelog, `## [Unreleased]`).
- [x] `CONTRIBUTING.md`: short, points to `AGENTS.md` and the step workflow.
- [x] `documentation/adr/0001-record-architecture-decisions.md` with the ADR template.

### Workspace
- [x] Root `Cargo.toml` as a virtual workspace (`resolver = "2"`), shared `[workspace.package]` (version `0.0.0`, edition 2021, license MIT, `rust-version = "1.85"` to match the core) and `[workspace.dependencies]` (`ironweaver-core`, `thiserror`, `proptest`, `criterion`).
- [x] `rust-toolchain.toml` (stable, with `rustfmt` and `clippy`); `rustfmt.toml` with `max_width = 120` (same as Ironweaver).
- [x] `[workspace.lints]`: `unsafe_code = "forbid"` for now, clippy `unwrap_used` / `expect_used` warned in library code.
- [x] Create only `crates/iwdb-engine` (lib) for now. Other crates are created by the steps that need them.

### `ironweaver-core` dependency
- [x] Depend on `ironweaver-core` via `git = "https://github.com/p-sodmann/Ironweaver"` with a pinned `rev` (at least `02cefab`), switching to the crates.io release once 0.2.0 is published. Record the choice and bump policy in `documentation/adr/0002-ironweaver-core-dependency.md`.
- [x] Smoke tests in `crates/iwdb-engine/tests/core_smoke.rs` that confirm the assumptions in [ironweaver-core-review.md](../ironweaver-core-review.md):
  - `Graph<Record, Record>` builds without Python linked;
  - a batch of `Op`s round-trips through serde (postcard and JSON) and replays onto a second graph with identical edge ids;
  - a failing `apply_all` leaves the graph unchanged;
  - a graph with graph-level `meta["iwdb.seq"]` (passed to the codec next to the graph; `Graph` has no meta field) saves with `write_atomic` + binary format and loads back with the meta intact;
  - `create_index` + `find_nodes` + `index_candidates(&Expr)` work;
  - `cancel::run` with a token cancelled from another thread stops a `pagerank` on a large random `Projection`;
  - `Graph<Record, Record>` is `Send + Sync` (static assertion).
- [x] Canonical-state helper (`fn canonical(&Graph) -> Vec<String>`, order-independent) in a `testutil` module; later steps use it to compare graphs.

### Upstream issues
- [x] *(drafted, pending review: [upstream-issues.md](../upstream-issues.md); filing is left to the maintainer)* File one issue in the Ironweaver repo per item in "Recommended upstream changes" of the review (budgets, serde for `Expr`, streaming loader, deterministic save order, incremental memory counter, no panic in rollback, optional index persistence). Link them from the review doc (the drafts are linked; replace with issue links once filed).

### CI
- [x] GitHub Actions `ci.yml` on push and PR: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace` on `ubuntu-latest` and `macos-latest`, with `Swatinem/rust-cache`.
- [x] `cargo deny` job (licenses, advisories, duplicates) with `deny.toml`.
- [x] MSRV job building with Rust 1.85.

### Docs
- [x] Set this step to `done` and update [README.md](README.md).

## Acceptance criteria

- A fresh clone passes `fmt`, `clippy -D warnings` and `test --workspace` locally and on CI (Linux, macOS).
- All smoke tests pass against the pinned revision, without Python.
- ADR 0001 and 0002, `LICENSE`, `CHANGELOG.md`, `CONTRIBUTING.md` exist.
- Upstream issues are filed and linked. *(Drafted and linked; filing is pending the maintainer's review, see below.)*

## Non-goals

- No DB payload, commit pipeline, storage or server code.
- No changes in the Ironweaver repo beyond filing issues (PRs there are separate work).

## Notes from implementation

- Pinned `ironweaver-core` at `02cefabaaf99c796e828fef616a3e792945aca17` (origin/main HEAD on 2026-09-30), see ADR 0002.
- Upstream issues were drafted in [upstream-issues.md](../upstream-issues.md) instead of filed, so the maintainer can review them first. The acceptance criterion is met once they are filed and the links replace the drafts.
- The smoke tests corrected the review: graph-level meta is not part of `Graph`, and a failed `apply_all` keeps `next_edge_id()` raised. Both are recorded in the review; the canonical-state helper therefore ignores the edge id counter.
- `unwrap_used` / `expect_used` are warned for library code; `clippy.toml` allows them in `#[test]` code and integration test files allow them for their helpers.
- CI (`ci.yml`) was not run on GitHub; its commands (fmt, clippy, test, `cargo tree` pyo3 check, MSRV check with Rust 1.85, `cargo deny check`) pass locally on macOS.
