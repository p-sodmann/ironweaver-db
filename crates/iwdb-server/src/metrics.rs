//! `GET /metrics` (step 16c, ADR 0050): the metrics in Prometheus' text
//! format, served by the gate in every build (also without the REST API),
//! like health. Prometheus pulls them; the server pushes nothing.
//!
//! It needs credentials like any call (an API token as
//! `Authorization: Bearer`, Prometheus' `authorization.credentials_file`),
//! and runs `GetMetrics` through the authorisation point: a caller sees
//! the series of the namespaces it has a role on, and those of none.

use std::sync::Arc;

use iwdb_query::Error;
use iwdb_query::audit::AuditSink;

use crate::auth::{AuthMode, Caller, Served, authorized};
use crate::ops;

/// The path of the metrics in Prometheus' text format.
pub const METRICS_PATH: &str = "/metrics";
/// Their media type: the text exposition format, version 0.0.4.
pub const PROMETHEUS_TEXT: &str = "text/plain; version=0.0.4; charset=utf-8";

/// The answer to `GET /metrics`: the status and the body (the metrics, or
/// an error's code and message as text).
pub(crate) async fn answer<D: Served>(
    db: &Arc<D>,
    mode: AuthMode,
    caller: Option<&Caller>,
    audit: &Arc<dyn AuditSink>,
) -> (http::StatusCode, &'static str, String) {
    let result: Result<String, Error> = async { ops::metrics_text(&authorized(db, mode, caller, audit)?).await }.await;
    match result {
        Ok(text) => (http::StatusCode::OK, PROMETHEUS_TEXT, text),
        Err(e) => failure(&e),
    }
}

/// An error as `/metrics` answers it: the HTTP status of its code and an
/// `Error` body, as every REST route (written here: this route is served
/// without the REST API's JSON too).
pub(crate) fn failure(e: &Error) -> (http::StatusCode, &'static str, String) {
    crate::status::log_server_error(e);
    let body = format!("{{\"code\":{},\"message\":{}}}", json_string(e.code().as_str()), json_string(e.message()));
    (crate::status::http_status(e.code()), "application/json", body)
}

/// `text` as a JSON string.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_error_bodies() {
        let (status, media, body) = failure(&Error::invalid("a \"quoted\" \\ line\nbreak"));
        assert_eq!((status.as_u16(), media), (400, "application/json"));
        assert_eq!(body, r#"{"code":"invalid_argument","message":"a \"quoted\" \\ line\u000abreak"}"#);
    }
}
