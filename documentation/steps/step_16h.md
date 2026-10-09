# Step 16h: Windows

Status: done
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

## Outcome

- [ADR 0058](../adr/0058-windows.md). Windows is supported on NTFS (and ReFS) with the guarantees of guarantees.md; its "Platforms" section lists what differs.
- **Directory fsync**: `FlushFileBuffers` on a directory opened with `FILE_FLAG_BACKUP_SEMANTICS` and write access, through std (a constant, no unsafe code, no dependency). Its error fails the operation as on Unix. The core's `write_atomic` skips it on Windows: upstream [#71](https://github.com/p-sodmann/Ironweaver/issues/71) (draft 27, pinned in `write_atomic_does_not_sync_the_directory_on_windows`); `StdFs::write_atomic` syncs the parent itself until it is fixed (`write_atomic_reports_a_failed_directory_sync_on_windows`). The real call succeeds on the runner's NTFS (`directories_are_synced_for_real`). On an SMB share (`\\localhost\C$`) the runner showed the flush failing with `ERROR_INVALID_FUNCTION`, so a store there refuses to open (`data_directories_at_unusual_paths`).
- **Open files**: std opens everything with `FILE_SHARE_DELETE` and renames and deletes with POSIX semantics where needed, so nothing changed: the checkpointer removes a segment and a checkpoint that readers hold, and they read to the end (`files_that_readers_hold_open_are_removed_and_the_readers_finish`); `write_atomic` replaces a file a reader holds (`directories_are_synced_for_real`).
- **The lock**: std's `LockFileEx`; a second process is refused and a killed one releases it (`the_lock_is_released_when_the_process_dies`, unchanged, now on Windows); the spawn race of step 7 doesn't exist there (its test runs on Windows too). The 80 ms retry was enough on the runner. Locks are mandatory: a reader of every file of a live data directory can't read `LOCK` (the tests' snapshots read its length).
- **Crash harness and failpoints**: the same code; the kill is `Child::kill` (`TerminateProcess`), an abort is exit code `0xC0000409`, failpoint paths match with `/`, a full disk is `ERROR_DISK_FULL`. The runner found one harness bug: the OS-crash simulation compared the sync log's paths as strings, which differ in separators on Windows. The short run (150 cycles × 3 policies) takes 42–49 s on windows-latest (Ubuntu 30 s, macOS 87 s). The benchmark gate wasn't run on Windows (non-goal).
- **Server**: Ctrl-C, Ctrl-Break, console close and system shutdown drain gracefully (`serves_its_data_directory_and_shuts_down_gracefully` sends Ctrl-Break through PowerShell's `GenerateConsoleCtrlEvent` and checks the final checkpoint); no certificate reload (the SIGHUP tests are Unix only, with the reason); audit files get the inherited ACL; disk free space from `fs4` (`GetDiskFreeSpaceExW`); `iwctl`'s prompt from `rpassword`.
- **Paths**: spaces, a path over 300 characters, a verbatim `\\?\` path and a share (`data_directories_at_unusual_paths`). A backup's manifest paths are joined one component at a time (a verbatim root doesn't read `/`). A Windows checkout gets the protos without the symlink and LF text files (`.gitattributes`).
- **Tests that stay Unix only**, each with its reason: `write_atomic_reports_a_failed_directory_sync` (mode bits; the Windows counterpart uses `icacls`), the audit file mode check, `sighup_reloads_the_certificate` and the reload part of `no_secret_reaches_the_logs` (no SIGHUP).
- **CI**: windows-latest runs fmt, clippy and the workspace tests (now `--no-fail-fast`), the short crash run, and the wheel with pytest on Python 3.9 and 3.13 including the remote half against a Windows-built server; the nightly crash run and the release workflow include Windows. PGlite isn't started on Windows, so the Postgres tests skip there.
- New dependencies, both Windows only: `fs4` (`iwdb`, disk free space without unsafe code) and `rpassword` (`iwctl`, the password prompt without echo).
- Acceptance criterion: the crash suite passes on windows-latest (`crash harness, short (windows-latest)`, 450 cycles, in every CI run since the sync-log fix), and guarantees.md lists Windows as supported.
