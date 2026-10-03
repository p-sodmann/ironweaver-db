# Step 11a: Maintenance round

Status: in progress
Milestone: M3 Network access
Depends on: step 11

## Goal

Make the code easier to maintain before step 12 adds REST: smaller files, one copy of each test helper, fewer overlapping tests and shorter doc comments. Nothing a user can observe changes.

## Tasks

### Tests
- [ ] `iwdb-engine/tests/core_smoke.rs`: remove the smoke tests that later tests cover (streaming loader, deterministic saves, meta via `write_atomic`, property index lookups, pagerank cancel, op serde replay); move the three unique index asserts into `db_graph.rs`; point the file header and the core review at the tests that now check each claim.
- [ ] Engine: remove `commit.rs::committed_graphs_save_and_load_with_their_versions`; fold `db_graph.rs::saving_twice_gives_identical_bytes` into `saves_are_deterministic`.
- [ ] `iwdb/tests/query.rs`: remove `closing_waits_for_requests_and_releases_the_directory` (covered by `iwdb-server/tests/shutdown.rs`); `grpc.rs`: drop the `not_found` half of `errors_carry_their_code_in_the_trailer` (covered by `status.rs` and the conformance suite).
- [ ] Python: test the binding's translation, not the engine again (`test_values.py`, `test_step8.py`, `test_store.py`).

### Test helpers
- [ ] One observable-state helper in `iwdb_engine::testutil`, one default-namespace path helper, one `StoreOptions` test preset; remove the copies.

### Code
- [ ] Split `iwdb/src/store.rs` (`ns.rs`, `background.rs`), `iwdb-server/src/convert.rs` (a `convert/` module), `tests/crash/src/harness.rs` (`plan.rs`, `check.rs`, `restore.rs`) and `iwdb-python/src/store.rs` (`transaction.rs`, no duplicated namespace methods).
- [ ] `iwdb-storage`: one set of fs helpers in `io.rs`, one `copy_file` with CRC, `manifest.rs`, `marker.rs`, `lock.rs`; failpoint enums from one macro.
- [ ] Remove dead code (`iwdb_engine::catalog::Catalog`, conversion aliases).

### Docs
- [ ] Doc comments: no step history, no text copied from `documentation/`, no one-liners that restate a name. Every stated guarantee stays.

## Acceptance criteria

- `cargo fmt`, `clippy -D warnings`, `cargo test --workspace`, `cargo doc` (no broken links) and `pytest` pass.
- No change under `proto/`, `documentation/formats/` or the compatibility fixtures; failpoint names unchanged.
- The tests kept by AGENTS.md rule 3 (crash, fault, fixture, upstream pins, acceptance tests named in step files) all remain.
- Line counts before and after recorded below.

## Non-goals

- Behaviour, format, proto or public-API changes. New features.

## Result

(filled in at the end)
