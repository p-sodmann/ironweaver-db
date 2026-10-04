# Step 16h: Windows

Status: todo
Milestone: M4 Production 1.0
Depends on: step 7 (storage); independent of steps 16b to 16g, can run in parallel

Split out of step 16 on 2026-10-04 (see its "Plan change"); moved to step 16 from step 7 by [ADR 0013](../adr/0013-python-bindings.md).

## Goal

Windows is a supported platform with the same durability guarantees, tested.

## Tasks

- [ ] A directory fsync (`FILE_FLAG_BACKUP_SEMANTICS` and `FlushFileBuffers`), checked on NTFS.
- [ ] A CI job that builds and tests the workspace and the Python bindings on Windows.
- [ ] A crash-harness mode that kills with `TerminateProcess` (design rule 3).
- [ ] Windows wheels; the platform row in [guarantees.md](../guarantees.md).

## Acceptance criteria

- The crash suite passes on Windows in CI, and guarantees.md lists Windows as supported.

## Non-goals

- Windows services or installers.
