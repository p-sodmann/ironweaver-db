//! Trace spans (step 16g, [ADR 0057](../../../documentation/adr/0057-traces.md)).
//!
//! Every span of the database has the target [`TARGET`] and level `INFO`,
//! and a fixed name (`iwdb.commit`, `iwdb.wal.fsync`, ...): never an id, a
//! namespace's name or a value. [`trace_span!`](crate::trace_span) makes
//! one; the storage, query and store crates use it.
//!
//! The server's log filter turns the target off, so spans only reach an
//! OpenTelemetry exporter (feature `otel` of `iwdb-server`). With no
//! subscriber interested, a span's callsite is disabled: making one costs an
//! atomic load, and its fields aren't evaluated.

/// The target of every trace span.
pub const TARGET: &str = "iwdb::trace";

#[doc(hidden)]
pub use tracing as __tracing;

/// A trace span: `tracing::info_span!` with the target [`TARGET`]. The name
/// must be a literal; fields follow as in `info_span!`.
#[macro_export]
macro_rules! trace_span {
    (parent: $parent:expr, $name:literal $(, $($fields:tt)*)?) => {
        $crate::trace::__tracing::info_span!(target: "iwdb::trace", parent: $parent, $name $(, $($fields)*)?)
    };
    ($name:literal $(, $($fields:tt)*)?) => {
        $crate::trace::__tracing::info_span!(target: "iwdb::trace", $name $(, $($fields)*)?)
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_macro_uses_the_target() {
        assert_eq!(super::TARGET, "iwdb::trace");
        // Without a subscriber the span is disabled
        assert!(crate::trace_span!("iwdb.test", iwdb.n = 1).is_disabled());
    }
}
