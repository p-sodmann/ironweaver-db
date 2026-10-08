# ADR 0053: The schema read, and the console on the status views

Status: accepted
Date: 2026-10-06

## Context

Step 16c's second part puts the operator console on a real server with nothing faked. Until then the console's REST Source made up what the server couldn't answer:

- **The schema** (labels with counts, edge types with counts, attribute keys per label) came from a `find` of the first 2000 nodes and a `subgraph` over them.
- **The server's page** had readiness and every namespace's status, and nothing else.
- **`cancel`** failed, and **the log** was the page's own requests.
- **The mock** had more: a `find` total, explain's `matched`, jobs, endpoints and a memory limit.

Step 16c-1 added the status views (ADR 0051), so most of this has a server answer now. The schema has none. The step expected label and type counts to come from the core's label index if it has an O(1) count, and keys to stay sampled.

What the pinned core (`7e7b7fa`) offers:

- `Graph::label_count(name)`: O(1).
- No way to list the labels a graph has. `Symbols` interns labels and edge types together, has no iterator, and `Symbol` is private.
- No index of edges by type.

## Decision

**A `schema` read on the `Database` trait**: `GetSchema` over gRPC, `GET /v1/namespaces/{ns}/schema` over REST, role `read`. It reads a sample: the first `max_visited` nodes and the first `max_edges` edges, in the core's slot order. It answers:

- **Labels**: those of the sampled nodes and those named by a constraint. Each comes with its exact node count (`label_count`, O(1)), the number of sampled nodes that have it, and the attribute keys of those nodes with a count per value kind. Keys are capped at 256 per label (`more_keys`).
- **Edge types**: those of the sampled edges, with their count in the sample. Untyped edges have no name.
- **Sizes**: the namespace's node and edge counts, and the sampled ones. The lists are complete, and every count exact, when the sample covers the namespace.

Reaching a limit ends the sample, not the read: the answer is never `budget_exceeded` and never `truncated`. A caller compares the sampled counts with the totals. Limits of 0 are refused, and limits above the caps are lowered, like every read. It is O(sample + labels) under the namespace's read lock, and it times out and can be cancelled like any read.

It is a `Database` method, not an `Admin` one, because it reads a namespace's data. It is in the conformance suite (`schema_counts_labels_and_samples_keys_and_types`, `schema_is_bounded`, `every_read_times_out`). The Python bindings don't expose it yet; they didn't get the `Admin` reads either.

**Upstream**: [#60](https://github.com/p-sodmann/Ironweaver/issues/60) ([draft 24](../upstream-issues.md#24-list-a-graphs-labels-and-count-its-edges-by-type)) proposes listing a graph's labels and counting edges by type. With it, the label list becomes complete and the type counts exact without a sample. The sample stays for keys.

*Update, upstream check of 2026-10-07: #60 was fixed in core `c69ef51` (`Graph::labels`, `edge_types`, `edge_type_count`).* `read::schema` now lists every label (with the constraints' labels) and every edge type from the core, with exact counts, in O(labels + types). The sample of the first `max_visited` nodes is only for the keys; `max_edges` no longer applies and no edge is read. `sampled_edges` stays in the answer, always equal to `edges`, so clients that compare it see complete lists. To keep the read bounded with any number of labels, at most 10 000 labels and 10 000 types are listed (`MAX_NAMES`); more set the answer's `truncated`, which it never was before. `schema_is_bounded` checks that labels and types are complete at a sample of 3 nodes. The console no longer sends `max_edges` and dropped the "counted in the first N edges" note.

**The console's Source contract follows the server** (`console/src/source.js`):

- **No unbounded counts.** `find` has no total and explain no `matched` (design rule 5). The pager counts pages as it goes, and a plan shows a match count only when the first page holds every match.
- **`server()` is the status views.** It is built from `GET /v1/status`, `/v1/requests`, `/v1/consumers` and `/v1/metrics`, in the server's shapes. The mock returns the same shape (a test compares the keys).
- **Latencies come from the histograms.** The latency per operation is estimated from the metrics' buckets since the start, as Prometheus' `histogram_quantile` does it.
- **Series come from differences.** The status page's series for the last minute and a half are the differences between one poll's metrics and the previous one's. The server keeps no history, and this needs none.
- **The log is the server's** for a server-wide admin (`GET /v1/log`, polled). For anyone else it is the page's own requests, and the page says so (`logKind()`).
- **What has no server answer is gone from the mock.** Jobs come with step 16f, so the panel shows index builds only. The memory limit and its thresholds come with step 16d. Endpoints and the data directory are gone; the Source's `endpoint` says where the page reads from.

## Consequences

- ~~A label that only nodes outside the sample carry is missing from the list until the sample grows or draft 24 lands.~~ Since core `c69ef51` (#60) every label and type is listed with its exact count. With the console's sample (10 000 nodes, 100 000 edges), a namespace up to that size is listed completely, and the structure view says when it isn't.
- Label counts are exact at any size, so the explorer's primary label and the navigator's counts are right on large namespaces too. Key presence is a fraction of the sampled nodes with the label, not of all of them.
- The console's latencies are bucket estimates, at most one bucket wide. The series need two polls before they show anything, and they restart with the page.
- The running-requests table lists the page's own status calls while they run: they are real requests.
