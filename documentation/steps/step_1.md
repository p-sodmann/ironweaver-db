# Step 1: Bootstrap on `ironweaver-core`

Status: todo
Milestone: M0 Foundation
Depends on: nothing

## Goal

An empty but complete Rust workspace with CI and project hygiene, depending on a pinned `ironweaver-core`, with a smoke test that proves the core does what the review says. The upstream change requests are filed. Every later step only adds code and never has to fix scaffolding.

## Tasks

### Repository and licensing
- [ ] `git init`, `.gitignore` (Rust `target/`, Python `__pycache__/`, `.venv/`, `*.so`, `dist/`, editor files, `.DS_Store`).
- [ ] `LICENSE` (MIT, matching Ironweaver). Update the license line in `README.md`.
- [ ] `CHANGELOG.md` (Keep a Changelog, `## [Unreleased]`).
- [ ] `CONTRIBUTING.md`: short, points to `AGENTS.md` and the step workflow.
- [ ] `documentation/adr/0001-record-architecture-decisions.md` with the ADR template.

### Workspace
- [ ] Root `Cargo.toml` as a virtual workspace (`resolver = "2"`), shared `[workspace.package]` (version `0.0.0`, edition 2021, license MIT, `rust-version = "1.85"` to match the core) and `[workspace.dependencies]` (`ironweaver-core`, `thiserror`, `proptest`, `criterion`).
- [ ] `rust-toolchain.toml` (stable, with `rustfmt` and `clippy`); `rustfmt.toml` with `max_width = 120` (same as Ironweaver).
- [ ] `[workspace.lints]`: `unsafe_code = "forbid"` for now, clippy `unwrap_used` / `expect_used` warned in library code.
- [ ] Create only `crates/iwdb-engine` (lib) for now. Other crates are created by the steps that need them.

### `ironweaver-core` dependency
- [ ] Depend on `ironweaver-core` via `git = "https://github.com/p-sodmann/Ironweaver"` with a pinned `rev` (at least `02cefab`), switching to the crates.io release once 0.2.0 is published. Record the choice and bump policy in `documentation/adr/0002-ironweaver-core-dependency.md`.
- [ ] Smoke tests in `crates/iwdb-engine/tests/core_smoke.rs` that confirm the assumptions in [ironweaver-core-review.md](../ironweaver-core-review.md):
  - `Graph<Record, Record>` builds without Python linked;
  - a batch of `Op`s round-trips through serde (postcard and JSON) and replays onto a second graph with identical edge ids;
  - a failing `apply_all` leaves the graph unchanged;
  - a graph with `meta["iwdb.seq"]` saves with `write_atomic` + binary format and loads back with the meta intact;
  - `create_index` + `find_nodes` + `index_candidates(&Expr)` work;
  - `cancel::run` with a token cancelled from another thread stops a `pagerank` on a large random `Projection`;
  - `Graph<Record, Record>` is `Send + Sync` (static assertion).
- [ ] Canonical-state helper (`fn canonical(&Graph) -> Vec<String>`, order-independent) in a `testutil` module; later steps use it to compare graphs.

### Upstream issues
- [ ] File one issue in the Ironweaver repo per item in "Recommended upstream changes" of the review (budgets, serde for `Expr`, streaming loader, deterministic save order, incremental memory counter, no panic in rollback, optional index persistence). Link them from the review doc.

### CI
- [ ] GitHub Actions `ci.yml` on push and PR: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace` on `ubuntu-latest` and `macos-latest`, with `Swatinem/rust-cache`.
- [ ] `cargo deny` job (licenses, advisories, duplicates) with `deny.toml`.
- [ ] MSRV job building with Rust 1.85.

### Docs
- [ ] Set this step to `done` and update [README.md](README.md).

## Acceptance criteria

- A fresh clone passes `fmt`, `clippy -D warnings` and `test --workspace` locally and on CI (Linux, macOS).
- All smoke tests pass against the pinned revision, without Python.
- ADR 0001 and 0002, `LICENSE`, `CHANGELOG.md`, `CONTRIBUTING.md` exist.
- Upstream issues are filed and linked.

## Non-goals

- No DB payload, commit pipeline, storage or server code.
- No changes in the Ironweaver repo beyond filing issues (PRs there are separate work).
