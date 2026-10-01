# ironweaver-db

Ironweaver DB, embedded: a durable graph store in a directory, built on
[ironweaver-core](https://github.com/p-sodmann/Ironweaver). Commits go
through a write-ahead log (fsync policies `always`, `group`, `off`), with
checkpoints, crash recovery, online backups, continuous WAL archiving,
point-in-time restore and `verify`.

```python
import iwdb

with iwdb.Store.open("data") as store:
    with store.transaction() as tx:
        tx.upsert_node("alice", labels=["Person"], attr={"name": "Alice"})
        tx.upsert_node("bob", labels=["Person"], attr={"name": "Bob"})
        tx.add_edge("alice", "bob", type="KNOWS")
    print(store.node("alice"))
    store.backup("backups/today")

iwdb.restore("restored", backup="backups/today")
```

Wheels for Linux and macOS (x86_64 and arm64), CPython 3.9 and later
(abi3). Windows isn't supported yet.

- [Python API](https://github.com/p-sodmann/ironweaver-db/blob/main/documentation/python-api.md)
- [Guarantees](https://github.com/p-sodmann/ironweaver-db/blob/main/documentation/guarantees.md)

A panic in the store's commit path (a bug) aborts the process, and so the
interpreter; the next `Store.open` recovers every logged commit. Don't fork
while a store is open (use the `spawn` start method of `multiprocessing`).

MIT license.
