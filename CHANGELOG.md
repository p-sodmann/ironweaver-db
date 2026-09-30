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
