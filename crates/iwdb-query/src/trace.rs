//! The request's trace span (step 16g, ADR 0057), opened by the
//! authorisation point ([`Authorized`](crate::Authorized)) for every call it
//! checks, and what it records: the operation, the namespace, the request's
//! id, its bounds, what its answer reports, and its outcome.
//!
//! The span's name is `request`, and `otel.name` gives the exporter the
//! operation's name (`Find`, `Commit`, ...): a closed set of fixed strings.
//! Never a user, a client address, an id of the data, a value or an error
//! message.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use iwdb_engine::CommitResult;
use tracing::Span;
use tracing::field::Empty;

use crate::auth::Operation;
use crate::{Answer, Bounds, Code};

/// The span of a call of `op` (on `namespace`, if it has one). Disabled,
/// at the cost of a callsite check, when no subscriber wants trace spans.
pub(crate) fn request(op: Operation, namespace: Option<&str>) -> Span {
    let span = iwdb_storage::trace_span!(
        "request",
        otel.name = op.name(),
        otel.kind = "server",
        otel.status_code = Empty,
        db.system.name = "ironweaver_db",
        db.operation.name = op.name(),
        db.namespace = Empty,
        iwdb.request_id = Empty,
        iwdb.outcome = Empty,
        iwdb.max_results = Empty,
        iwdb.max_visited = Empty,
        iwdb.max_edges = Empty,
        iwdb.timeout_ms = Empty,
        iwdb.seq = Empty,
        iwdb.visited = Empty,
        iwdb.edges = Empty,
        iwdb.truncated = Empty,
    );
    if let Some(namespace) = namespace.filter(|_| !span.is_disabled()) {
        span.record("db.namespace", namespace);
    }
    span
}

/// Record the outcome: `ok`, or the error code (and the span's status
/// `error`, without a description: a message can quote data).
pub(crate) fn outcome(span: &Span, code: Option<Code>) {
    match code {
        None => {
            span.record("iwdb.outcome", "ok");
        }
        Some(code) => {
            span.record("iwdb.outcome", code.as_str());
            span.record("otel.status_code", "error");
        }
    }
}

/// Record a read's resolved bounds on the current span, if it is a trace
/// span (only the request's has the fields). Not `tracing::enabled!`: when
/// every layer says no, tracing-subscriber's per-layer filters then drop
/// the next event on the thread.
pub(crate) fn bounds(bounds: &Bounds, timeout: Duration) {
    let span = Span::current();
    if !span.metadata().is_some_and(|m| m.target() == iwdb_storage::trace::TARGET) {
        return;
    }
    span.record("iwdb.max_results", bounds.max_results);
    span.record("iwdb.max_visited", bounds.max_visited);
    span.record("iwdb.max_edges", bounds.max_edges);
    span.record("iwdb.timeout_ms", u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX));
}

/// What a read's answer reports: the seq it saw, its work, whether a limit
/// cut it.
pub(crate) fn answer<T>(answer: &Answer<T>, span: &Span) {
    span.record("iwdb.seq", answer.seq);
    span.record("iwdb.visited", answer.work.visited);
    span.record("iwdb.edges", answer.work.edges);
    span.record("iwdb.truncated", answer.truncated);
}

/// A commit's seq.
pub(crate) fn committed(result: &CommitResult, span: &Span) {
    span.record("iwdb.seq", result.seq);
}

/// Answers that report nothing for the span.
pub(crate) fn nothing<T>(_: &T, _: &Span) {}

/// The trace exporter's counts (ADR 0057), read by the metrics: spans
/// exported, and spans dropped because the queue was full or their export
/// failed. The server's exporter adds to them; they stay 0 without one.
#[derive(Debug, Default)]
pub struct SpanCounters {
    exported: AtomicU64,
    queue_full: AtomicU64,
    export_failed: AtomicU64,
}

impl SpanCounters {
    pub fn exported(&self, n: u64) {
        self.exported.fetch_add(n, Ordering::Relaxed);
    }

    pub fn queue_full(&self, n: u64) {
        self.queue_full.fetch_add(n, Ordering::Relaxed);
    }

    pub fn export_failed(&self, n: u64) {
        self.export_failed.fetch_add(n, Ordering::Relaxed);
    }

    /// Exported, dropped at the queue, dropped by a failed export.
    pub fn read(&self) -> (u64, u64, u64) {
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        (get(&self.exported), get(&self.queue_full), get(&self.export_failed))
    }
}
