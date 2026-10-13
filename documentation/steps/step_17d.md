# Step 17d: Documentation for 1.0

Status: done
Milestone: M4 Production 1.0
Depends on: step 17a (it documents what the suites prove; 17b and 17c were dropped)

Split out of step 17 on 2026-10-11 (see its "Plan change").

## Goal

A user can learn the concepts, the guarantees, operations and the APIs from the repository's documentation, without reading the code.

## Tasks

- [x] `documentation/guarantees.md` as the user-facing guarantees page: durability per fsync policy, isolation, limits, failure behaviour and compatibility between versions, without the per-step history (it moves to the ADRs and step files it cites).
- [x] A concepts page: namespaces, commits, seqs and versions, idempotency keys, marks, the change stream, projections, jobs.
- [x] An index page that links concepts, guarantees, operations (`operations.md`), the gRPC and REST references (`api/`), the Python API and `iwctl`.
- [x] Check that every page's examples run or are taken from the code's tables, as the operations guide does.

## Acceptance criteria

- Every guarantee on the page names the test that checks it.

## Non-goals

- A hosted docs site outside GitHub (step 17's Plan change). If a site is wanted later, it is built from these pages.

## Outcome

Done on 2026-10-12.

- **[guarantees.md](../guarantees.md)** is organised by topic (durability, atomicity, recovery, failures, ..., compatibility, platforms) instead of by step; step markers and history are gone, ADR links stay. Each section lists guarantees, then **Assumes:**, **Not guaranteed:** and **Notes:**. Every guarantee, bullet or table row, ends with `Tests:` (or a `Tests` column): test functions, `file::test` where a name is ambiguous, or a CI job (`ci.yml#docker`, for checks that only CI runs).
- **The acceptance criterion is a test**: `every_guarantee_names_tests_that_exist` (`crates/iwdb/tests/docs.rs`) finds every guarantee, fails if one names no test, and resolves each name against the `fn`s and `def`s of the repository and the jobs of the workflows. `the_index_concepts_and_guarantees_link_to_what_exists` checks the links and anchors of the three pages.
- **Claims corrected on the way:** the console does use `localStorage`, for its theme only (the page said "nothing"; now: no credential in browser storage). Measurements (write-lock times, index build stalls) moved out to the step files and benchmarks.md; they are numbers of one machine, not guarantees. A guarantee that no test checked became a "Not guaranteed" or a "Note" instead of staying a promise: the audit log level, the console's storage, Windows process details.
- **[concepts.md](../concepts.md)** and **[the index](../README.md)** are new; the README's documentation list starts with the index.
- **Examples checked**: `iwctl.md`'s examples parse with `args::parse` (`every_iwctl_example_in_the_reference_parses`, `guide.rs`); the TOML blocks of `grpc.md` and `projections.md` pass `Config::parse` (`every_toml_block_of_the_api_references_is_a_valid_configuration`); the Python blocks of the README, `python-api.md` and `concepts.md` run in order (`test_docs.py`), and blocks that need a server are compiled.

## Follow-ups

- **Not checked:** the `curl` examples of `rest.md`, `changes.md` and `grpc.md`, and the Rust snippets of `grpc.md` and `projections.md`. They need a running server or a whole program; the routes they use are checked against the served OpenAPI document (`rest_md_lists_every_route`), their bodies aren't.
- The console's own claim (no credential in browser storage) has no test in `console/test/`.
