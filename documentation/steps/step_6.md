# Step 6: Crash and fault-injection suite

Status: todo
Milestone: M1 Embedded durable
Depends on: step 5

## Goal

Prove the durability guarantee: after any crash, recovery reaches exactly the last acknowledged commit (fsync `always`) and never exposes a partial transaction.

## Tasks

- [ ] Failpoints (`fail` crate) in WAL append, fsync, segment rotation, checkpoint write, rename and directory fsync.
- [ ] Kill -9 harness: a child process runs a random workload and reports acknowledged `seq`s; the parent kills it at random points, reopens and compares with the reference model (canonical state).
- [ ] Simulated torn writes, full disk (ENOSPC) and fsync errors, each with defined behaviour.
- [ ] A panic inside the commit path is treated as a crash (process exits, recovery restores state).
- [ ] Short run on every PR, long run nightly.

## Acceptance criteria

- Thousands of kill/recover cycles without a lost acknowledged commit or a partial transaction.
- Every failpoint has at least one test.
