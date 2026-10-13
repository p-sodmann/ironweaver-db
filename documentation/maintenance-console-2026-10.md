# Maintenance round: console reliability (step 16a follow-up)

Each task has one implementing subagent. Preserve existing contracts, avoid new dependencies, and do not modify other tasks’ owned source files. Add tests in a uniquely named console/test/maintenance-<task>.test.cjs file. Do not commit. Report changes, validation and limitations. The coordinator runs combined checks and rebuilds console assets.

## 1. Numeric value precision

- [x] Implemented; focused regressions pass

**Owned source:** `console/src/shared.js`

**Prompt:** In `/workspace/ironweaver-db`, read `AGENTS.md`, inspect the source and applicable contract documentation, then implement this focused maintenance fix: Prevent silent rounding of unsafe integer edits and non-finite float edits. Preserve exact i64 integer input via a representation supported by the REST/core value contract, or reject it clearly if the contract cannot carry it. Cover nested JSON numeric values where feasible; do not turn invalid numeric edits silently into strings. Inspect existing callers and surface actionable errors. Follow the shared instructions above.

## 2. Query limit validation

- [x] Implemented; focused regressions pass

**Owned source:** `console/src/query.js`

**Prompt:** In `/workspace/ironweaver-db`, read `AGENTS.md`, inspect the source and applicable contract documentation, then implement this focused maintenance fix: Reject query limits that are not positive safe integers rather than rounding or accepting Infinity. Preserve off and valid limits. Add focused boundary regressions. Follow the shared instructions above.

## 3. Explorer empty namespaces

- [x] Implemented; focused regressions pass

**Owned source:** `console/src/explorer.js`

**Prompt:** In `/workspace/ironweaver-db`, read `AGENTS.md`, inspect the source and applicable contract documentation, then implement this focused maintenance fix: Handle an empty visible namespace list without dereferencing undefined or issuing namespace reads. Handle initial namespace-list failures visibly and prevent obsolete initial/poll responses updating state after unmount. Add a meaningful regression using the existing test harness or a standalone new Node test. Follow the shared instructions above.

## 4. Status polling races

- [x] Implemented; focused regressions pass

**Owned source:** `console/src/status.js`

**Prompt:** In `/workspace/ironweaver-db`, read `AGENTS.md`, inspect the source and applicable contract documentation, then implement this focused maintenance fix: Prevent overlapping server polls within the status effect and ignore responses from retired effects after pause/resume or unmount. Preserve immediate reads, cadence and log-kind handling. Add deferred-response regression coverage. Follow the shared instructions above.

## 5. REST response validation

- [x] Implemented; focused regressions pass

**Owned source:** `console/src/rest.js`

**Prompt:** In `/workspace/ironweaver-db`, read `AGENTS.md`, inspect the source and applicable contract documentation, then implement this focused maintenance fix: Treat malformed JSON on HTTP success as a typed SourceError with a useful message instead of returning null and causing downstream TypeErrors. Normalize response-body transport failures consistently with fetch failures. Preserve empty-success and HTTP error/session behavior. Add focused fetch-stub regressions. Follow the shared instructions above.

## Validation

`npm test` now discovers every `test/*.test.cjs` regression file. All six Node test files pass. `npm run check`, JavaScript syntax checks, `git diff --check`, and `cargo fmt --all -- --check` pass. Workspace Clippy with all targets/features and warnings denied also passes. The full `cargo test --workspace --all-features` suite passes. In this environment it requires `LIBRARY_PATH` and `LD_LIBRARY_PATH` pointing to the installed Python shared-library directory, plus local-socket permission for server integration tests. The console Python proxy tests were not run because pytest is unavailable; no Python proxy code changed.
