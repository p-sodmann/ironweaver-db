# Import fixtures

Graph files written by the Ironweaver library, copied from
`ironweaver-core`'s `tests/data/` at revision `7e7b7fa`:

- `legacy_graph.json`, `legacy_graph.bin`: Ironweaver 0.1 (format version 1).
  The JSON file imports (the core migrates it); the binary file is refused
  with a message saying how to convert it.
- `v2_graph.json`, `v2_graph.bin`, `v2_graph_f16.bin` (floats at half
  precision): format version 2.

Each holds nodes `a`, `b`, `c` (`a` with attributes of every simple kind
and the meta `note`), three edges (`a -> b` of type `knows`; in version 1
the type is the edge attribute `type`, which the core migrates), and the
graph meta `title`, which an import drops. Used by `tests/import.rs`.
