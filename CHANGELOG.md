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

### Changed
- Bumped `ironweaver-core` from `02cefab` to `a14149e` (PR #25, which implements all seven upstream drafts). Core review updated; follow-up draft 8 (edge budget) added.
