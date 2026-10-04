# Python API

Status: contract for the embedded bindings (`crates/iwdb-python`, step 7) and the remote client (`iwdb.connect`, step 14, [ADR 0035](adr/0035-python-remote-client.md)): both expose these names, arguments, return values and exceptions, so that code (and the shared test suite) runs against either. Decisions: [ADR 0013](adr/0013-python-bindings.md).

```python
import iwdb

with iwdb.Store.open("data") as store:
    with store.transaction() as tx:
        tx.upsert_node("alice", labels=["Person"], attr={"name": "Alice", "born": 1990})
        tx.upsert_node("bob", labels=["Person"], attr={"name": "Bob"})
        tx.add_edge("alice", "bob", type="KNOWS", attr={"since": 2020})
    print(tx.result)
    # {'seq': 1, 'edge_ids': [0], 'versions': {'nodes': {'alice': 1, 'bob': 1}, 'edges': {0: 1}},
    #  'time': datetime.datetime(2026, 10, 1, 12, 0, tzinfo=datetime.timezone.utc), 'deduplicated': False}
    print(store.node("alice"))
    # {'id': 'alice', 'labels': ['Person'], 'attr': {'born': 1990, 'name': 'Alice'}, 'meta': {}, 'version': 1}
```

Everything a call returns is a plain Python value: `dict`, `list`, `str`, `int`, `float`, `bool`, `bytes`, `None`, `datetime.date`, `datetime.datetime`. No result holds a reference into the store.

## `iwdb.Store`

### Opening and closing

`Store.open(path, *, create_if_missing=True, fsync="always", group_max_delay=0.01, group_max_batch=64, segment_size=64 << 20, checkpoint_wal_size=256 << 20, checkpoint_interval=300.0, checkpoint_on_close=True, checkpoint_keep=2, checkpoint_background=True, archive=None) -> Store`

Opens (and, with `create_if_missing`, creates) the store in the directory `path` (`str` or `os.PathLike`), and recovers it. The options are those of `iwdb::StoreOptions`:

| Option | Meaning |
|---|---|
| `fsync` | `"always"` (every commit is durable when acknowledged), `"group"` (fsync every `group_max_delay` seconds or `group_max_batch` commits), `"off"` (tests only). [guarantees.md](guarantees.md) says what each loses in a crash |
| `segment_size` | WAL segment size in bytes (1 KiB to 1 GiB) |
| `checkpoint_wal_size`, `checkpoint_interval` | Checkpoint after this many bytes of WAL, or seconds; `None` turns the trigger off |
| `checkpoint_on_close`, `checkpoint_keep`, `checkpoint_background` | Checkpoint on close; checkpoints kept; run the triggers in a background thread |
| `archive` | A WAL archive directory (continuous archiving, [archive.md](formats/archive.md)) |

`store.close() -> None`: stop the background threads, fsync the WAL, checkpoint (with `checkpoint_on_close`), release the lock. Closing a closed store does nothing. Afterwards every other method raises `iwdb.ClosedError`. `store.closed` (a property) says whether it is closed. A store that is garbage-collected without `close` releases its lock without a checkpoint or fsync, like a crash of the process (the OS keeps what was written).

`with Store.open(...) as store:` closes the store when the block ends, also on an exception.

Reopening a closed directory works at once (the lock is released). Opening a directory that another store has open, in this process or another one, raises `iwdb.LockedError`.

### Transactions

`store.transaction(*, idempotency_key=None) -> Transaction` collects mutations; they are committed as **one** transaction, all or nothing:

```python
with store.transaction() as tx:      # commits when the block ends without an exception
    tx.set_attr("alice", "age", 36)
# on an exception inside the block, nothing is committed and the exception propagates

tx = store.transaction()             # or explicitly
tx.delete_node("bob")
result = tx.commit()
```

A transaction is committed once: by `commit()`, or when its `with` block ends (if `commit()` wasn't called and it has mutations; an empty block commits nothing and `result` stays `None`). Adding a mutation after the commit raises `iwdb.InvalidError`. `tx.result` is the commit's result (or `None`), `len(tx)` the number of mutations. A transaction belongs to one thread.

Mutations (all arguments after the first ones are keyword-only; `expected_version` is optimistic concurrency: `None` skips the check, `0` means "must not exist", `n` means "must be at version `n` before this transaction"; nodes are addressed by `str` id, edges by `int` id):

| Method | Mutation |
|---|---|
| `upsert_node(id, *, labels=(), attr=None, meta=None, expected_version=None)` | create the node, or replace its attributes and meta; labels are added |
| `delete_node(id, *, expected_version=None)` | delete the node and its edges |
| `add_edge(from_, to, *, type=None, attr=None, meta=None) -> int` | add an edge with a new id; returns the position of its id in `result["edge_ids"]` |
| `upsert_edge(*, id=None, from_=None, to=None, type=None, attr=None, meta=None, expected_version=None) -> int` | update edge `id`, or the one edge `from_ -> to` of `type` (adding it if there is none); returns the position of its id in `result["edge_ids"]` |
| `delete_edge(id, *, expected_version=None)` | |
| `set_attr(target, key, value, *, expected_version=None)` | `target`: a node id (`str`) or an edge id (`int`) |
| `remove_attr(target, key, *, expected_version=None)` | |
| `append_attr(target, key, value, *, expected_version=None)` | append to the list in `key` (a missing or `None` attribute becomes `[value]`) |
| `add_label(id, label, *, expected_version=None)`, `remove_label(...)` | |
| `set_edge_type(id, type, *, expected_version=None)` | `type`: `str` or `None` |

A commit returns (and `tx.result` holds) `{"seq": int, "edge_ids": [int, ...], "versions": {"nodes": {id: version}, "edges": {id: version}}, "time": datetime, "deduplicated": bool}`: the commit's seq, one edge id per `add_edge` / `upsert_edge` in order, the new version of every node and edge it wrote that still exists, the commit time (an aware `datetime` in UTC, from the store's clock when the WAL appended the commit; [ADR 0010](adr/0010-commit-times.md)), and whether the commit was answered from its idempotency key instead of being applied now.

**Idempotency keys** (step 8, [ADR 0015](adr/0015-idempotency-keys.md)). `idempotency_key` (a `str` of 1 to 255 UTF-8 bytes, such as a UUID) makes a commit apply at most once: retrying it with the same key after an unknown outcome (an `iwdb.IoError`, a timeout, a crash of the process) returns the original result, with `"deduplicated": True`, if the first attempt was applied, and commits now if it wasn't. The store remembers the last 10 000 keyed commits, across restarts, checkpoints, backups and restores (a restore keeps the keys of the commits it restores). Reusing a key for **different** mutations raises `iwdb.ConflictError` (`iwdb.InvalidError` before step 10) and changes nothing. The catalog methods take `idempotency_key` too.

### Namespaces

A store has named namespaces (step 9, [ADR 0017](adr/0017-namespaces.md)), each its own graph with its own seq space, WAL, checkpoints, key table, indexes and constraints. `"default"` exists in every store (it is what a layout 1 to 3 store becomes) and can't be dropped. The `Store` methods for nodes, transactions, catalog, `seq`, `checkpoint` and the rest act on `"default"`; a `Namespace` handle has the same methods for any namespace.

| Method | |
|---|---|
| `store.create_namespace(name, *, idempotency_key=None) -> dict` | `{"id": int, "name": str, "time": datetime, "event": int, "deduplicated": bool}`. A name is 1 to 64 ASCII letters, digits, `_` or `-`, starting with a letter or digit. An existing name raises `iwdb.InvalidError`. With an idempotency key a retry returns the original event with `"deduplicated": True` |
| `store.drop_namespace(name, *, idempotency_key=None) -> dict` | the same dict for the drop. The namespace and its data are gone for good; commits and `min_seq` waits on it raise `iwdb.NotFoundError`; a read that started finishes. `"default"` raises `iwdb.InvalidError`. A name that doesn't exist raises `iwdb.NotFoundError` |
| `store.namespaces() -> list[dict]` | `[{"id": int, "name": str, "created": datetime}, ...]` sorted by name (`created` is the epoch for a namespace that predates layout 4) |
| `store.namespace(name) -> Namespace` | a handle; `iwdb.NotFoundError` if there is no such namespace *now*. A handle looks the name up on every call, so it fails with `NotFoundError` after the namespace is dropped, and a namespace created again under the name is a different one (compare `ns.id`) |

A **`Namespace`** has `name` and `id`, `transaction(*, idempotency_key=None)`, `node`, `edge`, `wait_for_seq`, `seq`, `synced_seq`, `read_only`, `catalog`, `indexes`, `create_index`, `drop_index`, `add_constraint`, `drop_constraint`, `sync`, `checkpoint` and `status` with the arguments and results documented below for the `Store`. Seqs, `min_seq` and idempotency keys are **per namespace**: the same key in two namespaces is two requests. The history id is the store's.

```python
with iwdb.Store.open("data") as store:
    store.create_namespace("social")
    social = store.namespace("social")
    social.create_index("email")
    social.add_constraint("unique", "Person", "email")
    with social.transaction() as tx:
        tx.upsert_node("alice", labels=["Person"], attr={"email": "a@example.com"})
    print(social.node("alice", min_seq=tx.result["seq"]))
```

`create_index` builds the index **online** ([ADR 0019](adr/0019-online-index-build.md)): readers and commits go on while the nodes are scanned. A build in progress shows in `indexes()` with `"state": "building"`. Adding a constraint validates the existing data first (`iwdb.ConstraintError` if some node violates it) and takes the namespace's writer meanwhile (reads go on).

### Catalog

Each is its own commit, and returns the same result dict (with empty `edge_ids` and `versions`). A `path` is a `str` (one attribute) or a list of `str` (an attribute, then keys into nested dicts).

| Method | |
|---|---|
| `create_index(path, *, idempotency_key=None)`, `drop_index(path, *, idempotency_key=None)` | a property index on node attributes |
| `add_constraint(kind, label, path, *, idempotency_key=None)`, `drop_constraint(kind, label, path, *, idempotency_key=None)` | `kind`: `"unique"` or `"required"`, for the nodes with `label` |
| `catalog(*, min_seq=None, timeout=None) -> dict` | `{"indexes": [["a"], ["b", "c"]], "constraints": [{"kind": "unique", "label": "Person", "path": ["email"]}]}` |
| `indexes() -> list[dict]` | every index with its state: `{"path": [..], "state": "ready" \| "building", "declared": bool, "unique": bool, "scanned": int \| None, "total": int \| None, "entries": int \| None, "distinct_keys": int \| None, "memory_bytes": int \| None}`; `declared` by `create_index`, `unique` if a unique constraint needs it; `scanned` and `total` while building; `entries` (nodes with a value at the path), `distinct_keys` and `memory_bytes` once ready |

### Reads

| Method | Returns |
|---|---|
| `node(id, *, min_seq=None, timeout=None) -> dict \| None` | `{"id": str, "labels": [str] (sorted), "attr": dict, "meta": dict, "version": int}` |
| `edge(id, *, min_seq=None, timeout=None) -> dict \| None` | `{"id": int, "from": str, "to": str, "type": str \| None, "attr": dict, "meta": dict, "version": int}` |
| `wait_for_seq(seq, *, timeout=None) -> int` | waits until commit `seq` is applied; returns the store's seq |
| `seq() -> int` | the seq of the last commit (0: none) |
| `synced_seq() -> int \| None` | the highest seq known to be durable; `None` under `fsync="off"` until an explicit `sync()` |
| `read_only() -> str \| None` | why the store accepts no more commits (a failed WAL write or fsync), until it is reopened |
| `history() -> str` | the history id (32 hex digits) |
| `status() -> dict` | `{"seq", "synced_seq", "checkpoint", "read_only", "checkpoint_failure"` (of `"default"`)`, "history", "fsync", "archive", "catalog_failure", "recovery": {...}, "namespaces": [...]}`. Each of `namespaces` (and `namespace.status()`) is `{"id", "name", "created", "seq", "synced_seq", "checkpoint", "read_only", "checkpoint_failure", "nodes", "edges", "memory_bytes", "constraints", "indexes": [{"path", "state", "declared", "unique"}], "recovery": {...}}`; `memory_bytes` is the graph's approximate memory, indexes included |

**Read-your-writes** (step 8, [ADR 0016](adr/0016-read-your-writes-and-deadlines.md)): with `min_seq` (for example `result["seq"]` of an earlier commit), a read first waits until that commit is applied, so it sees it and everything before it. `timeout` (seconds, default 30; `float("inf")`: none) bounds the wait and, since step 10, the read itself: after it, `iwdb.TimeoutError`. A store that is read-only below `min_seq` raises `iwdb.ReadOnlyError` at once (it can't get there). In the embedded store every commit is applied before it returns, so a thread's own commits are always visible; `min_seq` matters for seqs from other threads, and for the remote client (step 14), which tracks the seq of its last commit and sends it. Reads run concurrently with each other and with commits; they never see part of a transaction.

### Queries

Since step 14 ([ADR 0035](adr/0035-python-remote-client.md)), the reads of the `Database` trait: bounded (design rule 5), on `Store` (namespace `"default"`) and `Namespace`, embedded and remote alike. Semantics, limits and orders are those of the trait ([ADR 0021](adr/0021-bounded-reads-and-cursors.md), [ADR 0022](adr/0022-analytics-jobs.md)).

Every query takes the read options as keywords: `max_results`, `max_visited`, `max_edges` (`None`: the store's or server's defaults: 1 000, 100 000 and 1 000 000; above the caps, the caps; 0 raises `InvalidError`), `partial=False`, `cursor=None`, `min_seq` and `timeout` (as for `node`). `explain` takes only `min_seq` and `timeout`.

Every answer is a dict with the results (below) and `"seq"` (the state the read saw), `"cursor"` (`str`: pass it as `cursor=` with the same request for the next page; `None`: no more), `"truncated"` (with `partial=True`, a limit cut the answer; then there is no cursor) and `"work"` (`{"visited": int, "edges": int}`).

Reaching a limit raises `iwdb.BudgetExceededError`, unless `partial=True`. `max_results` is the page size of the paginated reads (`find`, `neighbourhood`, `match`: more results give a cursor, not an error) and keeps the top rows of `analyze`. A cursor is valid at one seq: after a commit to the namespace, the next page raises `iwdb.CursorExpiredError` (start again without it); a cursor of another request raises `InvalidError`.

| Method | Results |
|---|---|
| `find(filter)` | `"nodes"`: node dicts matching the [filter](#filters), sorted by id; uses an index where it can |
| `explain(filter, *, analyze=False)` | `"plan"` (`{"kind": "index", "path": [...], "lookup": "point" \| "in" \| "range", "values": n}`, `{"kind": "label", "label"}`, `{"kind": "union", "plans": [...]}`, `{"kind": "scan"}`, `{"kind": "empty"}` or `{"kind": "other", "text"}`), `"estimated_candidates"`, `"candidates"` (with `analyze`: exact, `None` otherwise), `"nodes"` (the namespace's node count), `"building"` (paths whose index is being built) |
| `neighbourhood(seeds, *, depth=1, direction="out", edge_types=None, edge_filter=None, node_filter=None)` | `"nodes"`: the nodes within `depth` edges of `seeds` (an id or a list of ids; missing ones are skipped), sorted by id; `node_filter` selects what is returned, not what is followed |
| `traverse(start, *, order="bfs", depth=None, direction="out", edge_types=None, edge_filter=None)` | `"ids"`: node ids in BFS or DFS (`"dfs"`) order from `start` |
| `shortest_path(from_, to, *, method="bfs", weight=None, default_weight=1.0, coords=None, metric="euclidean", direction="out", max_depth=None, max_cost=None)` | `"path"`: `{"nodes": [ids], "cost": float}` or `None`. `method`: `"bfs"` (fewest edges), `"dijkstra"` or `"astar"`, cheapest by the edge attribute `weight` (`default_weight` for edges without it; unweighted if `None`). A* guesses distances from `coords`: an attribute holding a list of numbers (`"pos"`), or one attribute path per dimension (`["x", "y"]`, the default), with `metric` `"euclidean"` or `"manhattan"`. A missing `from_` or `to` raises `NotFoundError` |
| `random_walks(start, *, max_length, walks=1, min_length=1, allow_revisit=False, seed=None)` | `"walks"`: lists of ids, duplicates removed |
| `subgraph(seeds, *, depth=1, direction="out", edge_types=None, edge_filter=None)` | `"nodes"` and `"edges"` (edge dicts as from `edge`), both sorted by id |
| `match(pattern, *, where=None)` | `"rows"`: `{"nodes": [ids], "edges": [[edge ids], ...]}` per match, one id per pattern node and one list of edge ids per pattern edge (a path for a variable-length edge), in the pattern's order; rows sorted. `pattern` is the core's text, `"(a:Person {age: 30})-[:knows*1..3]->(b)"`; `where` maps node variables to filters |
| `analyze(job, *, params=None, direction="out", weight=None, default_weight=1.0)` | a job on a projection of every node and the edges in `direction`: `"pagerank"` (`params`: `alpha`, `max_iter`, `tol`) and `"degree"` (`incoming`) give `"scores"`, `[[id, float], ...]` highest first; `"weakly_connected_components"`, `"strongly_connected_components"`, `"leiden"` (`resolution`, `randomness`, `max_iter`, `seed`) and `"label_propagation"` (`max_iter`, default 100) give `"groups"`, lists of ids, biggest first; `"core_number"` and `"triangles"` give `"counts"`, `[[id, int], ...]` highest first. Unknown jobs or parameters raise `ValueError`. The projection is collected under the read limits: `max_visited` and `max_edges` must cover the namespace |

`direction` is `"out"`, `"in"` or `"both"`; `edge_types` a list of types to follow (all if `None`).

```python
from iwdb import attr, label

adults = store.find(label("Person") & (attr("age") >= 18), max_results=100)
while adults["cursor"]:
    adults = store.find(label("Person") & (attr("age") >= 18), max_results=100, cursor=adults["cursor"])
rows = store.match("(a:Person)-[:KNOWS]->(b)", where={"b": attr("name") == "Bob"})["rows"]
```

### Operations

| Method | Returns |
|---|---|
| `sync() -> None` | fsync every commit so far (whatever the policy) |
| `checkpoint() -> dict` | `{"seq", "written", "removed_checkpoints", "removed_segments"}` (of `"default"`; `namespace.checkpoint()` for another; `store.checkpoint_all()` returns `{name: {...}}` for every namespace) |
| `import_namespace(name, path, *, format=None) -> dict` | create the namespace `name` from a graph file (core JSON or binary, or LGF; `format` `"json"`, `"binary"` or `"lgf"`, detected by default), as one checkpoint at seq 1, all or nothing: `{"id", "name", "time", "format", "seq", "nodes", "edges", "indexes", "dropped", "bytes_read", "checkpoint_bytes"}` ([api/import-export.md](api/import-export.md)). With an `archive`, the import's checkpoint is archived too |
| `import_file(path, *, format=None) -> dict` | merge a graph file into `"default"` through commits, in batches (nodes upserted, edges upserted by their ends and type; [api/import-export.md](api/import-export.md)): `{"format", "nodes", "edges", "created_indexes", "dropped", "bytes_read", "commits", "first_seq", "last_seq"}`; `namespace.import_file(...)` for another |
| `export(path, *, format=None) -> dict` | write `"default"`'s graph to `path` as a core file (JSON for `.json`, binary otherwise, or `format`), atomically: `{"format", "seq", "nodes", "edges", "bytes"}`; `namespace.export(...)` for another |
| `backup(dest) -> dict` | an online backup of every namespace into a new or empty directory: `{"path", "history", "bytes", "namespaces": [{"id", "name", "seq", "time", "checkpoints", "segments"}, ...]}`, and `"seq"`, `"time"`, `"checkpoints"`, `"segments"` of `"default"` ([backup.md](formats/backup.md)) |

## Module functions

| Function | Returns |
|---|---|
| `iwdb.connect(endpoint) -> Store` | a client of a server ([The remote client](#the-remote-client)) |
| `iwdb.verify(path) -> dict` | check a data directory, backup or archive and change nothing: `{"ok": bool, "kind", "problems": [{"path", "message"}], "notes": [...], "seq", "last_seq", "records", "checkpoints", "segments", "namespaces": [{"id", "name", "seq", "records", ...}], ...}` (single-namespace fields are those of the one namespace, `None` when there are several; [ADR 0011](adr/0011-verify.md)) |
| `iwdb.restore(dest, *, backup=None, archive=None, seq=None, time=None, namespaces=None) -> dict` | restore a store into a new or empty directory from a backup and/or an archive: every namespace that existed at the target (or just the names in `namespaces`), each to the latest it reaches, or to the last commit at or before `time` (an aware `datetime`). `seq` is one namespace's seq, so it needs exactly one namespace to restore (give `namespaces=["name"]`, or restore a store that has just one): otherwise `iwdb.InvalidError`. Returns `{"path", "history", "source_history", "namespaces": [{"id", "name", "seq", "time", "checkpoint", "replayed", ...}]}` and the fields of `"default"` at the top |

The remote client (`iwdb.connect`, step 14) has the API above apart from the calls that need the store's directory: see [The remote client](#the-remote-client).

## Filters

`iwdb.attr`, `iwdb.label`, `iwdb.edge_type` and `iwdb.const` build filters (the core's `Expr`), combined with `&` (and), `|` (or) and `~` (not):

| Filter | Holds for |
|---|---|
| `attr(path) == v`, `!=`, `<`, `<=`, `>`, `>=` | the attribute at `path` (a `str`, or a list of `str`: a key, then keys into nested dicts) compared with `v`; order comparisons only between two numbers, two strings or two bools |
| `attr(path).isin(values)` | the attribute equals one of `values` |
| `attr(path).exists()` | the attribute exists and is not `None` |
| `label(name)` | nodes with the label (never edges) |
| `edge_type(name)` | edges of the type (never nodes) |
| `const(True)`, `const(False)` | everything, nothing |

Values convert as in [Values](#values). A filter has no truth value: `and`, `or`, `not` and chained comparisons (`1 < attr("x") < 3`) raise `TypeError`; write `(attr("x") > 1) & (attr("x") < 3)`. A filter nests at most 100 levels (And, Or and Not around a leaf, which counts as one; the core's limit): deeper ones raise `ValueError`. Anything else where a filter is expected raises `TypeError`.

## The remote client

`iwdb.connect(endpoint) -> Store` (`endpoint`: `"http://host:port"` of an `iwdb-server`; no TLS until step 15) returns a store with this API, served over gRPC ([ADR 0035](adr/0035-python-remote-client.md)). It connects on the first call and again after a lost connection; without a server, calls raise `iwdb.UnavailableError`. `close()`, `closed` and `with` work as for an embedded store.

- **Embedded only.** These need the store's directory and raise `iwdb.InvalidError` on a remote store: `sync`, `checkpoint`, `checkpoint_all`, `history`, `status` (the store's; `namespace.status()` works), `backup`, `import_namespace`, `import_file`, `export` (and their `Namespace` forms). `verify` and `restore` act on directories.
- **Read-your-writes.** The client remembers the seq of its last commit (data or catalog) per namespace, and a read without `min_seq` waits for it, so a client always sees its own commits. Pass `result["seq"]` as `min_seq` to another client to let it see them too. Creating or dropping a namespace through the client forgets the namespace's seq.
- **Errors** keep their class: the server sends the error's code and the client raises the class of the [exceptions table](#exceptions).
- **Limits** are the server's (`[limits]` in its config), timeouts included: `timeout=float("inf")` is the server's maximum.

## asyncio

`iwdb.aio` wraps both kinds of store: `await iwdb.aio.open(path, **options)` and `await iwdb.aio.connect(endpoint)` return an `AsyncStore` whose methods are coroutines with the same arguments, results and exceptions; `await store.namespace(name)` an `AsyncNamespace`. Each call runs in a worker thread (`asyncio.to_thread`) and releases the GIL in Rust, so calls run concurrently with the event loop and each other. `async with store:` closes it. A transaction collects mutations with plain calls; `await tx.commit()`, or `async with store.transaction() as tx:` commits when the block ends, as `with` does.

```python
store = await iwdb.aio.connect("http://127.0.0.1:7600")
async with store:
    async with store.transaction() as tx:
        tx.upsert_node("alice", labels=["Person"])
    print(await store.find(iwdb.label("Person")))
```

## Values

Python values become database values and back **exactly**:

| Python | Database (`Value`) | Notes |
|---|---|---|
| `None` | `None` | |
| `bool` | `Bool` | checked before `int` (`bool` is an `int`) |
| `int` | `Int` (`i64`) | outside -2^63 .. 2^63 - 1: `OverflowError` |
| `float` | `Float` (`f64`) | bit for bit: `-0.0`, `nan`, `inf` |
| `str` | `String` | |
| `bytes`, `bytearray` | `Bytes` | read back as `bytes` |
| `list` | `List` | a `tuple` is refused (`TypeError`): it would come back as a list |
| `dict` with `str` keys | `Dict` | other keys: `TypeError`; read back with keys sorted |
| `datetime.datetime` | `DateTime` | microseconds; an aware one keeps its UTC offset (as a fixed `timezone`, not a zone name; offsets with seconds' fractions are refused), a naive one stays naive |
| `datetime.date` | `Date` | |

Anything else raises `TypeError`. Values nest at most 100 levels (a scalar is 1; an empty list or dict counts as holding a scalar): deeper ones raise `ValueError`, and so does a list that contains itself. Half-precision floats (which only the core's own files can hold) read back as `float`. Top-level attribute keys and meta keys starting with `iwdb.` are reserved (`InvalidError`).

## Exceptions

Every error is an `iwdb.Error` (an `Exception`), with the Rust message as its text, except argument errors, which are Python's own (`TypeError`, `ValueError`, `OverflowError`). Since step 10 the store's reads, commits, catalog and namespace calls go through the `Database` trait (`iwdb::Embedded`), and their exceptions follow its error codes (`documentation/api/errors.md`): `conflict` is `ConflictError`, `not_found` is `NotFoundError`, and so on; only `cancelled` has no class of its own (`iwdb.Error`; Python can't cancel a call).

| Exception | Raised for |
|---|---|
| `iwdb.ConflictError` | a version conflict (`expected_version`), a namespace or index that exists already, or an idempotency key reused for another request; nothing changed |
| `iwdb.ConstraintError` | a unique or required constraint violated by the transaction; nothing changed |
| `iwdb.NotFoundError` | a mutation addressed a node or edge that doesn't exist, an index or constraint to drop doesn't exist, or a namespace doesn't exist (any more); nothing changed |
| `iwdb.InvalidError` | any other invalid commit (reserved key, empty transaction, value too deep, ambiguous edge, an idempotency key of an invalid length, ...), invalid options, a directory that isn't a store (or is a backup), a destination that isn't empty |
| `iwdb.ReadOnlyError` | the store is read-only after a failed WAL write or fsync, until reopened |
| `iwdb.LockedError` | another store has the directory (or archive) open |
| `iwdb.IoError` | a file operation failed; for a commit, its outcome is unknown and the store is read-only ([guarantees.md](guarantees.md)) |
| `iwdb.CorruptError` | damage in the WAL, a checkpoint, a marker, a manifest or the namespace log, or a namespace directory that is missing; recovery refused |
| `iwdb.TimeoutError` | a read (its `min_seq` wait included) didn't finish within its `timeout`; nothing changed |
| `iwdb.BudgetExceededError` | a query reached `max_results`, `max_visited` or `max_edges` without `partial=True` (step 14) |
| `iwdb.CursorExpiredError` | the namespace changed since a paginated query's first page: start again without the cursor (step 14) |
| `iwdb.NotRetainedError` | `changes` was asked for a seq older than the oldest one still in the WAL |
| `iwdb.UnavailableError` | a remote store can't reach its server (no connection, shutting down, overloaded): retry with backoff (step 14) |
| `iwdb.ClosedError` | the store is closed |
| `iwdb.InternalError` | a bug: a Rust panic outside the commit path |

## Threads, processes and crashes

- A `Store` may be shared between Python threads. Commits are serialized (one writer); reads run concurrently with each other and with commits, and wait only while a commit applies its changes (step 8, [ADR 0014](adr/0014-concurrent-readers.md)). Every call that does I/O or may wait (open, close, commits, reads and their `min_seq` waits, `sync`, `checkpoint`, `backup`, `verify`, `restore`) releases the GIL while it runs in Rust. `close()` waits for calls in progress on other threads, a `min_seq` wait at most until its timeout.
- **Don't fork while a store is open** (`os.fork()`, `multiprocessing` with the `fork` start method): the child inherits the directory's lock, which then stays held until the child exits, and the child must not use the store. Use the `spawn` start method, and open the store in the child.
- **A panic in the commit path aborts the process** ([ADR 0008](adr/0008-panics-in-the-commit-path-abort.md)), and so the Python interpreter: no `finally` runs. It happens only on a bug (for example upstream #28), never on user input. The next `Store.open` recovers every logged commit. A panic anywhere else in the bindings raises `iwdb.InternalError`.
- A killed process (`kill -9`) loses no acknowledged commit under `fsync="always"`; the next `Store.open` recovers ([guarantees.md](guarantees.md)).
