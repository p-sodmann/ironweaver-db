# Guarantees

What Ironweaver DB promises, and under which conditions. Anything not listed here is not guaranteed.

How to read this page:

- Every guarantee ends with **Tests:** naming the tests that check it (a test function, or `file::test` where a name is ambiguous; `ci.yml#job` for a CI job). `crates/iwdb/tests/docs.rs` fails if a guarantee names no test or a test that doesn't exist.
- **Assumes:** lists what a guarantee needs from the system around it. **Not guaranteed:** lists the limits, and **Notes:** explains, without promising anything.
- Terms (namespace, seq, version, idempotency key, mark) are explained in [concepts.md](concepts.md). The decisions behind each section are in the ADRs it links; how each part was built is in the [step files](steps/README.md).

## Durability of commits

Every commit is appended to the write-ahead log ([format](formats/wal.md)) before it is applied in memory and acknowledged. When it becomes durable depends on the fsync policy ([ADR 0005](adr/0005-wal-fsync-and-failures.md)):

| Policy | Acknowledged commits lost on a process crash | Acknowledged commits lost on an OS crash or power loss | Tests |
|---|---|---|---|
| `always` (default) | none | none | `always_syncs_every_commit_and_every_new_segment`, `short_run::always` |
| `group { max_delay, max_batch }` | none | only the commits after the last completed fsync: a suffix of the log, fewer than `max_batch` commits, none older than about `2 × max_delay` (the store's timer syncs every `max_delay`) | `group_commit_syncs_every_batch_and_when_due`, `the_group_commit_timer_syncs_idle_commits`, `group_commit_tolerates_out_of_order_loss_of_unsynced_records_only`, `short_run::group` |
| `off` (tests only) | none | anything not covered by an explicit sync (`Store::sync`, `checkpoint`, `close`). Recovery may also refuse to open the log: with `LogEndsBefore` if a checkpoint is newer than what is left of the log, and with a corruption error if the loss hit an earlier segment, or a segment's header while later frames survived | `off_never_syncs`, `off_syncs_every_unsynced_segment_on_an_explicit_sync`, `short_run::off` |

- **Lost commits are a suffix.** Commits are applied in seq order; if commit `n` survives, so do all commits before it. Tests: `reopening_always_recovers_the_reference_state`, `group_commit_tolerates_out_of_order_loss_of_unsynced_records_only`.
- **New segments and renames are durable** with a directory fsync. Tests: `always_syncs_every_commit_and_every_new_segment`, `a_failed_rotation_loses_only_the_commit_that_needed_it`.
- **Closing syncs everything.** `Store::close` fsyncs every WAL; dropping a store without `close` doesn't. Tests: `clean_shutdown_and_reopen`, `off_syncs_every_unsynced_segment_on_an_explicit_sync`.

**Assumes:**

- The storage honours flushes. The WAL is synced with `File::sync_all`: `F_FULLFSYNC` on macOS (where `fsync` alone doesn't flush the drive's cache), `fsync` on Linux, `FlushFileBuffers` on Windows. A drive or virtual disk that acknowledges flushes without persisting them voids every durability guarantee.
- A local file system: see [Platforms](#platforms).

## Atomicity and failed commits

- **A commit is all or nothing.** It is one log record, applied whole or not at all; no reader sees part of it. Tests: `a_failing_transaction_changes_nothing`, `readers_see_the_state_at_some_seq_never_part_of_a_transaction`.
- **A commit that fails validation changes nothing** (a conflict, a constraint violation, a reserved name, a record above 64 MiB, ...): it isn't logged, and the namespace stays writable. Tests: `a_failing_transaction_changes_nothing`, `catalog_changes_are_validated_against_the_data`, `reserved_keys_are_rejected_in_every_mutation`, `a_record_above_the_limit_is_rejected_before_anything_happens`.
- **A commit that fails because the log can't be written or fsynced** is not applied and not acknowledged. Its outcome is unknown: the record may still reach the log and be recovered after a restart. Retry it with an idempotency key to apply it exactly once. Tests: `a_write_that_fails_after_reaching_the_file_is_recovered`, `a_failed_wal_fsync_is_not_retried_and_its_record_is_recovered`, `a_retry_after_an_unknown_outcome_applies_once`.
- **After such a failure the namespace is read-only until reopened.** Reads keep working and see every applied commit; writes fail with a read-only error; a failed fsync is never retried. The store reports it (`Store::read_only`), and reopening runs recovery and makes it writable again. Tests: `a_failed_write_is_not_applied_and_makes_the_namespace_read_only`, `a_failed_fsync_is_not_applied_not_retried_and_makes_the_namespace_read_only`, `a_wal_failure_makes_the_store_read_only_until_reopened`.
- **A failed apply poisons the namespace.** If applying a logged commit fails and the core rolled it back, the namespace becomes read-only as after a WAL failure, and reads see the state before the commit. Tests: `a_failed_apply_poisons_the_namespace`, `a_poisoned_namespace_is_read_only`, `a_cleanly_rolled_back_apply_error_makes_the_namespace_read_only`.
- **A panic while the store changes a namespace or its WAL** (a commit, an fsync, the group commit timer) **aborts the process** ([ADR 0008](adr/0008-panics-in-the-commit-path-abort.md)), and so does an apply that fails inside the core with `GraphError::Internal` ([ADR 0028](adr/0028-internal-apply-errors-abort.md)). It is a crash: no reader sees a partial transaction, and the next open recovers every logged commit. Tests: `a_panic_before_a_wal_write_aborts_and_loses_nothing_acknowledged`, `a_panic_in_a_wal_fsync_aborts_and_recovers_the_written_record`, `a_panic_in_the_group_commit_timer_aborts_and_loses_nothing`, `an_internal_apply_error_aborts_and_recovery_restores_the_log`.
- **A panic in a read closure changes nothing** and leaves the store writable. Tests: `a_panic_in_a_read_leaves_the_store_writable`.

**Not guaranteed:**

- Atomicity across namespaces: a commit goes to one namespace.

## Recovery

`Store::open` takes the data directory's exclusive lock, then rebuilds each namespace from its newest checkpoint that loads and every WAL record after it ([data-dir.md](formats/data-dir.md), [ADR 0006](adr/0006-checkpoints-and-recovery.md)).

- **It recovers the last complete commit** in the log, with the catalog as of that commit: every acknowledged commit the fsync policy made durable. Tests: `crash_after_append`, `reopening_always_recovers_the_reference_state`, `short_run::always`.
- **A torn tail is cut.** A crash during an append leaves a torn last record; recovery cuts it, and its commit was never acknowledged. Tests: `crash_during_an_append_leaves_a_torn_last_frame`, `a_disk_full_halfway_through_a_frame_leaves_a_torn_tail_that_recovery_cuts`.
- **A crash during a checkpoint loses nothing**: it leaves the previous checkpoints and a temporary file (removed on open), or the new checkpoint without the WAL cut yet. Tests: `crash_mid_checkpoint_leaves_a_temp_file`, `crash_mid_checkpoint_before_the_wal_is_cut`.
- **A damaged checkpoint is skipped**, and recovery falls back to the next older one and replays more WAL, as long as one kept checkpoint loads (2 are kept by default). Tests: `a_damaged_newest_checkpoint_falls_back_to_the_older_one`, `no_fallback_without_the_wal_it_would_need`.
- **It refuses rather than repairs.** Corruption in the WAL, WAL records missing behind the newest usable checkpoint, a WAL that ends before a checkpoint, or a record that fails to replay: open fails with a typed error and changes no data file. Tests: `wal_corruption_is_refused_without_changes`, `a_wal_that_ends_before_the_checkpoint_is_refused`, `a_record_that_fails_to_replay_is_reported_and_not_truncated`.
- **One store per directory.** A second open of the same directory fails with `Locked`, in the same process or another. The lock is released by `close`, by dropping the store, and when the process exits, `kill -9` included. Tests: `a_second_open_fails_until_the_first_is_closed_or_dropped`, `the_lock_holds_across_threads`, `the_lock_is_released_when_the_process_dies`, `reopening_while_another_thread_spawns_processes`.
- **Directories that aren't ours are refused unchanged**, and so are a newer layout, a damaged marker, a backup and an interrupted restore. Tests: `directories_that_are_not_ours_are_refused_unchanged`, `a_newer_layout_or_a_damaged_marker_is_refused`, `backups_and_interrupted_restores_are_refused`.

## Crashes and other failures

What happens at each failure the storage layer can meet ([ADR 0007](adr/0007-failpoints-and-crash-harness.md)). Each row is checked by failpoint tests, which reopen and compare with a reference, and by the kill -9 harness (`tests/crash`), which runs them as real crashes.

| Failure | Defined behaviour | Tests |
|---|---|---|
| **Process crash** (`kill -9`, abort) anywhere: in an append, a rotation, a checkpoint, a removal, recovery, initialization | Nothing written is lost under any policy (the page cache survives). The next open recovers the last acknowledged commit, or the one in flight if its record is complete. It removes temporary files, cuts a torn tail and finishes an interrupted initialization or cleanup | `during_an_append`, `inside_a_checkpoint_write`, `between_a_checkpoints_rename_and_its_directory_sync`, `in_the_middle_of_removing_segments`, `during_recoverys_truncation`, `during_initialization`, `short_run::always` |
| **Torn WAL frame** (a crash or an error halfway through an append) | A torn tail: recovery cuts it. The commit was never acknowledged | `crash_during_an_append_leaves_a_torn_last_frame`, `a_disk_full_halfway_through_a_frame_leaves_a_torn_tail_that_recovery_cuts` |
| **Torn segment header** | A segment is created as `<name>.tmp` and renamed only after its header is written (and, except with `off`, fsynced), so a torn header is in a temporary file, which open removes. A last segment without a valid header is a torn tail at offset 0: removed | `a_segment_without_a_valid_header_is_removed`, `between_a_rotations_rename_and_its_directory_sync` |
| **Partial checkpoint** | Exists only as a temporary file, never under a checkpoint's name: open removes it, and recovery uses the previous checkpoint | `crash_mid_checkpoint_leaves_a_temp_file`, `inside_a_checkpoint_write` |
| **Full disk (`ENOSPC`) or I/O error on a WAL write**, whole or halfway, in an append or a rotation | The commit fails with `Io` (`StorageFull` for a full disk), is not applied, and the store is read-only until reopened. Its record is not in the log, or is a torn tail that recovery cuts. Opening needs room for a new segment; until there is room, open fails with `Io` and loses nothing | `a_failed_or_full_wal_write_loses_only_the_failed_commit`, `a_full_disk_when_the_writer_starts_fails_the_open_and_loses_nothing` |
| **A WAL write or fsync that reports an error after the data reached the file** | Like any failed write or fsync: not applied, read-only, never retried. The outcome is unknown, and recovery finds the record | `a_write_that_fails_after_reaching_the_file_is_recovered`, `a_failed_wal_fsync_is_not_retried_and_its_record_is_recovered` |
| **Failed WAL fsync** (the commit's, a rotation's, the group commit timer's) | The commit fails (with the timer, no commit notices), and the store is read-only until reopened. The fsync is never retried | `a_failed_fsync_is_not_applied_not_retried_and_makes_the_namespace_read_only`, `a_failed_group_fsync_fails_the_commit_that_waits_for_it`, `a_failed_group_commit_timer_fsync_makes_the_store_read_only` |
| **Failed directory fsync of `wal/`** (a new segment) | The commit that needed it fails, and the store is read-only until reopened | `a_failed_rotation_loses_only_the_commit_that_needed_it`, `a_failed_rotation_is_not_applied_and_makes_the_namespace_read_only` |
| **`ENOSPC` or I/O error on a checkpoint write** (before, halfway, when complete but not renamed) | The checkpoint fails with `Io`. Its temporary file is removed, nothing else changes, commits go on, and the next trigger retries | `a_checkpoint_that_fails_before_its_rename_deletes_nothing_and_is_retried`, `a_failed_checkpoint_write_keeps_the_previous_checkpoint_and_the_wal` |
| **A failure after a checkpoint's rename**: the directory fsync of `checkpoints/` or `wal/`, removing an old checkpoint or a WAL segment | The checkpoint fails. The new file stays, and what wasn't removed yet stays. Checkpoints are disabled until the store is reopened (`CheckpointsDisabled`), because a retried directory fsync can succeed without persisting anything. Commits go on, and recovery uses the new checkpoint | `a_checkpoint_that_fails_after_its_rename_disables_checkpoints_until_reopened`, `a_failed_directory_sync_disables_checkpoints_until_reopened`, `a_checkpoint_whose_write_reports_an_error_after_the_rename_deletes_nothing` |
| **A failure while opening**: removing temporary files, cutting the torn tail (or its fsync), removing a headerless segment, starting the writer, initializing | Open fails with `Io` and releases the lock. What it did is part of recovery's defined repairs, and the next open finishes the rest | `a_failed_truncation_of_the_torn_tail_is_finished_by_the_next_open`, `a_failed_removal_of_a_headerless_segment_is_finished_by_the_next_open`, `a_failed_removal_of_temporary_files_is_finished_by_the_next_open`, `a_failed_initialization_is_finished_by_the_next_open` |
| **A panic in the commit path**, or an apply that fails with `GraphError::Internal` | The process aborts (ADRs 0008 and 0028), which is a process crash (first row) | `an_abort_or_a_panic_in_the_commit_path`, `an_internal_apply_error_aborts_and_recovery_restores_the_log` |
| **A crash or a failure during an online backup**: any file write, fsync, directory sync, the manifest, the marker | The backup fails (or the process dies), and the store is unaffected: a backup only reads the store's files. The backup directory has no marker until its last step and holds an empty `BACKUP` from its first: a store, `verify` and restore all refuse it. A failure after the marker's rename leaves a complete backup | `every_write_of_a_backup_can_fail_and_leaves_nothing_valid_looking`, `during_a_backup` |
| **A crash or a failure while archiving**: a segment's copy (create, write, fsync, rename) or the archive's directory sync | No segment leaves `wal/` before it is durable in the archive, so none is lost. A failed copy fails the checkpoint and is retried by the next one; a failed directory sync disables checkpoints until reopening. A crash between archiving and removal leaves a segment in both places, archived again by the next checkpoint | `every_archive_write_can_fail_without_losing_a_segment`, `archiving_is_idempotent`, `during_archiving` |
| **A crash or a failure during a restore** | The target is refused by the next open (`InterruptedRestore` while `RESTORING` exists; `NotADataDir` for a checkpoint without a marker), or is empty, or complete once its marker is in place. Never a wrong state. The sources are only read | `every_write_of_a_restore_can_fail_and_leaves_nothing_wrong`, `during_a_restore`, `backups_and_interrupted_restores_are_refused` |
| **OS crash or power loss** | Per policy (the durability table). The harness simulates it by cutting or zeroing the unsynced end of the last segment | `short_run::always`, `short_run::group`, `short_run::off`, `group_commit_tolerates_out_of_order_loss_of_unsynced_records_only` |

**Notes:**

- The harness kills a child running a random workload with background checkpoints, at random moments and at failpoints on every write-side call, sometimes with a simulated OS crash, then recovers and compares the seq, the canonical state and the catalog with a reference model. Every run prints its seed. CI runs 150 cycles per policy on every PR (`ci.yml#crash`) and thousands nightly ([step 6](steps/step_6.md) has the numbers).

## Checkpoints

- **A checkpoint holds exactly the commits up to its seq**, catalog included, and only commits already synced to the WAL, so the WAL always reaches it. Tests: `background_checkpoints_never_pass_the_synced_seq`, `the_wal_is_cut_to_what_the_oldest_kept_checkpoint_needs`.
- **Checkpoints don't block commits**: the checkpointer replays the WAL into its own copy of the namespace. Tests: `commits_keep_flowing_while_a_checkpoint_runs`, `commit_latency_during_a_checkpoint`.
- **A failed checkpoint never deletes a WAL segment or changes a previous checkpoint.** If it fails after its new file is in place, checkpoints stay off until reopening, commits go on, and `Store::checkpoint_failure` reports it. Tests: `a_failed_checkpoint_write_keeps_the_previous_checkpoint_and_the_wal`, `a_failed_directory_sync_disables_checkpoints_until_reopened`.

**Not guaranteed:**

- Memory: the checkpointer's copy is a second copy of the namespace's graph while it runs.

## Integrity of the log

- **Every record and segment header is checksummed** (CRC32C over all of its fields and payload). Tests: `every_bit_flip_is_a_torn_tail_in_the_last_record_and_an_error_before_it`, `a_flipped_byte_in_any_file_is_a_problem`.
- **Damage at the end is the end of the log**: a torn or damaged last record (from a crash during a write) is reported for recovery to cut; the records before it are intact. Tests: `truncation_at_every_offset_ends_the_log_before_the_cut_record`, `garbage_after_the_log_is_a_torn_tail`, `a_torn_last_segment_ends_the_log`.
- **Damage anywhere else is an error, never skipped**: in an earlier segment, before a record that proves the damaged one was synced, a gap or a repeat in seq, or an unreadable record with a valid checksum. Tests: `damage_in_an_earlier_segment_is_an_error`, `gaps_and_mismatches_between_segments_are_errors`, `a_checked_but_invalid_last_record_is_an_error_not_a_tail`, `replay_rejects_gaps_and_repeats`.
- **Reading never panics on corrupt input** and never allocates more than the file's size for a corrupt length. Tests: `arbitrary_segments_never_panic`, `arbitrary_frames_never_panic`, `arbitrary_payloads_never_panic`, `a_huge_segment_file_is_rejected_before_it_is_read`, `ci.yml#fuzz`.

## Concurrency and isolation

[ADR 0014](adr/0014-concurrent-readers.md).

- **One writer per namespace, many readers.** Commits are serialized. Reads run concurrently with each other and with a commit's validation, WAL append and fsync; they wait only while a commit applies its record (the namespace's write lock). Tests: `reads_and_commits_go_on_during_a_long_analytics_job`, `readers_see_the_state_at_some_seq_never_part_of_a_transaction`.
- **A read never sees part of a transaction**: it sees the state after some commit, all of whose ops are applied, and the seq it reports is that commit's. Tests: `readers_see_the_state_at_some_seq_never_part_of_a_transaction`, `test_readers_run_while_a_writer_commits`.
- **Analytics run on a copy.** `analyze` copies the graph into a projection under the read lock (O(n + m)) and runs without any lock, on the state at the seq it reports. Tests: `reads_and_commits_go_on_during_a_long_analytics_job`.
- **Unique constraints hold under concurrent writers**, within each namespace. Tests: `unique_constraints_hold_under_concurrent_writers_per_namespace`.

**Not guaranteed:**

- A long `Store::read` closure delays commits (they wait to apply) and the reads queued behind them; long work belongs in `analyze`.

**Notes:**

- Measured write-lock hold times and read latencies are in [step 8](steps/step_8.md) and [benchmarks.md](benchmarks.md).

## Idempotency keys

[ADR 0015](adr/0015-idempotency-keys.md).

- **A keyed commit applies at most once.** A retry with the same key and the same request returns the original result (seq, edge ids, versions, commit time) with `deduplicated` set and commits nothing; the same key with a different request fails with `IdempotencyKeyReused` and changes nothing. So a commit whose outcome is unknown is applied exactly once by retrying it with its key until it succeeds. Tests: `a_retry_returns_the_original_result_and_applies_nothing`, `another_request_under_the_same_key_is_refused`, `a_retry_after_an_unknown_outcome_applies_once`.
- **Keys survive restarts, checkpoints, WAL cuts, backups, restores and crashes**: the key table is part of the namespace's state. Tests: `a_retry_applies_once_across_restarts_and_checkpoints`, `keys_survive_a_backup_and_restores_keep_those_of_their_history`, `replay_and_checkpoints_rebuild_the_table`, `short_run::always`.
- **A key is exactly as durable as its commit**: under `group`, an OS crash that loses an acknowledged commit loses its key too, and the retry applies it once. Tests: `short_run::group`.
- **A restore to seq `N` keeps the keys of commits `1 ..= N`** and none after: a retry of a later commit applies it. Tests: `keys_survive_a_backup_and_restores_keep_those_of_their_history`.
- **A duplicate is answered while the store is read-only.** Tests: `a_duplicate_is_answered_while_the_store_is_read_only`.
- **Keys are 1 to 255 bytes.** Tests: `keys_have_1_to_255_bytes`, `test_invalid_keys_are_refused`.

**Not guaranteed:**

- The store remembers the last 10 000 keyed commits per namespace; a retry after more keyed commits than that applies again (`the_table_evicts_the_oldest_keys`).

## Read-your-writes and deadlines

[ADR 0016](adr/0016-read-your-writes-and-deadlines.md), [ADR 0010](adr/0010-commit-times.md).

- **A read with `min_seq` sees that commit and all before it.** It waits until the commit is applied and returns as soon as it is; it fails with `Timeout` after its timeout (30 s by default), at once with `ReadOnly` if the store is read-only below `min_seq`, and with `OtherHistory` if the seq belongs to another history. Every commit is applied before it is acknowledged, so a client's own commits are visible to its later reads. Tests: `min_seq_waits_until_the_seq_is_applied_and_no_longer`, `a_read_only_store_fails_a_min_seq_wait_at_once`, `read_your_writes`, `test_read_your_writes_with_the_last_seq`.
- **Every commit result carries its commit time** (the writer's wall clock, made non-decreasing, in microseconds). Tests: `test_results_have_a_commit_time_and_are_not_deduplicated`, `both_versions_read_and_only_version_2_has_times`.
- **Every read ends at its deadline**, the wait for a worker included. Tests: `every_read_times_out`, `a_timeout_stops_a_read_in_progress`, `a_read_waiting_for_a_worker_times_out_at_its_deadline`.
- **An analytics job ends at its deadline or when cancelled**, with `Timeout` or `Cancelled`, no partial result, and the store usable. Tests: `analytics_stop_at_their_deadline_or_when_cancelled`.

## Namespaces and the catalog

[ADR 0017](adr/0017-namespaces.md), [ADR 0018](adr/0018-namespace-keys-and-restore.md), [ADR 0019](adr/0019-online-index-build.md); format: [data-dir.md](formats/data-dir.md).

- **Namespaces are independent.** Each is its own graph with its own seqs, WAL, checkpoints, key table, indexes and constraints; a commit goes to one namespace. Unique constraints are scoped to one namespace. Tests: `namespaces_are_independent_graphs_with_their_own_seqs`, `unique_constraints_hold_under_concurrent_writers_per_namespace`, `test_namespaces_are_independent_graphs`.
- **Create and drop are atomic and crash-safe.** A namespace exists exactly when its create event is in the namespace log, and not once its drop event is; the event is fsynced before the call returns, whatever the fsync policy. A crash or failed write at any point leaves the namespace as it was or created / dropped completely. Directories the log doesn't list are removed on the next open if empty; one with data makes the open fail rather than be deleted. Tests: `every_file_operation_of_a_create_can_fail`, `every_file_operation_of_a_drop_can_fail`, `creating_and_dropping_namespaces`, `the_namespace_log_and_the_directories_must_agree`.
- **Ids are never reused.** A dropped namespace's name can be created again, as a new namespace with a new id. Tests: `ids_are_never_reused`.
- **A dropped namespace fails its commits and waits at once**; a read that already started finishes on the state it had. Tests: `a_dropped_namespace_fails_commits_and_wakes_waiters_but_finishes_reads`, `test_a_dropped_namespace_fails_its_handle_and_its_waiters`.
- **Creating and dropping take idempotency keys**, answered after a restart and after a drop; they are never evicted. Tests: `namespace_operations_take_idempotency_keys`, `test_keys_for_create_and_drop`.
- **Older layouts are upgraded.** A store of layout 1, 2 or 3 opens as one namespace, `default`, with its history, seqs, checkpoints, keys and history id; a crash at any point of the upgrade is finished by the next open. Tests: `the_v1_fixture_opens_recovers_its_state_and_is_upgraded`, `the_v3_fixture_opens_recovers_its_state_keys_and_history_and_is_upgraded`, `every_file_operation_of_the_layout_upgrade_can_fail`, `upgrading_to_layout_4`.
- **Indexes are built online**: reads and commits go on while the keys are read, and the index is installed in time proportional to the nodes changed during the build. An abandoned build leaves nothing. Tests: `an_online_index_build_sees_commits_made_during_the_scan`, `an_abandoned_index_build_leaves_nothing_behind`, `latency_during_an_online_index_build`.
- **The catalog decides which indexes exist**; they are rebuilt from it on recovery. Tests: `the_catalog_decides_which_indexes_exist_after_loading`, `catalog_changes_show_in_catalog_and_status`.

**Not guaranteed:**

- A unique constraint's validation holds the writer for its whole scan.
- An online build that is killed simply isn't there (no record was logged).

## Backups, archiving and restore

[ADR 0009](adr/0009-backup-archive-restore.md); formats: [backup.md](formats/backup.md), [archive.md](formats/archive.md).

**Online backup** (`Store::backup`, `iwctl backup`, `store.backup()` in Python):

- **It reaches the last commit and nothing the store could still lose.** The WAL is fsynced first; the backup holds every commit up to the seq then synced. Tests: `a_backup_syncs_first_and_reaches_the_last_commit`.
- **It holds every checkpoint at or below that seq** and the WAL from the oldest of them, cut right after the seq, so it restores to any seq in that range. Tests: `a_backup_holds_the_checkpoints_and_the_wal_up_to_its_seq`, `pitr_from_a_backup_alone`.
- **It is consistent while the store runs**: no checkpoint or segment it copies is removed during the copy; commits wait only for the fsync, checkpoints wait for the copy. Each namespace is consistent at its own seq, and none is created or dropped while it runs. Tests: `online_backups_while_commits_and_checkpoints_run`, `a_checkpoint_waits_for_the_backups_copy`, `backup_archive_and_restore_with_several_namespaces`.
- **It is complete or refused.** Every file is fsynced, then the manifest, then the marker; an interrupted backup is refused by a store, `verify` and restore, and the manifest lets `verify` find a missing, extra or truncated file. Tests: `every_write_of_a_backup_can_fail_and_leaves_nothing_valid_looking`, `damage_to_a_backup_that_its_files_cant_show_is_found_by_the_manifest`, `a_backup_must_hold_what_its_manifest_lists`.
- **A store refuses to open a backup** (`IsBackup`); it is restored instead. Tests: `backups_and_interrupted_restores_are_refused`.
- **A throttled backup is the same backup**, written at most at `max_bytes_per_second`; a checkpoint started meanwhile completes after the copy, and both are consistent. Tests: `a_checkpoint_during_a_throttled_backup_completes_and_both_are_consistent`.
- **On a server, a backup is written only into `[backup] dir`**, under a name that is one path component; nothing that exists under the name is written to or replaced, and a failed backup is removed ([ADR 0055](adr/0055-admin-writes-and-iwctl-against-a-server.md)). Tests: `backups_are_named_verified_and_never_overwrite`.

**Continuous WAL archiving** (`StoreOptions::archive`):

- **The archive and the WAL together always hold the history** from the archive's first segment: a segment leaves `wal/` only after its copy and the archive's directory are fsynced. Each namespace's segments are kept in their own directory, dropped namespaces' included. Tests: `the_archive_and_the_wal_hold_the_whole_history`, `backup_archive_and_restore_with_several_namespaces`.
- **Archiving is idempotent**, and a conflicting segment in the archive keeps the WAL's. Tests: `archiving_is_idempotent`, `a_conflicting_segment_in_the_archive_keeps_the_wal`.
- **If archiving fails, nothing is removed from `wal/`**: the WAL grows, `Store::checkpoint_failure` reports it, and the next checkpoint retries; a failed directory fsync keeps checkpoints off until reopening. Commits go on. Tests: `every_archive_write_can_fail_without_losing_a_segment`.
- **An archive belongs to one history**: a store refuses an archive of another (`ArchiveMismatch`), and two stores never write one archive. Tests: `an_archive_belongs_to_one_history`.
- **Pruning before a backup removes only what that backup makes unnecessary**: a restore from that backup (or a later one) and the archive reaches every seq it reached before; a backup of another history is refused, and an interruption leaves a valid archive. Tests: `pruning_never_removes_what_a_kept_backup_needs`, `the_archive_is_pruned_before_a_backup`, `archive_prune_before_a_backup`.

**Restore and point-in-time recovery** (`iwdb::restore`, `iwctl restore`, `iwdb.restore()` in Python):

- **A restore to seq `N`** (from a backup, an archive, or both, of one history) gives exactly the state after commits `1 ..= N`: data, versions and catalog; the next commit is `N + 1`. Tests: `pitr_from_a_backup_alone`, `pitr_from_the_archive_alone`, `pitr_from_a_backup_and_the_archive`.
- **A restore to a time `T`** gives the state after the last commit, in seq order, whose commit time is at or before `T`; with several namespaces, every namespace as of that time. Tests: `pitr_to_a_time`, `test_restore_to_a_time`.
- **A restore to the latest** brings back every namespace that existed then, each at the last seq the sources reach; a restore to a seq selects one namespace (`AmbiguousTarget` otherwise). Tests: `backup_archive_and_restore_with_several_namespaces`.
- **Every restore starts a new history**, never writes to its sources, and an interrupted restore is never opened as a store. Tests: `restores_that_must_be_refused`, `every_write_of_a_restore_can_fail_and_leaves_nothing_wrong`.

**Not guaranteed:**

- A consistent point in time across namespaces in a backup: each namespace is at its own seq.
- A restored store needs a new archive directory.

## Verify

`iwdb::verify` (`iwctl verify`, `iwdb.verify()`; [ADR 0011](adr/0011-verify.md)):

- **It never writes** anything and never creates `LOCK`. It takes a shared lock: `Locked` while a store has the directory open. Tests: `verify_takes_a_shared_lock_and_changes_nothing`, `test_verify_a_live_store_is_locked`.
- **It finds damage in any file the database writes**: every checksum, a checkpoint whose state (keys included) differs from what the WAL replays to, a WAL that doesn't reach a checkpoint, and broken invariants (edge endpoints, versions, reserved keys, value depth, indexes against a scan, constraints). Tests: `a_flipped_byte_in_any_file_is_a_problem`, `missing_and_cut_wal_files_are_problems`, `invariants_broken_behind_the_stores_back_are_problems`, `an_inconsistent_or_invalid_key_table_is_a_problem`, `a_wal_that_ends_before_the_only_checkpoint_is_a_problem`.
- **Each namespace is checked, and findings name it**, with the namespace log and the directories checked against each other. Tests: `damage_in_one_namespace_names_it`, `the_namespace_log_and_the_directories_must_agree`.
- **What a crash leaves is a note, not damage** (a torn tail, temporary files, an interrupted cleanup), and verify finds no problem exactly when recovery succeeds. Tests: `what_a_crash_leaves_is_noted_not_damage`, `an_interrupted_cleanup_is_tolerated`, `a_store_verifies_clean_and_reports_what_recovery_reaches`.
- **An open store verifies without stopping** (`Store::verify`, the `Verify` RPC): its checkpoints and WAL up to the synced seq, then the live state; it changes nothing. Tests: `an_open_store_verifies_while_it_commits_and_damage_is_found`, `the_running_store_verifies`.

**Not guaranteed:**

- An open store isn't verified while the server refuses writes for memory: its replay uses memory the limit doesn't count.

## Limits

[ADR 0054](adr/0054-the-memory-limit.md), [ADR 0059](adr/0059-resource-limits-per-client-and-namespace.md); configuration: [api/config.md](api/config.md).

**Sizes:**

- **A commit's WAL record is at most 64 MiB**; a larger one is rejected with `RecordTooLarge` before anything happens. Tests: `a_record_above_the_limit_is_rejected_before_anything_happens`.
- **Attribute values nest at most 100 levels** (`MAX_VALUE_DEPTH`), the same over every API. Tests: `values_nested_too_deep_for_the_log_are_rejected`, `values_and_filters_nested_100_levels_pass_and_101_fail_with_the_cores_message`, `test_the_depth_limit`.
- **Segments are 1 KiB to 1 GiB** (64 MiB by default); a reader holds one segment in memory at a time. Tests: `writer::options_are_checked`.

**The memory limit** (`[memory]`, `StoreOptions::memory`):

- **A refused write is never logged.** Above `refuse_writes_at` of the limit, a commit, an index or constraint creation, a namespace creation or an import fails with `resource_exhausted` before anything reaches the WAL or the namespace log, and recovery finds none of it. Tests: `a_refused_commit_is_never_logged_and_one_below_the_line_is_durable`, `a_crash_after_a_refused_commit_recovers_exactly_the_accepted_ones`.
- **A commit accepted below the line is durable** like any other. Tests: `a_refused_commit_is_never_logged_and_one_below_the_line_is_durable`.
- **One check, in the commit pipeline**: every API gets the same answer. Tests: `memory::over_grpc`, `memory::over_rest`.
- **What goes on while writes are refused**: reads, `analyze` included; commits that only remove; dropping an index, a constraint or a namespace; the system namespace (users, grants, logins); repeats of keyed commits. Tests: `removals_repeats_and_the_system_namespace_pass_while_writes_are_refused`, `deleting_brings_the_store_back_below_the_line`.
- **Hysteresis**: writes resume only once memory is 5 % of the limit below the line. Tests: `writes_resume_only_once_memory_is_below_the_band`, `the_state_rises_at_each_line_and_falls_only_below_its_band`.
- **What is counted**: the live graphs with their payloads, index builds, the checkpointers' copies, running projections and stored job results. Tests: `checkpointer_copies_and_analytics_are_counted`, `a_namespace_charges_its_graph_with_its_payloads_until_it_is_dropped`, `a_job_is_counted_while_it_runs_and_starts_while_writes_are_refused`.

**Resource limits per user and per namespace** (`[limits]`):

- **One client over its limits can't starve another.** A call over its user's rate (a token bucket) or count of running calls, or its namespace's count of running calls, is refused with `resource_exhausted` at the authorisation point, before it holds a worker, a queue slot or a namespace lock. Tests: `one_client_over_its_limits_cannot_starve_another`, `running_calls_are_limited_per_user_and_per_namespace`, `a_namespace_concurrency_limit_counts_every_user`, `rest_refuses_with_resource_exhausted`.
- **Admission is all or nothing**: a refused call takes no token and counts nowhere, so a flooding client still gets exactly its rate. Tests: `a_refusal_takes_no_token`, `the_bucket_admits_its_burst_then_its_rate`.
- **A namespace above its memory limit refuses writes and keeps serving reads**, and a refused write leaves nothing in the WAL; the limit holds after a restart. Tests: `a_namespace_at_its_memory_limit_refuses_writes_and_serves_reads`, `the_default_namespace_limit_applies_where_none_is_set`.
- **Query budgets lower what a read asks for**, and a read that reaches a budget says so. Tests: `budgets_clamp_reads_and_say_so`, `a_budget_lowers_what_is_asked_and_the_defaults`.
- **Limits are durable with the users**: records of the system namespace, one commit each, recovered, backed up and restored with them. Tests: `reads_auth_format_2`, `own_values_replace_defaults_and_zero_means_none`, `defaults_apply_to_users_who_are_not_admins`.

**Not guaranteed:**

- **The memory accounting is an estimate**, from lengths, not capacities. It doesn't count request buffers, the WAL's buffer, the allocator's slack or the runtime, so resident memory can be higher; the 10 % between the refusal line and the limit is for that.
- **One commit can cross the line**: a commit is admitted on the state before it.
- **Reads aren't limited by memory**; an `analyze` is counted and can push the store into refusing writes.
- **An embedded store has no memory limit** unless `StoreOptions::memory` sets one. Nothing is evicted to disk.
- **Over-limit calls are refused, never queued**; the client retries with backoff. CPU and network aren't limited.
- **Only remote calls are limited**: calls on an embedded store in-process aren't, and with authentication off only namespace limits apply. Limits are counted per server process.

## The server

The server ([api/grpc.md](api/grpc.md), [api/rest.md](api/rest.md)) gives the guarantees of the store it serves, through the `Database` trait, and adds:

- **gRPC and REST answer alike.** Every operation is implemented once; both run the same conformance suite. Tests: `commit_then_get_nodes_and_edges`, `errors_have_stable_codes`, `every_rpc_has_a_route`.
- **Shutdown loses nothing acknowledged.** On SIGINT/SIGTERM (on Windows Ctrl-C, Ctrl-Break, closing the console or a system shutdown) the server drains running calls up to its drain timeout, cancels the rest, lets accepted commits finish, then fsyncs every WAL and checkpoints before it exits ([ADR 0027](adr/0027-graceful-shutdown.md)). Tests: `acknowledged_commits_survive_with_fsync_off_and_no_checkpoint`, `acknowledged_commits_survive_with_group_commit_and_a_checkpoint`, `calls_still_running_at_the_end_of_the_drain_are_cancelled`.
- **Every read ends at its deadline**: the smaller of `grpc-timeout` and the request's `timeout_ms`, capped by the server's maximum ([ADR 0026](adr/0026-deadlines-over-grpc.md)). A read whose client goes away is cancelled. Tests: `grpc_timeout_bounds_a_read_and_the_smaller_timeout_wins`, `a_client_that_goes_away_stops_its_read`, `a_rest_client_that_goes_away_stops_its_read`.
- **A commit has no deadline on the server**: once accepted it runs to the end, even if its client goes away; that client retries with the same idempotency key. Tests: `a_commit_runs_to_the_end_after_its_client_went_away`.
- **Errors carry their code** ([api/errors.md](api/errors.md)): in the `iwdb-code` trailer over gRPC, and as the HTTP status and error body over REST. Tests: `errors_carry_their_code_in_the_trailer`, `errors_have_the_status_of_their_code_and_an_error_body`, `codes_have_unique_stable_names`.
- **Ready means recovered.** The server answers health while it recovers, but isn't ready (`GET /v1/health/ready` 503, `grpc.health.v1` `NOT_SERVING`), and every database call fails with `unavailable`, until recovery has applied every record in the log. A shutdown turns readiness off before it drains ([ADR 0040](adr/0040-health-and-readiness.md)). Tests: `not_ready_until_recovery_has_finished`, `ready_only_after_a_large_recovery`, `a_shutdown_turns_readiness_off_first`, `a_failed_open_never_becomes_ready`.
- **Request bodies and values are bounded**: bodies above the message limit are refused, and values, filters and patterns are read to the core's depth limits. Tests: `request_bodies_above_the_message_limit_are_refused`, `the_server_refuses_bytes_nested_101_levels_with_the_cores_message`.

**Not guaranteed:**

- A bug in a commit's apply path aborts the whole server (all namespaces), as a crash: run it under a supervisor ([ADR 0028](adr/0028-internal-apply-errors-abort.md)).

## Authentication and authorisation

[ADRs 0043 to 0047](adr/0045-the-authorisation-point.md); configuration: [api/config.md](api/config.md#authentication-and-the-first-admin). With `[auth] enabled` (the default):

- **Every database call has a principal.** Everything but health, login, the console's pages and the OpenAPI document needs a session, an API token or a client certificate; without one it fails with `unauthenticated` before any operation runs. Tests: `every_role_and_operation_over_grpc_and_rest`, `test_without_credentials_calls_are_unauthenticated`.
- **Roles are checked once, for every operation**, in front of the `Database` trait: `read`, `write` and `admin` per namespace, and a server-wide admin. A refused call changes nothing (`permission_denied`). Tests: `every_role_and_operation_over_grpc_and_rest`, `roles_include_the_ones_below`, `admins_may_do_everything_and_users_manage_themselves`.
- **User, grant and token changes are all or nothing and durable**: each is one commit to the store's reserved system namespace, recovered, backed up and restored with the rest of the store ([ADR 0043](adr/0043-users-and-roles-in-the-system-namespace.md)). Tests: `user_and_grant_changes_are_all_or_nothing`, `users_are_durable_and_their_namespace_is_hidden_and_reserved`, `backups_and_restores_carry_the_users`.
- **No password or token is stored, logged or put in an error message**: passwords as argon2id hashes, compared in constant time; tokens as SHA-256 hashes. Tests: `no_secret_reaches_the_logs`, `hashes_record_their_parameters_and_verify`, `tokens_are_256_random_bits_and_stored_as_hashes`, `secrets_never_print`, `errors_never_echo_a_token`.
- **No default password**: a server with authentication on and no users refuses to start. Tests: `a_store_without_users_refuses_to_start`.
- **Sessions end** at their lifetime, at logout, when the user's password changes, when the user is deleted, and when the server restarts. API tokens end when revoked, when they expire, or with their user. A grant ends with its namespace. Tests: `logins_give_sessions_that_end`, `sessions_expire_and_are_bounded`, `api_tokens_authenticate_until_revoked_or_expired`, `grants_end_with_their_namespace`, `test_an_expired_session_is_unauthenticated`.
- **Failed logins are slowed down**: at most `login_max_failures` per user and per client address within `login_window_secs`. Tests: `failed_logins_are_slowed_down_per_user_and_address`, `the_throttle_refuses_after_too_many_failures_and_forgets`.

**Not guaranteed:**

- **No secrecy on the wire with TLS off** (below): passwords, tokens and data cross the network in clear.
- **No protection of the data directory.** The embedded store, Python's `Store.open` and `iwctl` on a data directory are unauthenticated: whoever can open the directory owns the store, users included. Its file permissions are the boundary, and a backup holds the password hashes.
- **The login slowdown is per process**, forgotten at a restart, and can lock a known user out for one window at a time.
- **A restore brings back the users of its point in time**, passwords included.

## TLS and mTLS

[ADR 0048](adr/0048-tls-and-mtls.md); configuration: [api/config.md](api/config.md#tls-and-mtls).

- **A default server speaks only TLS.** Without `[tls] enabled = false` it serves nothing in plaintext, and without a certificate and key it doesn't start; plaintext on a non-loopback address also needs `[server] plaintext_public = true`. Tests: `plaintext_needs_explicit_flags_and_tls_needs_a_certificate`, `a_plaintext_client_gets_no_answer_from_a_tls_server`, `grpc_and_rest_speak_tls_on_one_port`.
- **Clients verify the server**: the Rust and Python clients, `iwctl` and the console's `serve.py` refuse a certificate of a CA they don't trust and an expired one (`unavailable`, with the reason). Tests: `a_server_of_another_ca_is_refused`, `an_expired_server_certificate_is_refused`, `test_a_server_of_another_ca_is_unavailable`, `test_a_server_over_tls_is_verified_against_the_ca`.
- **A client certificate is verified before it counts**: an expired one or one of another CA fails the handshake, and nothing is served on that connection. Tests: `client_certificates_that_are_expired_or_of_another_ca_are_refused`.
- **A client certificate authenticates as the user its subject's common name names**, with that user's roles; one that names no user, or not exactly one common name, is `unauthenticated`. A token or session in the request wins over the certificate. Tests: `a_client_certificate_is_its_users_principal_with_its_roles`, `certificates_that_name_no_user_are_refused`, `without_a_client_ca_certificates_authenticate_nobody`, `test_a_client_certificate_logs_in_as_its_user`.
- **With `[tls] client_auth = "required"`, every request without a client certificate is refused**; only health and the console's pages are answered. Tests: `a_required_client_certificate_leaves_only_health_and_the_pages_open`, `an_optional_client_certificate_is_optional`.
- **A REST write authenticated by a client certificate alone needs the CSRF header**, as one by the console's cookie does. Tests: `a_rest_write_by_certificate_needs_the_csrf_header`, `the_console_session_is_a_cookie_with_a_csrf_header`.
- **The console's session cookie is `Secure` over TLS** (and `HttpOnly`, `SameSite=Strict`). Tests: `the_session_cookie_is_secure_over_tls`.
- **SIGHUP reloads the certificate, key and client CA** for new connections (Unix); a failed reload keeps the ones in use. Tests: `sighup_reloads_the_certificate`, `a_reload_swaps_the_certificate`.
- **No private key is logged, printed or put in an error message.** Tests: `no_secret_reaches_the_logs`, `errors_never_quote_a_private_key`.
- **No certificate or key is in the Docker image.** Tests: `ci.yml#docker`.

**Not guaranteed:**

- **The probe doesn't verify the server's certificate** (`iwdb-server --probe`, the image's `HEALTHCHECK`): it sends no credentials and reads only readiness.
- **No certificate management**: nothing renews a certificate or checks revocation (no CRL or OCSP). `docker/dev-cert.sh`'s certificates are for development only.
- **Open connections keep their certificate** after a reload, until they close. Windows has no reload: restart the server.
- **The Postgres source of projections connects without TLS.**

**Notes:**

- The console keeps no credential in browser storage: its session is the cookie. It remembers only the theme in `localStorage`.

## Audit log

[ADR 0049](adr/0049-audit-log.md); configuration: [api/config.md](api/config.md#audit-log).

- **Every login, logout, user, grant, token, namespace and catalog change leaves one entry**, with its principal (and how it authenticated), the client's address, the operation, the namespace and the outcome; so does every refusal (`unauthenticated`, `permission_denied`) of any operation. All are made at the authorisation point, over gRPC and REST alike. Tests: `every_role_and_operation_over_grpc_and_rest`, `certificate_principals_are_audited`.
- **No secret or data value in an entry**: never a password, a token or its hash, a certificate, an error message or an attribute value. Tests: `no_secret_reaches_the_logs`, `only_valid_user_names_are_audited_as_typed`.
- **Bounded files**: with `[audit] dir`, files older than `[audit] retention_days` are deleted. Tests: `old_files_are_deleted_and_others_kept`, `retention_zero_keeps_every_file`.
- **No telemetry**: without projections the server opens no connection of its own ([SECURITY.md](../SECURITY.md)). Tests: `opens_no_connection_it_was_not_configured_for`.

**Not guaranteed:**

- **Entries are not durable with the change**: written after the outcome, without fsync, so a crash can lose the last ones (never the changes, which are in the WAL). A call whose client goes away before it finishes can leave no entry.
- **User, grant and token changes carry no seq** (catalog changes and namespace events do).
- **Successful reads and data commits aren't audited** (the change stream records commits), nor failed TLS handshakes, nor in-process access to a data directory.
- **A log level that names `iwdb::audit` can hide entries**; entries in stderr are kept as long as the log collector keeps them.

## Metrics, status views and cancel

[ADRs 0050 to 0052](adr/0051-the-status-views.md); [api/metrics.md](api/metrics.md), [api/rest.md](api/rest.md#operator-reads).

- **Every metric is documented, and every documented one exported**, with bounded labels (operation, outcome code, lock kind, live namespace, version; per-user series only within a bound). Tests: `metrics_hold_every_metric`, `serves_the_metrics_to_prometheus`, `user_series_are_bounded`.
- **Every status read is bounded** and answers the same over gRPC and REST. Tests: `admin_reads_are_bounded`, `schema_is_bounded`, `server_status_reports_the_database`.
- **The operator's reads don't wait** for a namespace lock or a worker. Tests: `the_operators_reads_answer_while_every_worker_is_busy`.
- **A cancelled read ends with `cancelled`**, unless its answer was ready first; a cancel of a request that ended is `not_found`; commits and other changes can't be cancelled. Users cancel only their own requests, server admins anyone's. Tests: `a_running_request_is_listed_and_cancelled`, `a_cancelled_call_answers_cancelled_and_drops_its_future`, `an_answer_that_is_ready_wins_over_a_cancel`, `changes_can_not_be_cancelled`, `requests_and_cancel`.
- **The log tail holds what the log holds, no more**, and only server admins read it. Tests: `no_secret_reaches_the_logs`, `the_ring_keeps_the_last_events`.
- **Pulled, never pushed**: `/metrics` needs credentials like any route, and the server sends metrics and logs nowhere. Tests: `opens_no_connection_it_was_not_configured_for`.

**Not guaranteed:**

- **The status isn't one consistent cut across namespaces**: each namespace's part is consistent on its own.
- **Counts start when the database starts serving**; calls on an embedded store in-process aren't counted or listed.
- **A running request's visited count isn't reported.**
- **The schema's attribute keys are sampled** ([ADR 0053](adr/0053-the-schema-read.md)) from the first `max_visited` nodes; labels and edge types are complete with exact counts up to 10 000 of each.
- **The log tail is in memory**: a restart empties it.

## Managed analytics jobs

[ADR 0056](adr/0056-managed-analytics-jobs.md); [api/rest.md](api/rest.md#managed-jobs), [api/config.md](api/config.md#managed-jobs).

- **A job outlives any request's timeout**: it runs until it ends, its own timeout, a cancel or the drain, and its result can be fetched in bounded pages until it expires. Tests: `a_job_outlives_the_request_timeout_over_grpc`, `a_job_outlives_the_request_timeout_over_rest`, `a_job_runs_to_the_end_and_its_result_is_paged`.
- **The same answer as `analyze`.** Tests: `a_job_runs_to_the_end_and_its_result_is_paged`.
- **Jobs never take a query worker.** Tests: `jobs_run_on_the_workers_and_their_results_arrive`, `a_job_outlives_the_request_timeout_over_grpc`.
- **Every bound has a cap and a defined answer**: a full queue or too many jobs per user is `unavailable`; a projection too large is `budget_exceeded`; ended jobs and stored results are kept by time, count and bytes. Tests: `too_many_jobs_are_refused_with_unavailable`, `queues_and_users_are_capped`, `results_are_kept_by_time_count_and_bytes`, `jobs_are_checked_and_bounded`.
- **A cancel takes effect at once**, and a queued job never starts. Tests: `a_cancel_takes_effect_at_once_and_reaches_the_work`, `a_job_is_cancelled_by_its_id_or_as_a_request`.
- **Who may**: starting needs `read` on the namespace; a job is seen, cancelled and fetched only by its owner and server admins; fetching needs `read` still. Tests: `a_job_result_needs_read_on_the_namespace_still`, `every_role_and_operation_over_grpc_and_rest`.
- **Jobs are counted in the memory limit** while they run and while their results are kept. Tests: `a_job_is_counted_while_it_runs_and_starts_while_writes_are_refused`.
- **The drain cancels every job** and still ends in time. Tests: `a_drain_cancels_jobs_and_still_ends_in_time`, `a_drain_cancels_every_job_and_refuses_new_ones`.

**Not guaranteed:**

- **Progress is the core's report** per phase, not a time estimate.
- **Nothing persists**: a restart or a drain loses every job and result, and ids restart at 1.
- **Expiry is checked when the registry is used**, not by a timer.

## Traces

[ADR 0057](adr/0057-traces.md); [api/config.md](api/config.md#traces) (`[tracing]`, feature `otel`). The span tree is in ADR 0057.

- **One trace per request**, with the attributes of ADR 0057, over gRPC and REST alike. Tests: `a_grpc_request_is_one_trace_under_its_callers_span`, `a_rest_request_is_one_trace_under_its_callers_span`, `a_write_shows_its_commit_append_and_fsync_per_policy`, `a_failed_request_records_its_code_and_an_error_status`, `a_job_is_a_trace_linked_to_its_start`.
- **The caller's trace is followed**: a valid `traceparent` is the parent, a caller that didn't sample isn't sampled, and a malformed one starts a new root without failing the request. Tests: `a_parent_that_did_not_sample_is_followed`, `a_malformed_or_missing_traceparent_starts_a_new_root`.
- **Never in a span**: an attribute value, a filter, a pattern, an id, a mutation, an error message, a user, a client address, a token or a password. Tests: `no_span_name_holds_an_id_or_a_value`.
- **The collector can't slow or fail a request**: spans go onto a bounded queue, and a full queue or a failed export drops spans, counted. Tests: `a_collector_that_is_down_slows_and_fails_no_request`.
- **Shutdown sends what is queued**, within the drain's deadline. Tests: `the_shutdown_sends_the_queued_spans_to_the_collector`.
- **Without the `otel` feature no OpenTelemetry crate is built in**, and `[tracing] enabled = true` is refused. Tests: `ci.yml#rust`.

**Not guaranteed:**

- **Spans are best effort**: dropped, failed or cut-off exports are gone, without retries; sampling leaves out whole traces.
- **Requests refused before an operation runs** have no span; the audit log and `iwdb_requests_total` count them.
- **No span inside the core's algorithms** or per record; the group-commit timer and recovery aren't traced; nor is the embedded library. Log lines don't carry trace ids.

## The change stream

[api/changes.md](api/changes.md), [ADR 0031](adr/0031-change-stream.md).

- **Only durable commits**: up to the lower of the applied and the synced seq (the applied seq under `off`), so no crash takes back a commit it returned. Tests: `an_os_crash_under_group_commit_takes_back_no_streamed_commit`, `the_streamable_seq_waits_for_the_group_fsync`, `off_streams_what_is_applied`.
- **In order, without gaps**: a consumer that resumes from the seq after its last one sees every commit once, across restarts. Tests: `a_consumer_resumes_after_restarts_without_gaps_or_duplicates`, `changes_resume_in_batches_without_gaps`, `server_sent_events_resume_after_the_last_event_id`.
- **As long as the WAL holds it**: older seqs fail with `not_retained`, and damage is `corrupt`, never skipped. Tests: `seqs_before_the_first_segment_are_not_retained`, `damage_inside_the_range_is_corruption_and_reads_before_it_work`, `retention_keeps_segments_that_checkpoints_no_longer_need`.

**Not guaranteed:**

- Under `group` the stream lags behind acknowledged commits by up to `2 × max_delay`.

## Projection mode

[api/projections.md](api/projections.md), [ADR 0032](adr/0032-projection-mode.md).

- **Exactly once across crashes**: a projection moves its mark in the commit that applies its events and resumes from it, so every event is applied once. Tests: `a_projection_survives_crashes_without_applying_an_event_twice`, `a_table_is_projected_with_rules_and_resumed_after_a_restart`.
- **An OS crash under `group` loses events and their marks together.** Tests: `an_os_crash_under_group_commit_loses_events_and_their_marks_together`.
- **One writer per mark**: a commit with a stale mark fails with `conflict` and changes nothing. Tests: `two_projectors_with_one_name_apply_each_event_once`, `a_commit_that_fails_moves_no_mark`.

**Not guaranteed:**

- **Postgres holes** are waited for up to `gap_timeout`, then skipped: an event whose transaction commits later than that is missed (`holes_are_waited_for_then_skipped`). Set the timeout above your longest writing transaction.

## Import and export

[api/import-export.md](api/import-export.md), [ADR 0033](adr/0033-bulk-import-export.md).

- **An import is all or nothing across crashes**: the namespace is there with all the file's data, or not at all. Tests: `every_file_operation_of_an_import_can_fail`.
- **It is checked before anything is written**: a file that is invalid or breaks an invariant creates nothing (`invalid_argument`). Tests: `bad_imports_create_nothing`, `test_lgf_files_and_errors`.
- **An imported namespace restores from the archive alone**: a store with an archive copies the import's checkpoint there before the import returns, and on every open if it is missing. Tests: `an_imported_namespace_restores_from_a_backup_and_from_the_archive_alone`, `an_open_archives_the_checkpoint_of_an_import`.
- **Merges are ordinary commits, in batches**: a failure leaves the batches before it committed. Tests: `a_merge_commits_in_batches_and_stops_at_a_failing_one`, `a_merge_upserts_through_commits_and_converges`.
- **An export imports back** to the same graph, and re-exported, to the same bytes. Tests: `an_import_creates_its_namespace_from_one_checkpoint_and_an_export_imports_back`, `test_an_export_imports_back`.

**Not guaranteed:**

- The change stream of an imported namespace starts at seq 2. Without an archive, take a backup after an import.
- Versions restart at 1 in an export; constraints, idempotency keys and marks are not exported. Commits to the namespace wait while an export writes.

## Python

The Python bindings ([python-api.md](python-api.md), [ADR 0013](adr/0013-python-bindings.md)) give the store's guarantees, and the remote client the server's ([ADR 0035](adr/0035-python-remote-client.md)):

- **A transaction is one commit**, all or nothing; nothing is committed when its `with` block raises. Tests: `test_a_transaction_commits_at_the_end_of_its_block`, `test_an_exception_in_the_block_commits_nothing`.
- **Idempotency keys and `min_seq` work as in Rust**, with the GIL released while waiting. Tests: `test_a_retry_with_the_same_key_returns_the_original_result`, `test_min_seq_reads_wait_for_the_commit`, `test_readers_run_while_a_writer_commits`.
- **A panic in the bindings is an exception** (`iwdb.InternalError`); one in the commit path aborts the interpreter, a crash the next open recovers. Tests: `test_a_panic_in_the_bindings_is_an_exception`, `test_kill_9_loses_no_acknowledged_commit`.
- **Errors keep their class over the network.** Tests: `test_errors_keep_their_class_over_the_network`, `test_every_exception_is_an_iwdb_error`.

**Not guaranteed:**

- A child forked without exec inherits the store's lock and must not use the store.

## Compatibility between versions

- **Every format is versioned** (magic bytes, a version, checksums; design rule 4), and a version reads at least the formats of the version before it. Each format version has a fixture that every later version must read. Tests: `the_v4_fixture_reads_as_its_records_keys_marks_and_times`, `the_v5_fixture_opens_with_all_its_namespaces_and_marks`, `the_v2_backup_verifies_its_manifest_reads_and_it_restores`, `the_v3_archive_verifies_and_restores`, `reads_auth_format_1`, `reads_auth_format_2`.
- **What a release wrote, a later version opens.** Every release leaves a fixture, `crates/iwdb/tests/fixtures/releases/<version>/`: a data directory with a WAL tail in every namespace, its backup and its WAL archive. Every later version must open the store with the same namespaces, keys, marks and users, go on writing, and restore the backup and the archive to the states recorded with it. Tests: `every_release_fixture_opens_in_this_version`, `this_version_writes_a_release_fixture_that_opens`.
- **A log of an older format continues in the current one.** Tests: `a_v1_log_continues_in_the_current_format`, `a_v3_log_continues_with_marked_records`.

**Not guaranteed:**

- Downgrades: an older version can't open what a newer one wrote; it refuses it (`a_newer_layout_or_a_damaged_marker_is_refused`).
- What may change in the wire contract (`proto/`) between versions: `buf breaking` checks it in CI (`ci.yml#buf`), and the versioning policy comes with the release pipeline ([step 17e](steps/step_17e.md)).

## Platforms

Linux, macOS and Windows ([ADR 0058](adr/0058-windows.md)), with the guarantees of this page on each.

- **Every platform runs the same checks**: the workspace tests, the short crash run and the Python suite on every push, and the long crash run nightly. Tests: `ci.yml#rust`, `ci.yml#crash`, `ci.yml#python`, `nightly.yml#crash`.
- **Files a reader holds open are removed and the reader finishes**, on Windows too (`FILE_SHARE_DELETE`). Tests: `files_that_readers_hold_open_are_removed_and_the_readers_finish`.
- **A killed process releases the lock** (`flock` on Unix, `LockFileEx` on Windows). Tests: `the_lock_is_released_when_the_process_dies`.
- **A data directory where a directory can't be synced refuses to open** (on Windows a network share). Tests: `data_directories_at_unusual_paths`.

**Not guaranteed:**

- **File systems**: local ones only (on Windows NTFS or ReFS). FAT32, exFAT and network file systems (NFS, SMB) make no promise about directory syncs.
- **Windows ends a process within a few seconds** of a console close or a system shutdown: a drain cut short there is a kill, which recovery handles.
- **File permissions**: the data directory's files and the audit files get the permissions or inherited ACL of where they are created (`0600` / `0700` for the audit files on Unix). Put them where only the server's account can read.
- **No Docker image for Windows**, no Windows service or installer.

**Notes:**

- On Windows an abort ends a process with exit code `0xC0000409` (`__fastfail`), and the crash harness kills with `TerminateProcess`; disk free space is `GetDiskFreeSpaceExW`'s bytes available to the caller.
