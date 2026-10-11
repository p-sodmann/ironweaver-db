# Step 17a: Upstream check and release fixtures

Status: done
Milestone: M4 Production 1.0
Depends on: step 16

Split out of step 17 on 2026-10-11 (see its "Plan change").

## Goal

Every release leaves a fixture of what it wrote, and every later version is tested against all of them.

## Tasks

- [x] Run the [upstream check](upstream-check.md).
- [x] `crates/iwdb/tests/release_fixture.rs`: a generator that writes `tests/fixtures/releases/<version>/` with the workspace version, and a test that opens every release fixture there. A fixture holds a data directory (checkpoints and a WAL tail in every namespace: keyed commits, a constraint, an index, marks, a namespace without a checkpoint, an imported one, a dropped one, and the system namespace's users, grants and token), a backup of it, and its WAL archive, with the expected state as text.
- [x] The check: the store opens with the expected namespaces, keys, marks and users, and goes on (a retried keyed commit, a new commit, the constraint, the token, a login, a grant, a clean close and `verify`); the backup verifies and restores the same state; the archive verifies and restores the state it reached. The fixture's files are unchanged.
- [x] The current tree writes a release fixture to a temporary directory and checks it the same way, so the generator can't rot between releases.

## Acceptance criteria

- `cargo test -p iwdb --test release_fixture` runs the check over every directory in `tests/fixtures/releases/` and over a fixture the current tree writes.
- The way to write a release's fixture is documented in the test's module comment (17e puts it into the release steps).

## Non-goals

- A fixture for an old version: no version has been released yet, so 1.0's is the first (17f writes it with the release commit).
- Replacing the format fixtures: `data-dir-v*`, `backup-v*`, `archive-v*`, `auth-v*` and `wal-v*` pin each format version on its own, as design rule 4 asks. A release fixture pins what a release actually wrote, all formats together.

## Outcome

Done on 2026-10-11.

- **Upstream check:** core `e088ae6` (bumped in PR #24); upstream `main` is at `603ac59`, which changes only the Python bindings, so no bump. No issue of ours is open.
- **The archive ends before the store does.** The archive gets a segment once it is sealed, and the fixture keeps a WAL tail in every namespace on purpose, so the archive doesn't reach the last records. The generator restores from the archive right after writing it and saves that state as `archive.txt`; later versions must restore exactly that.
- **Expected state comes from the store**, not from a reference namespace: the fixture records what that version read back, and later versions must read the same.
- Nothing is in `tests/fixtures/releases/` yet: `every_release_fixture_opens_in_this_version` passes trivially until 1.0's fixture lands, and `this_version_writes_a_release_fixture_that_opens` carries the check meanwhile.
