# Import and export

An **import** creates a namespace from a graph file: the file's graph becomes the namespace's state at seq 1, written as one checkpoint, with no WAL records. A **merge** imports a file into an existing namespace (`default` too) through ordinary commits. An **export** writes a namespace's graph as a core file, which the Ironweaver library and the import read. Design: [ADR 0033](../adr/0033-bulk-import-export.md).

| | Rust | Python | iwctl |
|---|---|---|---|
| import | `Store::import_namespace(name, format, reader, progress)`, `Store::import_file(name, path, format, progress)` | `store.import_namespace(name, path, format=None)` | `iwctl import <dir> <name> <file> [--format f]` |
| merge | `Ns::import(format, reader, progress)`, `Ns::import_file(path, format, progress)` | `store.import_file(path, format=None)`, `namespace.import_file(...)` | `iwctl import <dir> <name> <file> --merge` |
| export | `Ns::export(out, format, progress)`, `Ns::export_file(path, format, progress)` | `store.export(path, format=None)`, `namespace.export(...)` | `iwctl export <dir> <file> [-n ns] [--format f]` |

Not over gRPC or REST: that needs an upload and a download stream (a later step).

## Formats

| Format | Import | Export | Detected by |
|---|---|---|---|
| `json`: the core's JSON file (version 2; version 1 is migrated by the core) | yes | yes | first non-whitespace byte `{` |
| `binary`: the core's binary file (version 2; version 1 is refused with a message saying how to convert it) | yes | yes | the magic `IRONWEAV` |
| `lgf`: the LEMON Graph Format | yes | no | first line that isn't empty or a comment starts with `@` |

When no format is given, an import detects it from the file's first bytes; an export writes JSON for a path ending in `.json` and binary otherwise.

### Core files

Nodes keep their ids, labels, attributes and meta; edges their ids, types, endpoints, attributes and meta; the file's next edge id is kept. The indexes the file declares become the namespace's catalog indexes. Every node and edge is at version 1. A key starting with `iwdb.` (reserved by the database) anywhere in the file is refused, so a database checkpoint is not an import. The file's graph meta has no place in a namespace: it is left out and listed in the report (`dropped`).

An export holds the nodes, edges (with their ids), labels, types, attributes, user meta and indexes of the namespace at its current seq, sorted like a checkpoint and without a timestamp, so equal graphs export to equal bytes. Versions, constraints, idempotency keys and marks are not in it.

### LGF

```text
@nodes
label   coordinates  size   title
0       (10,20)      10     "First node"
1       (80,80)      8      "Second node"
@arcs
        capacity
0   1   10
@attributes
caption "LEMON test digraph"
```

- `@nodes`: a header line naming the columns, then one row per node. The `label` column (required, unique, not empty) is the node's id; the other columns are its attributes. Nodes get no labels.
- `@arcs` and `@edges`: a header line naming the columns after the two ends, then one row per edge: source label, target label, values. Each row is one directed edge from the first node to the second (`@edges` rows too, in the order written). The columns are attributes, `label` included (edge ids are the database's). Edges get no type.
- `@attributes` (graph-level) are left out and listed in the report. `@red_nodes` and `@blue_nodes` are refused. Section names (`@nodes cities`) are ignored.
- Tokens are separated by whitespace; a token in double quotes may hold whitespace. Escapes, in and outside quotes: `\\ \" \' \? \a \b \f \n \r \t \v`, `\x` with hex digits, up to three octal digits. Empty lines and lines starting with `#` are skipped, so a header line can't be empty: a section of edges without values has the header `label`, as LEMON writes it.
- **Values are typed by their look**: an unquoted token that parses as a 64-bit integer is an `Int`; one with a digit that parses as a number is a `Float`; anything else, and every quoted token, is a `String`. A string `007` written unquoted comes back as `Int(7)`: quote it.
- Errors name the line: `line 6: no node 'b'`.

## Merging into an existing namespace

A merge reads the file like an import, then commits it through the commit pipeline, in batches of up to 10 000 mutations, nodes before edges:

- **Nodes** are upserted: the file's attributes and meta replace the node's, its labels are added (labels the node has stay).
- **Edges** are upserted by their ends and type: an existing edge from the same node to the same node with the same type gets the file's attributes and meta; otherwise one is added. Edges of the file that share their ends and type (parallel edges) are added, each time. The file's edge ids are not kept. Where the namespace has parallel edges an edge of the file would update, the batch fails (`AmbiguousEdge`).
- **Indexes** the file declares that the namespace lacks are created first.
- Each batch is an ordinary commit: in the WAL, the change stream and the archive, constraints checked, versions bumped. **A failure stops the merge**, with the batches before it committed; running it again converges, apart from parallel edges, which are added again.

The report has the counts, the indexes created, what was left out, the number of commits and the seqs of the first and the last. Progress: the bytes read, then the mutations committed (`Phase::Committing`).

## Guarantees

- **All or nothing.** After a crash at any point, the namespace is there with all the file's data, or not at all. The import's checkpoint is staged before the namespace's create event is logged, and the next open finishes a staged import whose event is logged ([data-dir.md](../formats/data-dir.md), "Import").
- **Checked first.** The graph is checked like a recovered namespace before anything is written; a refused file creates nothing.
- **A new namespace.** The name must not exist (`conflict`); to import into an existing one, merge.
- **Not in the WAL.** The namespace's change stream starts at seq 2 (seq 1 is `not_retained`), and the WAL archive doesn't hold the import. **Take a backup after an import**: a restore from an older backup plus the archive can't rebuild the namespace.
- **Export** reads the namespace at one seq; commits to that namespace wait while it writes (reads don't). `export_file` writes a temporary file, fsyncs it and renames it over the path.

Memory: the graph, plus the whole file for a JSON import (binary and LGF files are decoded as they are read) and the whole output for a JSON export.

## Reports and progress

An import returns the namespace's create event, the format, the seq (1), the node and edge counts, the indexes, what was left out (`dropped`), the bytes read and the size of the checkpoint. An export returns the format, the seq exported, the counts and the bytes written. The Rust calls take an optional callback, called with the phase (`Reading`, then `Writing` for an import; `Writing` for an export) and the bytes so far, about every 4 MiB and at the end of each phase; `iwctl` shows it on stderr when stderr is a terminal.

## Errors

| Error | Code | When |
|---|---|---|
| `InvalidImport` | `invalid_argument` | the file is invalid (format, reserved keys, an LGF error, an invariant), or its format can't be detected |
| `NamespaceExists` | `conflict` | the namespace exists |
| `Io` | `io` | reading the file, or writing the checkpoint or the export |
| `NamespaceDropped` | `not_found` | exporting a dropped namespace |
