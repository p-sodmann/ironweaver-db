# Step 11a: Maintenance round

Status: done
Milestone: M3 Network access
Depends on: step 11

## Goal

Make the code easier to maintain before step 12 adds REST: smaller files, one copy of each test helper, fewer overlapping tests and shorter doc comments. Nothing a user can observe changes.

## Tasks

### Tests
- [x] `iwdb-engine/tests/core_smoke.rs`: remove the smoke tests that later tests cover (streaming loader, deterministic saves, meta via `write_atomic`, property index lookups, pagerank cancel, op serde replay); move the three unique index asserts into `db_graph.rs`; point the file header and the core review at the tests that now check each claim.
- [x] Engine: remove `commit.rs::committed_graphs_save_and_load_with_their_versions`; fold `db_graph.rs::saving_twice_gives_identical_bytes` into `saves_are_deterministic`.
- [x] `iwdb/tests/query.rs`: remove `closing_waits_for_requests_and_releases_the_directory` (covered by `iwdb-server/tests/shutdown.rs`); `grpc.rs`: drop the `not_found` half of `errors_carry_their_code_in_the_trailer` (covered by `status.rs` and the conformance suite).
- [x] Python: test the binding's translation, not the engine again (`test_values.py`, `test_step8.py`, `test_store.py`).

### Test helpers
- [x] One observable-state helper in `iwdb_engine::testutil` (`state`, `keys`, `State`); the four copies are gone.

### Code
- [x] Split `iwdb/src/store.rs` (`store/ns.rs`, `store/background.rs`), `iwdb-server/src/convert.rs` (`convert/{entities,mutations,requests,answers,catalog}.rs`), `tests/crash/src/harness.rs` (`harness/{process,plan,check,restore}.rs`) and `iwdb-python/src/store.rs` (`namespace.rs`, `transaction.rs`).
- [x] `iwdb-storage`: one set of fs helpers in `io.rs`, one `copy_file` (with CRC) for backups and archives, `backup/manifest.rs`, `layout/{marker,lock}.rs`, `default_deref!` in `lib.rs`; `Call`, `When` and `Action` from one `named_enum!` macro (names unchanged).
- [x] Remove dead code (`iwdb_engine::catalog::Catalog`, with a note in ADR 0003; the `idempotency_key_*` conversion aliases).

### Docs
- [x] Doc comments: no step history, no text copied from `documentation/`, no one-liners that restate a name. Every stated guarantee stays.

## Acceptance criteria

- `cargo fmt`, `clippy -D warnings`, `cargo test --workspace`, `cargo doc` (no broken links) and `pytest` pass.
- No change under `proto/`, `documentation/formats/` or the compatibility fixtures; failpoint names unchanged.
- The tests kept by AGENTS.md rule 3 (crash, fault, fixture, upstream pins, acceptance tests named in step files) all remain.
- Line counts before and after recorded below.

## Non-goals

- Behaviour, format, proto or public-API changes. New features.

**Deviations from the plan**, found while doing it:

- The Python `Store` and `Namespace` still each declare the graph methods. The logic is shared already (the `*_in` helpers); removing the remaining signatures would need pyo3's `multiple-pymethods` feature (a new dependency) or a macro around a whole `#[pymethods]` block, which rustfmt can't format.
- No shared default-namespace path helper: the hard-coded `ns/00000000000000000001/...` paths in tests also check the on-disk layout (a versioned contract), so they stay explicit. No shared `StoreOptions` test preset: the presets differ per test, and one would need a test-only public API on `StoreOptions` to save a few lines. `support::workload` stays (a short name for `workload::seeded`).
- `convert.rs`'s `nodes_*` / `edges_*` / `maybe_*` helpers stay: generic replacements made the call sites longer.
- rustdoc's four warnings (two links that didn't resolve, an ambiguous `format` link, a redundant link target) predate this step; fixed, so `cargo doc` passes with `-D warnings`.
- Doc comments shrank less than hoped (about 1%): what is left after removing step history and stale notes states guarantees (durability, crash safety, errors, limits), which AGENTS.md requires. Fixture docs keep which step wrote each fixture, and pointers to later steps stay.

## Result

Lines of Rust and Python (tests included), and doc-comment lines, before (`84bdbf3`) and after:

| Crate | Lines before | after | Doc lines before | after |
|---|---|---|---|---|
| iwctl | 1208 | 1208 | 28 | 28 |
| iwdb | 7923 | 7928 | 913 | 912 |
| iwdb-engine | 7008 | 6738 | 886 | 880 |
| iwdb-python | 2746 | 2720 | 121 | 123 |
| iwdb-query | 3484 | 3484 | 553 | 553 |
| iwdb-server | 4135 | 4163 | 257 | 262 |
| iwdb-storage | 9454 | 9409 | 1737 | 1721 |
| crash harness | 3770 | 3811 | 458 | 459 |

The largest files went from 1405 (`iwdb/src/store.rs`), 1375 (`convert.rs`), 1173 (`harness.rs`) and 933 (Python `store.rs`) lines to 825, 368, 422 and 592; no source file is above 830 lines. Splits add module headers and imports, so the total drops only slightly (about 220 lines net).

**Verification** (macOS arm64): `cargo fmt --check`, `cargo clippy --workspace --all-targets` (with and without `--all-features`) `-D warnings`, `cargo test --workspace` (crash suite and the conformance suite over Embedded and gRPC included), `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`, `cargo deny check` and `pytest crates/iwdb-python/tests` (83 tests) pass. `buf` isn't installed locally; `proto/` is unchanged, so CI's `buf lint` and `buf breaking` are unaffected. `git diff 84bdbf3` is empty under `proto/`, `documentation/formats/` and the fixtures.
