# Change stream

Every namespace has a change stream: its commits from a seq on, in order and without gaps, exactly as the WAL logged them. Caches, search indexes and other systems use it to follow the database. Design: [ADR 0031](../adr/0031-change-stream.md).

## What a consumer gets

Each **event** is one commit:

- `seq`: its position in the namespace's history (1, 2, 3, ...);
- `time`: when the WAL appended it;
- the idempotency key it was committed with, if any;
- and either the **ops** of a data commit or the **catalog change** (an index or constraint created or dropped).

The ops are the resolved changes the commit pipeline logged, in the core's vocabulary: `AddNode`, `RemoveNode`, `RenameNode`, `AddLabel`, `RemoveLabel`, `SetNode`, `SetNodeAttr` (without a value: remove it), and the same for edges (`AddEdge` with its id, `RemoveEdge`, `SetEdgeType`, `SetEdge`, `SetEdgeAttr`). Every node and edge a commit writes gets a version op, `SetNodeVersion` / `SetEdgeVersion` (ADR 0004). `RemoveNode` removes the node's edges too: deleting a node logs no op per edge, so a consumer that tracks edges removes those of the node. Applying a commit's ops in order to the state before it gives the state after it, so a consumer that applies every event from seq 1 on rebuilds the namespace.

## Guarantees

- **Only durable commits.** The stream returns commits up to the lower of the applied and the synced seq. A crash can't take back a commit the stream returned, and a seq never comes back with other content. Under `always` that is every acknowledged commit. Under `group` the stream lags by up to the group delay (`2 * max_delay` at most). Under `off`, which promises nothing, it returns what is applied.
- **Exactly once, if the consumer cooperates.** Store the seq of the last commit you processed together with its effect (in the same transaction of your own store), and resume from that seq + 1. You then see every commit once, across restarts of either side. If you store the position separately, you see a commit at least once.
- **Histories.** A restore starts a new history, whose seqs mean something else. Pass the history id of your position (`history`; `Store.history()` embedded, `NamespaceStatus` remotely). A call with another history fails with `invalid_argument`, so you can't silently continue in the wrong log: start again from a snapshot.
- **Retention.** The stream serves the seqs still in the WAL. Checkpoints delete WAL segments they cover. Older seqs fail with **`not_retained`** (gRPC `OUT_OF_RANGE`, HTTP 410), whose message names the oldest retained seq (also every batch's `first_seq`). To keep more, set the store's retention: `StoreOptions::retention` (`WalRetention { records, age }`), Python `Store.open(retain_records=..., retain_age=...)`, server `[store] retain_records` and `retain_age_secs`. A segment is kept while it holds one of the last `records` commits or a commit younger than `age`. Retention is per store, not per consumer: a consumer that stops for longer loses its place.

## Batches and following

The one operation is `changes(namespace, {from_seq, wait}, options)` (the `Database` trait). It returns a **batch**: the commits from `from_seq` on (0 means 1), at most `max_results` of them and about 4 MiB of WAL payload, but always at least one if there is one. It also returns `next_seq` (where the next batch starts), `first_seq` (the oldest retained seq) and the streamable seq the batch was read at (`meta.seq`).

- Without `wait`, a batch with no commit yet is empty.
- With `wait` (a long poll), it waits for a commit for about the request's timeout and then answers with an empty batch, not `timeout`. A waiting call holds no thread.

Following the stream is a loop of long polls, written once in the server for its two streams:

| Access | How |
|---|---|
| Embedded Rust | `Ns::changes(from_seq, limits, wait, &ReadOptions)`, or `Database::changes` on `Embedded` |
| Python | `store.changes(from_seq, wait=..., max_results=..., history=..., timeout=...)`, also on a `Namespace`; events are dicts (below) |
| gRPC | `GetChanges` (one batch); **`Watch`** follows: a message per batch, an empty message (with `next_seq`) after a round without commits, each round as long as `options.timeout_ms` |
| REST | `GET /v1/namespaces/{ns}/changes?from_seq=&wait=` (one batch); **`GET /v1/namespaces/{ns}/changes/stream?from_seq=`** follows, as Server-Sent Events |

**Server-Sent Events.** Each commit is a `change` event whose `id` is its seq and whose `data` is the `ChangeEvent` message in JSON on one line. A round without commits sends a comment line (`: next seq N`). On an error the stream sends an `error` event with an `Error` body and ends. A browser's `EventSource` reconnects by itself and sends the last `id` it saw as `Last-Event-ID`, and the stream resumes after it. An error in the first batch (no such namespace, `not_retained`, a bad parameter) is the answer's HTTP status instead.

```sh
curl -N --cacert ca.pem -H "authorization: Bearer $T" 'https://127.0.0.1:7600/v1/namespaces/default/changes/stream?from_seq=1'
# id: 1
# event: change
# data: {"seq":"1","timeMicros":"...","data":{"ops":[{"addNode":{"id":"ann","labels":["Person"],"data":{"attr":{"age":{"Int":30}},"version":"1"}}}, ...]}}
```

**Shutdown.** The server ends every `Watch` and SSE stream with `unavailable` when it starts shutting down, so they don't hold up the drain. Reconnect elsewhere or later and resume from your position.

## Python events

A batch is `{"events": [...], "next_seq": int, "first_seq": int, "seq": int}`. An event is `{"seq": int, "time": datetime, "key": str | None, "ops": [...]}`, or `"catalog"` instead of `"ops"` (`{"change": "create_index", "path": [...]}`, or `add_constraint` with `kind`, `label`, `path`). An op is a dict with its name in `"op"`: `add_node` (`id`, `labels`, `attr`, `meta`, `version`), `remove_node`, `rename_node` (`new_id`), `add_label` / `remove_label` (`label`), `set_node` (`attr`, `meta`, `version`), `set_node_attr` (`key`, `value`), `remove_node_attr` (`key`), `set_node_version` (`version`), and for edges `add_edge` (`id`, `from`, `to`, `type`, `attr`, `meta`, `version`), `remove_edge`, `set_edge_type`, `set_edge`, `set_edge_attr`, `remove_edge_attr`, `set_edge_version`. Removing an attribute is an op of its own because `None` is a value.

## Not (yet) there

- Positions kept by the server for each consumer (like Postgres replication slots): consumers keep their own, and retention is per store.
- Reading older changes from the WAL archive.
- Filtering on the server (by label, type, key): consumers filter the ops.
- Limits on the number of open streams (step 16) and authentication (step 15).
