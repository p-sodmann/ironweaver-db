# Step 16h: Windows

Status: in progress
Milestone: M4 Production 1.0
Depends on: step 7 (storage); independent of steps 16b to 16g, can run in parallel

Split out of step 16 on 2026-10-04 (see its "Plan change"); moved to step 16 from step 7 by [ADR 0013](../adr/0013-python-bindings.md).

Started with the [upstream check](upstream-check.md) (2026-10-09, core bumped to `9cec233`, PR #21), and started from the merged state of steps 16g and the `c69ef51` check, which also changed storage, the crash harness and CI.

## Goal

Windows is a supported platform with the same durability guarantees, tested.

## Tasks

- [x] A directory fsync (`FILE_FLAG_BACKUP_SEMANTICS` and `FlushFileBuffers`), checked on NTFS.
- [x] A CI job that builds and tests the workspace and the Python bindings on Windows.
- [x] A crash-harness mode that kills with `TerminateProcess` (design rule 3).
- [x] Windows wheels; the platform row in [guarantees.md](../guarantees.md).
- [x] ADR 0058: the directory fsync, renames and removals of open files, the lock, process death, the server's shutdown events and certificate reload, audit file permissions, disk free space, paths, Python, Docker.
- [x] The server: Ctrl-C, Ctrl-Break, console close and system shutdown drain gracefully; no certificate reload on Windows; disk free space; `iwctl`'s password prompt without echo.
- [x] Tests gated `#[cfg(unix)]` ported where they can run, with the reason where they can't.

## Acceptance criteria

- The crash suite passes on Windows in CI, and guarantees.md lists Windows as supported.

## Non-goals

- Windows services or installers.
- A Windows Docker image.
- Performance tuning for Windows.
- The operations guide (step 16i).
