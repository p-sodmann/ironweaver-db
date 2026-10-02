# Step 13: Change stream, projection mode and bulk import/export

Status: todo
Milestone: M3 Network access
Depends on: step 12

## Goal

Other systems can follow every change, and Ironweaver DB can act as a durable projection of an external event log.

## Tasks

- [ ] First: run the [upstream check](upstream-check.md). Issues this step depends on: #46 (JSON export of NaN and infinities). #26 and #31 are fixed as of `3b15149`.
- [ ] Watch/change stream from any retained `seq` (like etcd Watch / CouchDB `_changes`): gRPC server-streaming and SSE, backed by the WAL; clear error when the `seq` is no longer retained. Retention policy configurable.
- [ ] Projection mode: pluggable source for an external ordered log (first: a Postgres table with a monotonically increasing id), mapping config from events to mutations, high-water mark committed in the same transaction as the changes.
- [ ] Bulk import/export: ironweaver JSON/binary files, LGF, CSV/Parquet edge lists, GraphML; streaming with progress; import produces one consistent checkpoint instead of millions of WAL records.

## Acceptance criteria

- A consumer resumes the change stream after a restart without gaps or duplicates.
- Projection mode survives crashes without applying an event twice.
