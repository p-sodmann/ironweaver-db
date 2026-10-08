//! Structured logs (step 16b, ADR 0042): `tracing` events written by
//! `tracing-subscriber` to stderr, one JSON object per line or as text.
//!
//! The library crates log through the `log` facade and gain no dependency
//! (design rule 1): the subscriber bridges their records (`tracing-log`), so
//! they come out in the same format, with their module as `target`.
//!
//! Audit entries (target `iwdb::audit`, ADR 0049) pass whatever the level
//! says, unless the level names `iwdb::audit` itself; with `[audit] dir`
//! they also go to the audit files ([`crate::audit::AuditFiles`]).
//!
//! A JSON line carries `timestamp` (RFC 3339, UTC), `level`, `target`,
//! `message` and the event's fields at the top level, and the fields of the
//! request it happened in (`span`: its `path`).
//!
//! The log tail (step 16c, ADR 0051): every event that passes the level
//! filter also goes to a [`LogRing`] ([`RingLayer`]), which `GetLog` and the
//! console read. The same events as stderr, nothing more.

use std::fmt::Write as _;
use std::io::IsTerminal;
use std::sync::Arc;

use iwdb_engine::CommitTime;
use iwdb_query::log::{Level, LogRing};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;

use serde::{Deserialize, Serialize};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Layer, fmt};

use iwdb_storage::trace::TARGET as TRACE_TARGET;

use crate::audit::{AuditFiles, TARGET};

/// A layer over the bare registry: the trace exporter's.
pub type TraceLayer = Box<dyn Layer<tracing_subscriber::Registry> + Send + Sync>;

/// How log lines look.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// JSON when stderr isn't a terminal (a container, a service, a pipe),
    /// text when it is.
    #[default]
    Auto,
    Json,
    Text,
}

impl LogFormat {
    /// `Auto` decided for this process's stderr.
    pub fn resolve(self) -> LogFormat {
        match self {
            LogFormat::Auto if std::io::stderr().is_terminal() => LogFormat::Text,
            LogFormat::Auto => LogFormat::Json,
            other => other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LogFormat::Auto => "auto",
            LogFormat::Json => "json",
            LogFormat::Text => "text",
        }
    }
}

/// Check a level directive (`info`, `warn,iwdb_storage=debug`, ...), the
/// syntax of `tracing-subscriber`'s `EnvFilter`.
pub fn check_level(level: &str) -> Result<(), String> {
    filter(level).map(|_| ())
}

/// The filter of `level`, which lets audit entries through unless it
/// names their target itself (`warn,iwdb::audit=off`), and keeps trace
/// spans (ADR 0057) out of the log unless it names theirs.
fn filter(level: &str) -> Result<EnvFilter, String> {
    let mut filter = EnvFilter::builder().parse(level).map_err(|e| e.to_string())?;
    if !level.contains(TARGET) {
        filter = filter.add_directive(format!("{}=info", TARGET).parse().map_err(|e| format!("{}", e))?);
    }
    if !level.contains(TRACE_TARGET) {
        filter = filter.add_directive(format!("{}=off", TRACE_TARGET).parse().map_err(|e| format!("{}", e))?);
    }
    Ok(filter)
}

/// Install the process's logger: `format` lines on stderr, filtered by
/// `level`, the same events to `ring` (the log tail), and audit entries to
/// `audit` too. Call once, early; later calls fail (a logger is
/// installed).
/// `traces` is the layer that exports trace spans, if tracing is on (ADR
/// 0057, `otel::Tracing::layer`).
pub fn init(
    format: LogFormat,
    level: &str,
    audit: Option<AuditFiles>,
    ring: Arc<LogRing>,
    traces: Option<TraceLayer>,
) -> Result<(), String> {
    let ring = RingLayer { ring }.with_filter(filter(level).map_err(|e| format!("[log] level: {}", e))?);
    let filter = filter(level).map_err(|e| format!("[log] level: {}", e))?;
    let stderr = fmt::layer().with_writer(std::io::stderr);
    let stderr = match format.resolve() {
        LogFormat::Json => stderr.json().flatten_event(true).with_current_span(true).with_span_list(false).boxed(),
        _ => stderr.with_ansi(std::io::stderr().is_terminal()).boxed(),
    };
    let files = audit.map(|files| {
        fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .with_span_list(false)
            .with_writer(files)
            .with_filter(Targets::new().with_target(TARGET, LevelFilter::INFO))
    });
    tracing_subscriber::registry()
        .with(traces)
        .with(stderr.with_filter(filter))
        .with(ring)
        .with(files)
        .try_init()
        .map_err(|e| format!("installing the logger: {}", e))
}

/// A layer that keeps each event in a [`LogRing`]: its level, target,
/// message and fields (as text), not its span.
pub struct RingLayer {
    pub ring: Arc<LogRing>,
}

/// An event's message and fields, as text.
#[derive(Default)]
struct Fields {
    message: String,
    fields: Vec<(String, String)>,
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_owned();
        } else {
            self.fields.push((field.name().to_owned(), value.to_owned()));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let mut text = String::new();
        let _ = write!(text, "{:?}", value);
        if field.name() == "message" {
            self.message = text;
        } else {
            self.fields.push((field.name().to_owned(), text));
        }
    }
}

fn level_of(level: &tracing::Level) -> Level {
    match *level {
        tracing::Level::TRACE => Level::Trace,
        tracing::Level::DEBUG => Level::Debug,
        tracing::Level::INFO => Level::Info,
        tracing::Level::WARN => Level::Warn,
        tracing::Level::ERROR => Level::Error,
    }
}

impl<S: tracing::Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _cx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let meta = event.metadata();
        // Records bridged from the `log` facade carry their target as a field
        let target = fields
            .fields
            .iter()
            .position(|(name, _)| name == "log.target")
            .map(|i| fields.fields.remove(i).1)
            .unwrap_or_else(|| meta.target().to_owned());
        fields.fields.retain(|(name, _)| !name.starts_with("log."));
        self.ring.push(CommitTime::now(), level_of(meta.level()), &target, &fields.message, fields.fields);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_entries_pass_any_level_unless_it_names_them() {
        for level in ["info", "warn", "error", "off", "warn,iwdb_storage=debug"] {
            assert!(filter(level).expect("filter").to_string().contains("iwdb::audit=info"), "{}", level);
        }
        assert!(!filter("warn,iwdb::audit=off").expect("filter").to_string().contains("iwdb::audit=info"));
        // Trace spans stay out of the log at any level
        for level in ["info", "trace", "debug,iwdb_storage=trace"] {
            assert!(filter(level).expect("filter").to_string().contains("iwdb::trace=off"), "{}", level);
        }
        assert!(check_level("nonsense=[").is_err());
    }

    #[test]
    fn the_ring_keeps_what_passes_the_filter() {
        let ring = Arc::new(LogRing::new(10));
        let filter = filter("info").expect("filter");
        let subscriber = tracing_subscriber::registry().with(RingLayer { ring: ring.clone() }.with_filter(filter));
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!("hidden");
            tracing::warn!(namespace = "social", count = 3, "a warning");
            tracing::info!(target: "iwdb::audit", operation = "Grant", "audit");
        });
        let tail = ring.read(0, 10);
        assert_eq!(tail.events.len(), 2, "{:?}", tail);
        let e = &tail.events[0];
        assert_eq!((e.level, e.message.as_str()), (Level::Warn, "a warning"));
        assert_eq!(e.fields, vec![("namespace".into(), "social".into()), ("count".into(), "3".into())]);
        assert_eq!(tail.events[1].target, "iwdb::audit");
    }
}
