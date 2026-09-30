# Step 18: Replication and high availability

Status: todo
Milestone: M5 HA
Depends on: step 17

## Goal

Read replicas and failover, with tested consistency.

## Tasks

- [ ] Read replicas via WAL shipping (reusing the change stream and the checkpointer's replay code); replicas serve reads with `min_seq`.
- [ ] Replica lag metrics.
- [ ] Manual failover procedure with `iwctl` support.
- [ ] Automatic leader election (Raft, e.g. openraft) only if users need it; decide in an ADR.
- [ ] Jepsen-style harness: linearizable writes, consistent reads under partitions and crashes.

## Acceptance criteria

- Jepsen-style tests pass.
- Failover procedure documented and tested.
- M5 is done: update [README.md](README.md).
