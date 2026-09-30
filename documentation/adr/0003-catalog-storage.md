# ADR 0003: Store a namespace's catalog in the graph meta of its file

Status: accepted
Date: 2026-09-30

## Context

The catalog (step 2) describes each namespace: its property indexes (by attribute path) and its constraints (unique or required, per label and attribute path). It has to survive restarts, and after recovery it must describe exactly the data it is recovered with. For example, a unique constraint added at `seq` 100 must not be applied to a checkpoint taken at `seq` 90, or be missing from one taken at `seq` 110.

Two places were considered:

- **Graph meta of the namespace's file.** The core's file format has a graph-level `meta` map (`Attrs`, a map of `Value`), which the database already owns (`iwdb.*` keys). Step 5 writes checkpoints with `format::write_atomic` and puts `iwdb.seq` in this map.
- **A sidecar file** next to the checkpoint (e.g. `catalog.json`).

Relevant facts:

- `write_atomic` replaces one file atomically (temp file, fsync, rename, directory fsync). Anything in that file is consistent with everything else in it.
- Catalog changes are WAL records (step 3 and step 4), replayed in `seq` order like data changes. So a checkpoint at `seq` S needs the catalog as of S, no more and no less.
- Since `a14149e` the core also writes property index definitions into the file (`metadata.indexes`) and recreates them, empty and unflushed, on load. That is a second, partial record of the indexes, taken from the in-memory graph.
- Step 9 gives each namespace its own graph and checkpoint.

## Decision

1. **A namespace's catalog is stored in the graph meta of that namespace's file**, under the reserved key `iwdb.catalog`. It is written and replaced together with the data, by the same `write_atomic`, and has the same `seq` as the data (`iwdb.seq`, step 5).
2. **Encoding: a `Value::String` holding a JSON document**, versioned independently of the file format:

   ```json
   {"format":1,"namespace":"social",
    "indexes":[{"path":["address","city"]},{"path":["age"]}],
    "constraints":[{"kind":"unique","label":"Person","path":["email"]}]}
   ```

   Indexes and constraints are sets, written sorted, and `serde_json` writes struct fields in declaration order, so equal catalogs give equal bytes and the file stays deterministic. The document is read in two passes: first only `format` (an unknown format is reported as such), then the whole document with unknown fields rejected. Every definition is validated while it is deserialized (`AttrPath`, `Label`, `NamespaceName` validate in `TryFrom`), so an invalid catalog is a typed `CatalogError`, never a panic.
3. **The graph meta holds nothing else** but database keys. Loading rejects a missing catalog, unknown `iwdb.*` keys (a newer writer) and non-reserved keys (not a database file).
4. **The catalog is the source of truth for indexes.** After loading, `NamespaceCatalog::apply_indexes` drops every index the file recreated that the catalog doesn't list, builds every index the catalog requires that the file didn't have (declared indexes plus the paths of unique constraints), then flushes. The differences are returned (`IndexChanges`) so that recovery and `verify` (step 7) can log or report them; for files the database wrote itself they are empty. `metadata.indexes` is only a hint the core uses; we never read it as the definition.
5. The store-wide `Catalog` (all namespaces) is an in-memory view assembled from the namespaces' catalogs. How a store tracks which namespaces exist between checkpoints is decided with the WAL layout in step 9.

## Consequences

- Catalog and data are consistent at one `seq` for free: no second file, no ordering between two renames, no recovery case where one was replaced and the other wasn't.
- The JSON-in-a-string encoding is readable in JSON checkpoints and with the core's own loaders (`format::from_binary` shows it as a string), costs one small dependency (`serde_json`, already used for tests) in `iwdb-engine`, and reuses the catalog's serde definitions, which REST (step 12) needs anyway. It is not a structured `Value::Dict`, so the core's tools see it as opaque text. Changing the document shape means bumping `CATALOG_FORMAT` and keeping a reader for the previous version (design rule 4).
- The catalog is rewritten with every checkpoint. It is small (a few definitions per namespace), so that's negligible.
- Reading the catalog needs the graph meta, which the streaming loader returns only after the whole graph is built. So catalog errors in a streamed file surface after the build. That is acceptable: loads are all-or-nothing.
- A file written by another tool, or a plain `Record` graph, doesn't load as a database file, because the catalog and versions are missing. That is intended.
- An index that the core saved but the catalog doesn't list is dropped silently apart from `IndexChanges`. If the two ever disagree for a file the database wrote, that's a bug. `verify` (step 7) should flag non-empty `IndexChanges`.
