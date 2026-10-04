//! Structured logs (step 16b, ADR 0042): `tracing` events written by
//! `tracing-subscriber` to stderr, one JSON object per line or as text.
//!
//! The library crates log through the `log` facade and gain no dependency
//! (design rule 1): the subscriber bridges their records (`tracing-log`), so
//! they come out in the same format, with their module as `target`.
//!
//! A JSON line carries `timestamp` (RFC 3339, UTC), `level`, `target`,
//! `message` and the event's fields at the top level, and the fields of the
//! request it happened in (`span`: its `path`).

use std::io::IsTerminal;

use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;

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
    EnvFilter::builder().parse(level).map(|_| ()).map_err(|e| e.to_string())
}

/// Install the process's logger: `format` lines on stderr, filtered by
/// `level`. Call once, early; later calls fail (a logger is installed).
pub fn init(format: LogFormat, level: &str) -> Result<(), String> {
    let filter = EnvFilter::builder().parse(level).map_err(|e| format!("[log] level: {}", e))?;
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr);
    let result = match format.resolve() {
        LogFormat::Json => builder.json().flatten_event(true).with_current_span(true).with_span_list(false).try_init(),
        _ => builder.with_ansi(std::io::stderr().is_terminal()).try_init(),
    };
    result.map_err(|e| format!("installing the logger: {}", e))
}
