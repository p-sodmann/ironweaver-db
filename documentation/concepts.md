# Concepts

The ideas you meet in every API of Ironweaver DB: what is stored, how it changes, and how other systems follow it. What each one promises is on the [guarantees](guarantees.md) page; how to call it is in the [Python API](python-api.md), the [gRPC](api/grpc.md) and [REST](api/rest.md) references, and [iwctl](iwctl.md).

## Store, data directory and namespaces

A **store** is one database: a **data directory** on disk ([layout](formats/data-dir.md)) that one process opens at a time. That process is your application (the embedded library, from Rust or Python) or `iwdb-server`, which serves the store to clients over gRPC and REST. Both use the same code; a client sees the same behaviour, limits and errors whichever way it reaches the store.

A store holds **namespaces**: named, independent graphs. Each has its own commits, seqs, write-ahead log, checkpoints, indexes, constraints and idempotency keys, so a namespace is what a database is in Postgres. `default` exists in every store. A commit changes one namespace; nothing spans two. A dropped namespace is gone for good; its name can be used again for a new namespace with a new id. The store also keeps a reserved, hidden namespace for its users and grants.

## Nodes, edges and values

A namespace is a directed property multigraph:

- A **node** has a string **id** (chosen by you), a set of **labels** (`Person`, `Order`), **attributes** (a map of values) and **meta** (a second map, for data that filters, indexes and constraints don't look at).
- An **edge** goes **from** one node **to** another, has an integer **id** assigned by the store, an optional **type** (`KNOWS`), attributes and meta. Two nodes can have any number of edges between them. Deleting a node deletes its edges.
- A **value** is a string, an integer (64-bit), a float, a bool, bytes, a date, a date-time, `None`, or a list or map of values, nested up to 100 levels.

## Commits, seqs and versions

Every change is a **commit**: a list of **mutations** (upsert or delete a node, add or delete an edge, set or remove an attribute, add a label, ...) applied **all or nothing**. A commit that fails changes nothing.

- Each commit gets the namespace's next **seq** (1, 2, 3, ...). The seq is the namespace's clock: a read reports the seq of the state it saw, the change stream is ordered by seq, and a restore goes back to a seq.
- Each node and edge has a **version**: 1 when created, one higher after every commit that writes it. A mutation can name the version it expects (`expected_version`); if someone else wrote the node or edge since, the commit fails with `conflict`. That is optimistic concurrency, without locks.
- Each commit records its **commit time**, from the store's clock. A restore can go back to a time as well as to a seq.
- A store has a **history id**. A restore starts a new history, so a seq from before the restore can't be mistaken for one after it.

A commit is **acknowledged** once it is in the **write-ahead log (WAL)** and applied. When it is also safe from a power loss depends on the store's **fsync policy**: `always` (every acknowledged commit, the default), `group` (fsyncs in batches, losing at most the last few milliseconds) or `off` (tests). In the background, a **checkpoint** writes a namespace's graph to a file so the WAL can be cut. Details: [guarantees](guarantees.md#durability-of-commits).

## Idempotency keys

A commit can carry an **idempotency key** (any string of 1 to 255 bytes, such as a UUID). The store remembers the result of the last 10 000 keyed commits per namespace. A retry with the same key returns the first result, marked `deduplicated`, instead of committing again. So when a commit's outcome is unknown (a timeout, a lost connection, a crash), you retry it with its key until you get an answer, and it is applied exactly once. The same key with different mutations is an error.

## Reads: read-your-writes, limits and cursors

- **Read-your-writes.** A read with **`min_seq`** waits until that commit is applied, so it sees it. Pass the seq of your commit to another client, and it sees your write too. The Python remote client does this for its own commits.
- **Every read is bounded**: at most `max_results` results, `max_visited` nodes and `max_edges` edges examined, and a timeout. Reaching a limit is an error (`budget_exceeded`), unless the read asks for a **partial** answer. A server sets defaults and caps, and can lower them per user.
- **Cursors.** Reads that list (find, neighbourhood, match) return a page and a **cursor** for the next one. A cursor is valid at the seq of its first page: after a commit to the namespace it expires, and you start again.
- **Indexes** on node attributes speed up finds; `explain` shows whether a read uses one. They are built online, while reads and commits go on. **Constraints** (`unique` or `required` attribute, per label) are checked on every commit.

## Analytics and jobs

**Analytics** (PageRank, connected components, Leiden, degree, core numbers, triangles, ...) run on a copy of the namespace's graph, so reads and commits go on meanwhile. `analyze` runs one within a request and its timeout. A **managed job** (on a server) runs on its own threads past any request's timeout. It has an id, reports its progress, can be cancelled, and keeps its result to be fetched page by page for a while. Jobs live in the server's memory: a restart loses them.

## The change stream

Each namespace has a **change stream**: its commits from a seq on, in order, without gaps, exactly as logged, and only once they are durable. A cache or a search index follows it, and remembers the seq of the last commit it processed. After a restart of either side it resumes from the next seq and misses nothing. The WAL keeps old commits for a while (retention); older seqs are `not_retained`. Over gRPC it is `Watch`, over REST Server-Sent Events. Details: [api/changes.md](api/changes.md).

## Marks and projections

A **mark** is a named position in an external log that a namespace keeps. A commit moves a mark compare-and-set, in the same commit as the changes it records.

A **projection** uses marks to keep a namespace a copy of an external, ordered event log (a Postgres table, so far). It reads events after its mark, maps them to mutations by rules, and commits them together with the new mark. A crash loses the events and the mark together, or neither, so every event is applied exactly once. Details: [api/projections.md](api/projections.md).

## Backups, archives and restore

- An **online backup** copies a running store's checkpoints and WAL into a directory, consistent per namespace.
- A **WAL archive** keeps every WAL segment the store no longer needs, so the history isn't lost when checkpoints cut the WAL.
- A **restore** builds a new data directory from a backup, an archive, or both, at the latest state, at a seq or at a time (point-in-time recovery).
- **`verify`** checks a data directory, backup or archive without changing it.

How to run them: the [operations guide](operations.md#backups-and-restore).

## Users, roles and limits

A server knows **users** (with passwords), their **API tokens**, and their **roles** per namespace: `read`, `write` or `admin`, plus a server-wide admin. Every call is checked against them ([authorisation](guarantees.md#authentication-and-authorisation)); a client certificate can stand for a user too. **Limits** per user (calls per second, running calls, query budgets) and per namespace (running calls, memory) keep one client from starving the others. An embedded store has no users: whoever can open the data directory owns it.

## An example

Two commits with one idempotency key: the retry commits nothing and returns the first result.

```python
import iwdb

with iwdb.Store.open("data") as store:
    with store.transaction(idempotency_key="order-17") as tx:
        tx.upsert_node("ann", labels=["Person"], attr={"name": "Ann"})
        tx.upsert_node("bob", labels=["Person"])
        tx.add_edge("ann", "bob", type="KNOWS")
    first = tx.result
    assert first["seq"] == 1 and first["versions"]["nodes"] == {"ann": 1, "bob": 1}

    # The outcome was lost, say: retry with the same key
    with store.transaction(idempotency_key="order-17") as tx:
        tx.upsert_node("ann", labels=["Person"], attr={"name": "Ann"})
        tx.upsert_node("bob", labels=["Person"])
        tx.add_edge("ann", "bob", type="KNOWS")
    assert tx.result["deduplicated"] and tx.result["seq"] == first["seq"]

    # A write with the version it expects, then a read that waits for it
    with store.transaction() as tx:
        tx.set_attr("ann", "age", 36, expected_version=1)
    ann = store.node("ann", min_seq=tx.result["seq"])
    assert ann["version"] == 2 and ann["attr"]["age"] == 36
```
