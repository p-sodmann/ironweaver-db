# Step 17b: Continuous fuzzing

Status: todo
Milestone: M4 Production 1.0
Depends on: step 17a (the order only; nothing in it is needed)

Split out of step 17 on 2026-10-11 (see its "Plan change").

## Goal

Every decoder that reads untrusted or on-disk bytes is fuzzed every night, and a crash is reproducible from CI's artifacts.

## Tasks

- [ ] Fuzz targets next to `wal_reader` in `fuzz/`: the checkpoint loader, the data directory's marker and namespace log, the backup manifest, the protobuf request messages and their conversion (`iwdb_server::convert`), the REST JSON bodies, and the pattern and filter inputs (text patterns, `Expr` and `Pattern` from postcard and JSON).
- [ ] A seed corpus for each target, written from valid inputs (the format fixtures, the proto and REST test cases).
- [ ] A nightly workflow that runs every target for a fixed time, keeps the corpus between runs (cache), and uploads crashing inputs as an artifact; the PR CI keeps its short run of every target.
- [ ] Each crash found becomes a regression test next to the code.

## Acceptance criteria

- Every target runs nightly without a finding for a week before 1.0 (17f checks it).

## Non-goals

- OSS-Fuzz or another outside service (step 17's Plan change: GitHub only).
