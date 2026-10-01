# ADR 0018: Idempotency keys with namespaces

Status: accepted
Date: 2026-10-01

Follow-up to [ADR 0015](0015-idempotency-keys.md) and part of [ADR 0017](0017-namespaces.md).

## Decision

- **Data and catalog commits: the key table stays per namespace.** It is namespace state, rebuilt from that namespace's records and saved in its checkpoints (`iwdb.keys`, unchanged). A key is a name for one request *to one namespace*: the same key may be used in two namespaces, as two independent requests. This is what ADR 0015 already guarantees per table, and it keeps the table a pure function of one namespace's records (verify, the checkpointer, restore and replicas rebuild it without the other namespaces). A key table's capacity (10 000) is per namespace.
- **Create and drop of a namespace** take a key too, kept in the **namespace log**: every event carries its key and the fingerprint of its request (the operation and the name). A store-wide table is needed because a dropped namespace has no state left to hold its drop's key, and a create's key must be findable before the namespace is opened. The table is the set of keys in the log: unbounded in principle, but bounded by the number of namespace operations, which are rare, and it is never evicted (the log is never truncated). A retry with the same key and request returns the original event (`deduplicated: true`, nothing logged, also after a restart and after the namespace was dropped since); the same key for another request (another operation or name) is `IdempotencyKeyReused`.
- **Two key spaces**: a key used for a namespace operation and the same string used for a commit are independent (different tables).
- **Across a restore**: the restored namespace log keeps the events up to the target with their keys, and each namespace's checkpoint keeps its table at its target seq: as in ADR 0015, the tables describe exactly the history the store holds.
- **Index and constraint changes** are catalog commits of one namespace, so they take keys through that namespace's table, as since step 8. An index build that a duplicate key would repeat is skipped (the key is looked up before the scan).
- **The table size** stays a format constant.
