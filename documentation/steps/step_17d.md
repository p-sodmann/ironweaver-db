# Step 17d: Documentation for 1.0

Status: todo
Milestone: M4 Production 1.0
Depends on: step 17c (it documents what the suites prove)

Split out of step 17 on 2026-10-11 (see its "Plan change").

## Goal

A user can learn the concepts, the guarantees, operations and the APIs from the repository's documentation, without reading the code.

## Tasks

- [ ] `documentation/guarantees.md` as the user-facing guarantees page: durability per fsync policy, isolation, limits, failure behaviour and compatibility between versions, without the per-step history (it moves to the ADRs and step files it cites).
- [ ] A concepts page: namespaces, commits, seqs and versions, idempotency keys, marks, the change stream, projections, jobs.
- [ ] An index page that links concepts, guarantees, operations (`operations.md`), the gRPC and REST references (`api/`), the Python API and `iwctl`.
- [ ] Check that every page's examples run or are taken from the code's tables, as the operations guide does.

## Acceptance criteria

- Every guarantee on the page names the test that checks it.

## Non-goals

- A hosted docs site outside GitHub (step 17's Plan change). If a site is wanted later, it is built from these pages.
