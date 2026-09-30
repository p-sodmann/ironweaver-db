# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Cargo workspace with the `iwdb-engine` crate, pinned `ironweaver-core` dependency and smoke tests for the core's guarantees (step 1).
- Order-independent canonical-state helper for comparing graphs in tests.
- CI (fmt, clippy, tests on Linux and macOS, MSRV 1.85, `cargo deny`).
- Project hygiene: MIT license, contributing guide, ADRs 0001 and 0002.
- `DbRecord` payload (attributes, user meta, version) with the core's attribute path rules, and `DbGraph` (step 2).
- Reserved-name policy: meta keys starting with `iwdb.` belong to the database (`iwdb.version`, `iwdb.seq`, `iwdb.catalog`).
- Catalog model: namespaces, property index definitions and unique / required constraints per label and attribute path, validated, with serde.
- Deterministic saving and loading of `DbGraph`s in the core's binary and JSON formats, with the version in entity meta and the catalog in graph meta (ADR 0003); streaming binary load; the catalog decides which indexes exist after loading.
- `Canonical` for `DbRecord`, including the version.
- Smoke tests for the core features from PR #25: deterministic saves, index definitions in files, streaming loader, `memory_usage` without payloads, `Expr` / `Pattern` round trips, and the known visit-budget gap.
- In-memory commit pipeline (step 3): the `Mutation` write vocabulary, `Namespace` with `prepare` / `apply` / `commit`, catalog changes validated against the data, `replay` of commit records, and a poisoned state when applying fails. Resolved ops use explicit edge ids and carry versions. Unique and required constraints are checked on the state after the whole transaction. `CommitRecord` has serde for the WAL.
- Version semantics: new entities start at 1, each commit that writes an entity adds 1, `expected_version: 0` means "must not exist", and versions never go above `i64::MAX`.
- ADR 0004: versions are set by an attribute op on the reserved key `iwdb.version`.
- Model-based proptest of the commit pipeline, and the replay property (replaying the commit records gives the same canonical state).
- `iwdb-storage` crate with the write-ahead log (step 4): a versioned segment format with CRC32C per header and record (`documentation/formats/wal.md`), rotation by size, fsync policies `always`, `group` (by count and age) and `off`, a read-only state after any write or fsync failure (never retried), and a reader that reports a torn tail and rejects corruption, gaps and repeats.
- `LoggedNamespace`: commits are logged, and fsynced per the policy, before they are applied and acknowledged.
- ADR 0005 (fsync policies and failure behaviour) and `documentation/guarantees.md`.
- WAL tests: random workloads reread and replayed, fault injection through the `LogFs` seam, truncation at every offset and every bit flip, and a format-1 compatibility fixture. Also a cargo-fuzz target for the reader (`fuzz/`, nightly, run in CI).

- `iwdb` crate, the embedded store (step 5): `Store::open` / `open_with`, `commit`, `commit_catalog`, `node`, `edge`, `catalog`, `seq`, `synced_seq`, `read`, `sync`, `checkpoint`, `close`; a read-only state after WAL or apply failures; a background checkpointer (WAL size and time triggers) and the group commit timer.
- Data directory layout 1 (`documentation/formats/data-dir.md`): a versioned marker, an exclusive lock (`flock` via `fs4`) that a second open in any process runs into, `checkpoints/` and `wal/`.
- Checkpoints: binary graph files named by seq, with `iwdb.seq` and `iwdb.catalog` in graph meta, covering only synced commits, written with `write_atomic` plus a checked directory sync. The checkpointer replays the WAL into its own namespace, so commits don't wait for it; it keeps 2 checkpoints by default and deletes the WAL segments they no longer need.
- Recovery: stale temp files removed, the newest checkpoint that loads (fallback to older ones), WAL replay, torn tail cut; typed errors that leave the files unchanged for corruption, missing WAL, and records that fail to replay. A `RecoveryReport` and log messages say what it did.
- `WalReader::open_until` (read up to a seq), `Wal::appended_bytes`, and `write_atomic` / `remove_file` / `truncate` in the `LogFs` seam.
- ADR 0006 (checkpoints, recovery and the lock). Tests: recovery acceptance cases, lock (threads, processes, `kill -9`), checkpoint faults, a random workload with checkpoints and crashes, a layout-1 fixture, and a commit latency measurement during a checkpoint.
- Crash and fault-injection suite (step 6, ADR 0007): `iwdb_storage::failpoint::FailFs` (feature `failpoints`), failpoints on every write-side file operation that fail, report a full disk, tear a write, pause, panic or abort; a fault test for each through the store (`crates/iwdb/tests/faults.rs`); the kill -9 harness `iwdb-crash` (`tests/crash`) with an OS-crash simulation, a reference model and deterministic crash points; a short run on every PR and a nightly long run in CI.
- `iwdb_engine::testutil::workload` (feature `testutil`): the random workload strategies, a seeded fixed workload and an endless `Stream`, shared by the tests and the harness.
- ADR 0008 and `documentation/guarantees.md`, "Crashes and simulated failures": the defined behaviour of each simulated failure.

### Changed
- A panic while the store changes its namespace or WAL (a commit, an fsync, the group commit timer) aborts the process, and the next open recovers (ADR 0008). Before, the store turned read-only, and readers could have seen part of a transaction.
- `GraphMeta` carries the seq (`iwdb.seq`), required when loading a database file; `Namespace::from_loaded` builds a namespace from a loaded file.
- Upstream issue #32 filed: `write_atomic` ignores a failed directory fsync.
- The step 3 workload strategies moved to `crates/iwdb-engine/tests/workload/mod.rs`, shared with the WAL tests.
- Top-level attribute keys starting with `iwdb.` are reserved, like meta keys: commits, `DbRecord`'s serde and the codec reject them (step 3, ADR 0004).
- Upstream issue #31 filed: `Value`'s serde rejects empty containers at the depth limit that the file format accepts.
- Bumped `ironweaver-core` from `02cefab` to `a14149e` (PR #25, which implements all seven upstream drafts). Core review updated; follow-up draft 8 (edge budget) added.

### Fixed
- Under the `off` fsync policy, `Wal::sync` (and so `Store::sync`, `checkpoint` and `close`) fsynced only the current segment and no directory, and a new writer claimed the whole log synced; now a sync covers every segment with unsynced records and the directory, and a writer under `off` starts with `synced_seq` 0 (step 6).
- A panic inside a `Store::read` closure made the store read-only (step 6).
