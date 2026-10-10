# ADR 0058: Windows

Status: accepted
Date: 2026-10-09
Amends: [ADR 0005](0005-wal-fsync-and-failures.md) (the directory fsync on Windows), [ADR 0013](0013-python-bindings.md) (Windows wheels)

## Context

Step 16h makes Windows a supported platform, with the same durability guarantees, tested (design rule 3). Until now the code was Unix-only in a few places, and nothing ran on Windows:

- `io::fsync_dir` (ADR 0005) was a no-op off Unix, so after an OS crash a rotation, a checkpoint, a backup or a restore could lose a directory entry. The core's `format::write_atomic` also syncs the directory on Unix only ("not possible on Windows").
- The crash harness (ADR 0007) kills with `SIGKILL`, and checks that a failpoint's abort ended the child with `SIGABRT`.
- The server shuts down on `SIGTERM` / `SIGINT` and reloads its certificate on `SIGHUP` (ADR 0027, 0048). The audit files are created `0600` in a `0700` directory (ADR 0049). The status views read disk free space with `statvfs` (ADR 0051). `iwctl` turns the terminal's echo off with `stty`.
- CI built and tested Linux and macOS only, and shipped no Windows wheel (ADR 0013).

The workspace forbids `unsafe_code`, so no Win32 call can be made from our code directly.

What std does on Windows (Rust 1.99, `library/std/src/sys/fs/windows.rs`), which the decisions below rely on:

- Every file is opened with the share mode `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE` unless a caller sets another; we and the core never do. A handle doesn't stop another from renaming or deleting the file.
- `fs::rename` is `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`. If that is refused (`ERROR_ACCESS_DENIED`, for example the target is open), it renames through the handle with `FileRenameInfoEx` and `FILE_RENAME_FLAG_POSIX_SEMANTICS | FILE_RENAME_FLAG_REPLACE_IF_EXISTS`.
- `fs::remove_file` is `DeleteFileW`, which on NTFS uses POSIX semantics since Windows 10 1709 (the name goes at once, open handles keep the data); if refused, std deletes through the handle with `FILE_DISPOSITION_FLAG_POSIX_SEMANTICS`.
- `File::try_lock` / `try_lock_shared` are `LockFileEx` with `LOCKFILE_FAIL_IMMEDIATELY` over the whole file. Handles are not inherited by child processes.
- `File::sync_all` is `FlushFileBuffers`. `Child::kill` is `TerminateProcess`. `process::abort` is `__fastfail`, which ends the process with `STATUS_STACK_BUFFER_OVERRUN` (`0xC0000409`), not a signal.
- Paths: absolute paths longer than `MAX_PATH` get the `\\?\` prefix internally, so long paths work without the registry setting; drive letters, UNC (`\\server\share\…`) and `\\?\` paths are accepted as they are.

## Decision

### Directory fsync

`StdFs::sync_dir` on Windows opens the directory with `OpenOptions::new().write(true)` and `OpenOptionsExt::custom_flags(FILE_FLAG_BACKUP_SEMANTICS)` (needed to open a directory at all; write access because `FlushFileBuffers` needs it), then calls `File::sync_all` (`FlushFileBuffers`). No unsafe code: `FILE_FLAG_BACKUP_SEMANTICS` is the constant `0x0200_0000` in `io.rs`, which is part of the Win32 ABI and never changes. **No new dependency** for it (`windows-sys` would only give the constant).

- **What it guarantees.** On NTFS, entries (created, renamed, removed files) are in the volume's metadata journal; `FlushFileBuffers` on the directory flushes the directory and the journal up to its changes, so after `Ok` they survive an OS crash. Windows doesn't document the call for directories; we rely on NTFS's behaviour, as SQLite and others rely on NTFS for files, and the runner shows that the call succeeds on NTFS. ReFS behaves the same way.
- **What it doesn't.** FAT32, exFAT and network shares (SMB) make no such promise (FAT has no journal; a share forwards the flush to a server whose behaviour we don't know). They are not supported, as network file systems aren't on Unix. Over SMB the flush of a directory fails (`ERROR_INVALID_FUNCTION`, seen on the runner with `\\localhost\C$\…`), so a store there refuses to open with the sync error (tested); FAT and exFAT aren't detected.
- **A failure fails the operation, as on Unix.** The error is returned, never ignored or retried as success (fsyncgate, ADR 0005): the WAL becomes read-only, a checkpoint disables checkpoints until the next open, a backup or restore fails.
- **The core's `write_atomic`** skipped the directory sync on Windows. Until upstream synced it (upstream issue, [draft 27](../upstream-issues.md)), `StdFs::write_atomic` called `fsync_dir` on the parent after the core's `write_atomic` succeeded, on Windows only, and so does `Ns::export_file`, which now writes through `StdFs`. *Update: the core syncs it since `73d8fab` (#71), the same way; our call is gone.* Where we already sync the directory afterwards (the checkpointer, ADR 0006 and the #32 deviation), Windows syncs twice, as Unix does; that costs one flush per checkpoint.

### Renames and removals while a file is open

Nothing changes in the code: every file is opened through std, so with `FILE_SHARE_DELETE`. The outcomes, the same as on Unix, and tested on the runner:

- **`write_atomic` over a name another handle holds open** (a checkpoint over a name being read, the archive marker) succeeds; the reader keeps reading the old file, a new open sees the new one.
- **The checkpointer removes a WAL segment that a change-stream reader or a backup has open**: the removal succeeds, and the reader reads the segment to its end, as on Unix. A reader that lists the segments and opens one after its removal gets `NotFound`, which the change stream reports as not retained (as on Unix). Backups hold every namespace's checkpointer mutex while they copy, and restores read under the data directory's lock (ADR 0009), so neither sees a removal.
- **Removing a checkpoint that a reader has open** succeeds; the reader reads its open file to the end.

A removed file that is still open can also stay "delete pending" under its name until the last handle closes, and opening it then fails with `ERROR_ACCESS_DENIED` (`PermissionDenied`). That is the rule on volumes without POSIX semantics (FAT, exFAT, which aren't supported), but it happens on NTFS too: the Windows crash harness on CI saw it when the checkpointer removed a WAL segment while a `sync` with `FsyncPolicy::Off` opened it to fsync it, most likely because a handle that isn't ours (a virus scanner's) made the removal fall back to the old semantics. The `sync` failed the log, and the store turned read-only. So wherever we open a file that the checkpointer may have removed after we listed it (the writer's sync of older segments, the change stream), `io::open_unless_removed` counts `NotFound` as removed and, on Windows, retries `PermissionDenied` for up to 500 ms, until the name goes and the open gets `NotFound`. A refusal that lasts longer is an error, as a real permission error is (and as every `PermissionDenied` is off Windows). Names are never reused (they are seqs), so a pending name never hides a new file.

We don't open files with a narrower share mode to stop renames: that would turn the checkpointer's removals into failures (and disable checkpoints) whenever a reader is behind.

### The data directory lock

std's `try_lock` / `try_lock_shared`, as on Unix (ADR 0006's step 11b update): `LockFileEx`, exclusive or shared, over the whole `LOCK` file. No `fs4`.

- A second store, in this process or another, is refused with `Locked`; `verify` and restore take the shared lock. Tested across processes on every platform.
- Locks belong to the handle and die with the process, however it ends (`TerminateProcess` included). Microsoft documents that the release after a process's end can take "time depending on available system resources". The store's retries (about 80 ms, step 7) stay as they are: the handle is closed before the process counts as ended, and a reopen right after `Child::wait` of a killed child, the crash harness's every cycle, finds the lock free (tested). A longer wait would also make every real `Locked` slower.
- `LockFileEx` locks are mandatory: no other handle may read or write the locked range. `LOCK` is empty and the store never reads it; a tool that reads every file of a live data directory (a file-level copy, our tests' snapshots) gets `ERROR_LOCK_VIOLATION` for it on Windows. Such a copy isn't a backup anyway (use `backup`, ADR 0009).
- Handles aren't inherited, so the Unix race of step 7 (a process spawned by another thread holds the lock file until its exec) doesn't exist; the test of it runs on Windows too.

### Process death in the crash harness and the failpoints

- **The kill** is `Child::kill`, `TerminateProcess` on Windows: no destructor, no flush, no unwinding, like `SIGKILL`. The harness code is the same on every platform.
- **The failpoints' abort** is `std::process::abort` everywhere. On Windows it ends the process with exit code `0xC0000409`; the harness and the abort tests check that code where Unix checks `SIGABRT`. A child that panicked (exit code 101) or was killed (`TerminateProcess` gives exit code 1) is never taken for an abort.
- **Failpoint paths** match with `/` as the separator on every platform (a `\` in the path is read as `/`), so the scenarios' rules (`path=/wal/`) fire on Windows.
- **A full disk** is `ERROR_DISK_FULL` (112) on Windows, `ENOSPC` (28) on Unix, as the OS would report it.
- **The OS-crash simulation** (ADR 0007) changes the last WAL segment's unsynced end after the kill: it models losing what the last fsync didn't cover. That is the same claim on Windows, where `FlushFileBuffers` is the fsync, so the simulation runs unchanged. As on Unix, a lost directory entry isn't simulated; the directory flushes above cover `always` and `group`.
- The **short run** (150 cycles × 3 policies) runs on windows-latest on every push, the **long run** nightly, with the same scenarios and checks (reference model, recovery, verify, backups, restores).

### The server

- **Shutdown events**: Ctrl-C and Ctrl-Break (`tokio::signal::windows::ctrl_c`, `ctrl_break`), the console's close and the system's shutdown (`ctrl_close`, `ctrl_shutdown`) start the graceful drain (ADR 0027), counted like `SIGINT` / `SIGTERM`: a second event ends the drain early. Windows ends a process a few seconds after a close or shutdown event (about 5 s, not configurable by us): a drain or final checkpoint cut short then is a kill, which recovery handles (nothing acknowledged is lost; the next open replays the WAL). Logoff is ignored: a server isn't tied to whoever logs off. Ctrl-Break is the event to send to a server in a process group of its own (`GenerateConsoleCtrlEvent`), which is what the tests do; Ctrl-C can't be sent to another process group.
- **No certificate reload on Windows.** There is no `SIGHUP`, and the alternatives aren't worth it now: a file watch needs a dependency and has to tell a half-replaced certificate and key apart from a finished replacement (two files, written in some order, by some tool); a named event or an admin call is new API for one platform. Rotating a certificate means restarting the server, which the graceful drain makes short. Documented in api/config.md and guarantees.md. A reload through the admin API, on every platform, can come later.
- **Audit file permissions**: Unix creates the files `0600` and the directory `0700`. Windows gives them the **inherited ACL** of the directory they are created in. Setting an ACL needs Win32 security calls (unsafe, or a dependency), and a wrong one can lock out administrators and backup tools. That is acceptable because the same holds for the data directory, which holds password hashes and every value: the operator puts both in a directory only the server's account and administrators can read. Documented.
- **Disk free space**: `fs4::available_space` (`GetDiskFreeSpaceExW`'s bytes available to the caller, which honours quotas: the same meaning as `statvfs`'s `f_bavail × f_frsize` on Unix). `fs4` is a Windows-only dependency of `iwdb`: well known (the maintained `fs2`), MIT or Apache-2.0, with `windows-sys` (already in the Windows tree through tokio) and no unsafe code of ours. Unix keeps `rustix`.
- `iwdb-server --probe`, health and readiness need nothing platform-specific.

### `iwctl`'s password prompt

`rpassword` reads the password with the console's echo off on Windows (`SetConsoleMode`). A Windows-only dependency of `iwctl` (a binary): well known, Apache-2.0, `windows-sys` again. Unix keeps `stty`. From a pipe the password is read as a line, as before.

### Paths

- Drive letters, UNC paths and `\\?\` paths are passed to std as they are. Long paths work (std adds `\\?\` for long absolute paths). A data directory on a network share (a UNC path to another machine) is not supported, as above; a UNC path to a local volume (`\\?\C:\…`) is the same volume.
- Names in the data directory are fixed or digits (`ns/<id>`, `<seq>.ckpt`, `<seq>.wal`), so case-insensitive file systems can't make two of them collide.
- A relative `data_dir` resolves as ADR 0039 says, on Windows too: from the config file, relative to the file's directory; from `IWDB_DATA_DIR`, to the working directory.
- `crates/iwdb-server/proto` is a symlink (ADR 0035), which a Windows checkout without symlink support makes a text file: `build.rs` falls back to the workspace's `proto/` then. `.gitattributes` keeps text files LF on every checkout (`* text=auto eol=lf`), so fixtures and generated documents compare equal on Windows.

### Python

Wheels for Windows x86_64 (abi3, CPython 3.9 and later), built by maturin on windows-latest. CI runs the Python suite on Python 3.9 and 3.13 on Windows, including the remote half against a Windows-built `iwdb-server`; the release workflow builds and tests the Windows wheel with the others. No Windows arm64 wheel yet (no runner in the matrix).

### What stays Linux-only

The Docker image. Windows services and installers are not part of this step.

## Consequences

- Windows is supported on NTFS (and ReFS), with the guarantees of guarantees.md. The differences are listed there: no certificate reload, inherited ACLs for the audit files, no FAT/exFAT or network shares.
- CI runs fmt, clippy, the workspace tests, the short crash run and the Python suite on windows-latest; the nightly crash run on Windows too. Windows runners are slower: tests that depend on timing leave room for a stalled runner.
- Two Windows-only dependencies: `fs4` (`iwdb`) and `rpassword` (`iwctl`).
- A Windows checkpoint costs one more directory flush than before (the core's `write_atomic`, then ours), until upstream syncs the directory itself; then `StdFs::write_atomic`'s extra sync goes (upstream check).
