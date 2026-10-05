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

use std::io::IsTerminal;

use serde::{Deserialize, Serialize};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Layer, fmt};

use crate::audit::{AuditFiles, TARGET};

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
/// names their target itself (`warn,iwdb::audit=off`).
fn filter(level: &str) -> Result<EnvFilter, String> {
    let filter = EnvFilter::builder().parse(level).map_err(|e| e.to_string())?;
    if level.contains(TARGET) {
        return Ok(filter);
    }
    let audit = format!("{}=info", TARGET).parse().map_err(|e| format!("{}", e))?;
    Ok(filter.add_directive(audit))
}

/// Install the process's logger: `format` lines on stderr, filtered by
/// `level`, and audit entries to `audit` too. Call once, early; later
/// calls fail (a logger is installed).
pub fn init(format: LogFormat, level: &str, audit: Option<AuditFiles>) -> Result<(), String> {
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
        .with(stderr.with_filter(filter))
        .with(files)
        .try_init()
        .map_err(|e| format!("installing the logger: {}", e))
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
        assert!(check_level("nonsense=[").is_err());
    }
}
