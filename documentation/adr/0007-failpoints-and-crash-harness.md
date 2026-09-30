# ADR 0007: Failpoints and the kill -9 crash harness

Status: accepted
Date: 2026-09-30

## Context

Step 6 has to prove the durability guarantee ([guarantees.md](../guarantees.md)): after any crash, recovery reaches the last acknowledged commit (with `always`) and never exposes a partial transaction. That needs two things:

- **fault injection** at every write-side file operation: WAL append, fsync, segment creation and rotation, rename, directory fsync, checkpoint write, file removal and truncation. Each must fail, report a full disk (`ENOSPC`), tear a write, or stop there;
- **real crashes**: a process killed with `SIGKILL` at random points and at chosen ones, then recovered and compared with a reference, thousands of times.

Facts that shaped the design:

- Every write already goes through the `iwdb_storage::io::LogFs` / `LogFile` seam (steps 4 and 5). Reads don't: a failed read changes nothing.
- The `fail` crate (tikv, 0.5.1, Apache-2.0) keeps one global registry. Tests that configure it must run one at a time (`FailScenario` takes a process-wide lock), and the configuration is a string per named point. It brings `rand` 0.8 and `once_cell`. It would pass `cargo deny` and build on 1.85.
- The core's `write_atomic` is one call. A failpoint can act before it, inside our writer (which writes into the core's `BufWriter` over the temporary file), and after it. It can't act between the core's fsync of the temporary file and its rename.
- A process kill loses nothing that was written, because the page cache survives. Only an OS crash or power loss loses unsynced data, and a kill -9 test can't produce one.
- The random workload strategies were test code, included with `#[path]`. A child binary can't include them that way.

## Decision

### Failpoints: a `FailFs` wrapper behind a feature

`iwdb_storage::failpoint::FailFs<F: LogFs>` wraps any `LogFs`. It exists only with the `failpoints` cargo feature. `StdFs` has no failpoints, so normal builds pay nothing. A `Rule` names the call (`create`, `open_append`, `rename`, `sync_dir`, `write`, `sync`, `write_atomic`, `remove_file`, `truncate`), the occurrence (`skip`), an optional path substring, the point (`before`, `midway`, `writer_done`, `after`) and the action (`fail`, `nospace`, `pause`, `panic`, `abort`). Each rule fires once. `FailFs` also records every call, and runs a hook before each one.

- **Per instance, not global**: the rules belong to one `FailFs` and its clones, so tests stay parallel. That is the reason for not using `fail`. It also needs no new dependency.
- **Points in a call**: `midway` writes half of a WAL write (a torn frame), or stops our checkpoint writer after half of its first write, flushed into the temporary file. `writer_done` stops `write_atomic` after the writer has finished and its output is flushed. `after` acts once the operation has happened, so a failing action reports an error for work that was done (a write that reached the file, a rename or a cut that happened).
- **The point between `write_atomic`'s fsync and its rename** is not reachable. For a process crash it is the same state as `writer_done`: the temporary file is complete in the page cache and not renamed, and an fsync changes nothing a kill can see. The harness pauses there and kills. For an OS crash the difference would matter, but checkpoints are durable only after our own directory sync, and nothing depends on the temporary file.
- **Text form**, `<call>:<when>:<action>[:skip=N][:path=S]`, so the parent can pass rules to a child on its command line.
- `TestFs`, used by the step 4 and 5 tests, is now `FailFs<StdFs>`, so the existing fault tests run through the same code.

### The harness: one binary, parent and child

`tests/crash` (`iwdb-crash`) is a workspace package: `tests/` is where the planned layout puts crash tests. The binary runs as parent by default, and as child with `iwdb-crash child ...`. The workload strategies moved into `iwdb_engine::testutil::workload` behind a `testutil` feature, together with `Stream`, an endless workload from a seed.

**The child** opens the store through `FailFs`, with the parent's rule, runs a script from its seed (commits, explicit checkpoints, syncs, short sleeps), and reports on stdout, flushing each line after the event:

- `open <seq> <synced> <digest>` once recovery is done;
- `ack <seq> <synced>` after each commit that returned `Ok`, with `Store::synced_seq`;
- `paused <rule> <path>` when a pause fires (the thread then blocks);
- `done` when the script ends, and `error ...` on any unexpected error.

It runs 1 KiB segments, background checkpoints every 8 KiB of WAL or 20 ms, and keeps 1 to 3 checkpoints, so kills land in rotations, checkpoints and WAL segment removal. Before every fsync it appends `<path> <length>` to a sync log. The length is written before the fsync runs, so it is an upper bound on what that fsync makes durable.

**The parent** runs cycles, each with a plan chosen from its seed:

- a **random delay** (up to 60 ms after `open`, sometimes counted from the spawn so that the kill lands in recovery), then `SIGKILL`;
- a **failpoint**: pause, then `SIGKILL` (most plans); or abort, or panic on the commit path, in the child itself. The plans cover every call, including recovery's truncation and, on new directories, each step of initialization.

It then simulates, sometimes, an **OS crash**: in the last segment it truncates or zeroes bytes after the durable length (the sync log's, and at least the header except with `off`). Then it **recovers** by opening the store. In a quarter of the cycles without an OS crash it leaves the recovery to the next child, whose `open` digest it checks. That way kills also land in a recovery that follows another kill.

**The reference** reruns the child's commits in memory. The workload and each commit's outcome are deterministic from the seed and the state, and a commit that fails validation uses no seq in either process. So the model reaches the state at any seq the child could have reached. It keeps every record, so it can also go back.

**Checks** after every recovery, where `A` is the last acknowledged seq the child reported:

| Case | Recovered seq | Also |
|---|---|---|
| `always`, any crash | `A` or `A + 1` | |
| `group` or `off`, process kill | `A` or `A + 1` (the page cache survives) | |
| `group`, OS crash | at least the highest synced seq reported, at most `A + 1` | |
| `off`, OS crash | at most `A + 1` | may refuse with `LogEndsBefore`, only if the newest checkpoint is past what is left of the log |

`A + 1` is the commit in flight: complete in the log, or acknowledged just before the kill and not yet reported. In every case the canonical graph, the catalog and the seq must equal the model's at the recovered seq, so no partial transaction and nothing out of order. The seed of every run is printed. A failure prints the policy, the seed, the cycle and a command that reruns it, and keeps the files. Kill timing is wall-clock time, so a rerun makes the same plans but kills at slightly different moments.

**Deterministic crash points**. `tests/crash/tests/crash_points.rs` pauses a child at each point step 5 listed and kills it, under every policy: an append, inside `write_atomic` (halfway, and complete but not renamed), between a checkpoint's rename and its directory sync, between removing old checkpoints and WAL segments, in the middle of removing segments, a rotation's rename, recovery's truncation (before and after), initialization (before and after the marker), an abort and a panic in the commit path. Each checks the files at the kill, and then the recovered state.

**Runs**: `cargo test` runs 12 cycles per policy (`tests/short_run.rs`) and the crash points. CI runs 150 cycles per policy on Linux and macOS for every push and PR, with the run id as the seed. The nightly workflow runs 2000 cycles × 3 seeds per policy and platform; the cycle count and seeds are inputs.

## Consequences

- Every write-side call has a failpoint and at least one test of what happens when it fails or stops. The per-call outcomes are in guarantees.md, "Simulated failures". The WAL-level tests from step 4 now run through `FailFs` as well.
- The harness checks the guarantee end to end, with the real `Store`, its background threads and a real `SIGKILL`. It is not a model of the store. Two deliberately wrong changes were tried while building it, and both failed the run within the first cycles: an OS-crash simulation that also cut synced data, and a model that skipped catalog changes.
- Not simulated: an OS crash's loss in files other than the last segment, lost directory entries, and, with `off`, loss in earlier segments or in a segment header while later frames survive. For `always` and `group` the directory fsyncs cover these. For `off` the guarantees don't promise anything about them.
- Building the harness found two bugs from steps 4 and 5, fixed in their own commits (see step_6.md): `Wal::sync` under `off` didn't sync rotated segments, and a panic in a read made the store read-only. It also led to ADR 0008 (a panic in the commit path aborts).
- `iwdb-crash` depends on the `failpoints` and `testutil` features. In a workspace build, cargo unifies features, so those modules are compiled into the workspace's `iwdb-storage` and `iwdb-engine` as well. They are unused there, and `StdFs` stays free of failpoints. A downstream user of `iwdb` doesn't get them.
