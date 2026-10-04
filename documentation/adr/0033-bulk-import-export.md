# ADR 0033: Bulk import creates a namespace from one checkpoint

Status: accepted
Date: 2026-10-04

## Context

Step 13, part 3: load a large graph into the database, and get a namespace's graph out, without going through millions of commits. The task named five formats. Parquet was dropped first (it would bring the `arrow`/`parquet` dependency tree for one import format), then CSV edge lists and GraphML (decision of 2026-10-04: keep it small). What is left:

- **The core's files** (version 2), JSON and binary, as written by the Ironweaver library and by `ironweaver_core::format`. The core loads them (`LoadGraph`), including version 1 JSON files, which it migrates. Since core `d15a7ec` (upstream #54) it refuses version 1 binary files with a message saying how to convert them. Binary files load from a reader, building the graph while decoding; JSON files are parsed from one buffer.
- **LGF**, the LEMON Graph Format: a text format of sections (`@nodes`, `@arcs`, `@edges`, `@attributes`), each a header line with column names and then one row per item, the tokens separated by whitespace, quoted with `"` and C escapes where needed. The `label` column of `@nodes` names the node, and the rows of `@arcs` and `@edges` start with the labels of their two ends. Its values are untyped. We parse it ourselves: it isn't the core's file format.

What we have to fit into:

- A namespace exists from the moment its create event is in the namespace log, and its directory is made before the event (ADR 0017). Recovery removes a namespace directory the log doesn't list, but refuses to open (`NamespaceDamaged`) if such a directory holds data (a checkpoint or WAL records), because that means the log lost an acknowledged create event.
- A namespace's state is its newest checkpoint plus the WAL after it, and a checkpoint at seq N needs no WAL before N + 1. The change stream reports seqs that aren't in the WAL any more as `not_retained` (ADR 0031).
- Rule 2: no write path that bypasses the WAL for a live namespace. Rule 4: format changes are versioned.

## Decision

**1. Import creates a new namespace.** `Store::import_namespace(name, format, reader, progress)` reads the file into a graph, checks it, and creates the namespace `name` with that graph as its state at **seq 1**: one checkpoint, no WAL record. Seq 1 rather than 0, so that a change stream consumer asking for seq 1 gets `not_retained` instead of silently missing the imported data. The namespace must not exist (`NamespaceExists`), so there is no write next to the WAL of a live namespace: an import is a namespace created with content. Importing into an existing namespace (merging) is not supported; neither is an idempotency key (a retry finds the namespace there, or not).

**2. Crash safety, all or nothing.**

1. The checkpoint is written, without holding any lock, to a temporary file in `ns/` (`import-*.tmp`) and fsynced. A crash leaves a `.tmp` file, which the next open removes.
2. Holding the namespace log (as a create does, so no other create, drop or backup runs meanwhile): the namespace directory is made (as for a create); the file is moved into it as `checkpoints/import.staged`, and the directories are synced. A `.staged` file is not a checkpoint, so a crash here leaves a directory without data, which the next open removes.
3. The create event is appended and fsynced: **the commit point**.
4. `import.staged` is renamed to the checkpoint `00000000000000000001.ckpt`, and the directory is synced; then the namespace opens and starts its WAL at seq 2.

**Recovery finishes an import** whose event was logged: in a listed namespace, an `import.staged` file is renamed to checkpoint 1 if the namespace has no checkpoint, and removed otherwise. So after a crash, the namespace is there with all its data or isn't there at all. This is part of data-dir layout 5 (introduced in this step and not released yet, so no version bump): `verify` accepts an `import.staged` file in a listed namespace (a pending import), and layout 4 readers never meet one.

**3. What an import accepts.** `ImportFormat::{Json, Binary, Lgf}`, and `ImportFormat::detect` from the first bytes (the binary magic `IRONWEAV`; `{` for JSON; for LGF, a first line that isn't a comment and starts with `@`).

- **Core files**: nodes with their labels, attributes and meta; edges with their ids, types, attributes and meta; the next edge id; the indexes the file lists, which become the namespace's catalog indexes. Every node and edge gets version 1. A file is a plain graph file: a reserved key (`iwdb.*`) in an attribute or meta is refused, so a checkpoint of the database is not a valid import. Graph meta has no place in a namespace: its keys are dropped and listed in the report.
- **LGF**: `@nodes` rows become nodes, named by their `label` column (required, unique); `@arcs` and `@edges` rows become edges from the first node to the second (the database's edges are directed, so an undirected `@edges` row is one edge, in the order written). Other columns become attributes. An arc's `label` column stays an attribute `label`: edge ids are the database's. Nodes get no labels and edges no type. Values: an unquoted token that is an integer is an `Int`, one that is a number with a digit is a `Float`, anything else (and every quoted token) is a `String`; escapes as in LEMON (`\"`, `\\`, `\n`, `\t`, ...). Section names are ignored. `@attributes` (graph-level) are dropped and listed in the report. `@red_nodes` and `@blue_nodes` are refused.
- After reading, the namespace is checked like a recovered one (`invariants::check`: nesting depth, reserved keys, indexes), and refused with `InvalidImport` if anything is wrong. Errors name what they concern (a line of an LGF file, a node, a key).

Memory: the graph, plus a buffer for binary and LGF files; JSON files are read into memory whole first.

**4. Export writes a core file.** `Ns::export(out, format, progress)`, `ExportFormat::{Json, Binary}`: the namespace's graph at its current seq as a plain version 2 file, which the Ironweaver library and the import read. It holds the nodes, edges (with ids), labels, types, attributes, user meta and indexes, sorted like a checkpoint; versions, constraints, idempotency keys and marks are not in it. It holds the namespace's read lock while it writes, so commits to that namespace wait (reads don't). There is no LGF export.

**5. Progress.** Import and export take an optional callback, called with the phase (reading, writing) and the bytes read or written so far, about every 4 MiB and at the end of each phase.

**6. Where it is available.** `Store` and `Ns` in Rust (plus `import_file` / `export_file` for paths, which write the export atomically), Python (`Store.import_namespace`, `Namespace.export`), and `iwctl import` / `iwctl export` on a local data directory. Not over the `Database` trait (gRPC, REST): that needs an upload and a download stream, and the server reading or writing files of its own machine is a question for authentication (step 15). Like backup and restore, these are operations on a store, not requests.

## Consequences

- An import of millions of nodes costs one file write, not millions of WAL records, and is atomic.
- **Backups and PITR.** The import's data is in a checkpoint only, not in the WAL, so the WAL archive doesn't have it. A restore from a backup taken before the import plus the archive can't rebuild the namespace: it fails with missing records once the archive has later segments of it, and gives an empty namespace while it has none. Take a backup after an import (documented in guarantees.md and the import's docs). A backup taken after it restores it as usual.
- The change stream of an imported namespace starts at seq 2: seq 1 is `not_retained`.
- Export blocks commits to the namespace while it runs (a few seconds for a graph of millions of nodes). A consumer that needs a live namespace untouched can export from a restored backup instead.
- LGF values are typed by their look: a string attribute `"007"` written unquoted comes back as `Int(7)`.
- Not in this step: import into an existing namespace, remote import and export, CSV, GraphML, Parquet, LGF export.
