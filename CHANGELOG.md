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

### Changed
- Top-level attribute keys starting with `iwdb.` are reserved, like meta keys: commits, `DbRecord`'s serde and the codec reject them (step 3, ADR 0004).
- Upstream issue #31 filed: `Value`'s serde rejects empty containers at the depth limit that the file format accepts.
- Bumped `ironweaver-core` from `02cefab` to `a14149e` (PR #25, which implements all seven upstream drafts). Core review updated; follow-up draft 8 (edge budget) added.
