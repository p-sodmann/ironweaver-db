# ADR 0022: Analytics jobs on a projection: synchronous now, job management later

Status: accepted
Date: 2026-10-03

## Context

The design lists "analytics jobs on a `Projection` (PageRank, components, Leiden, ...)" for the query layer. Since step 8, `Ns::analyze` collects a `Projection` under a short read lock (O(nodes + edges)), then runs a closure on it without any lock, under a cancel token cancelled at the deadline (ADR 0014, ADR 0016). Closures can't cross the wire (design rule 5), and a long job outlives a request's timeout.

"Jobs" could mean two things: a request that runs an algorithm and answers (bounded by its timeout), or managed jobs that run in the background with an id, status, progress, cancellation and stored results.

## Decision

Step 10 provides the first: `Database::analyze(namespace, AnalyticsRequest { projection, job })`, where `job` is data (`Job::PageRank`, `Degree`, `WeaklyConnectedComponents`, `StronglyConnectedComponents`, `Leiden`, `LabelPropagation`, `CoreNumber`, `Triangles`) run by the core's algorithms (`iwdb_query::read::run_job`) through `Ns::analyze`.

- **Bounds.** Collecting the projection reads the whole graph, so it needs `max_visited` of at least the node count and `max_edges` of at least the edge count (`budget_exceeded` otherwise, checked before collecting). The job runs until it ends or the timeout cancels it. `max_results` keeps the top rows: scores and counts highest first, groups biggest first, ties by id (design rule 7), with `truncated` set if rows were cut.
- **Options** reuse the core's types (`algo::PageRank`, `algo::Leiden`); PageRank's personalization, given by dense projection index, is refused.

Managed jobs are **deferred to step 16** (operability), with the reason: they need a job registry, result retention and limits on stored results, progress from the core's algorithms, and an API to list and cancel jobs (`iwctl` "cancel a request" is already there). None of that is needed to serve the synchronous form over gRPC and REST (steps 11, 12), and the right shape depends on how long real jobs take on the benchmark graphs of step 14.

## Consequences

- Analytics are available through every access method in step 11, bounded like any read. A job longer than the server's maximum timeout (5 minutes by default) can't run remotely until managed jobs exist.
- The set of jobs is small and explicit; adding one is an enum variant, a match arm and a conformance case.
- The projection is held in memory for the job's duration, beside the graph; per-namespace memory limits (step 15) must count it.
