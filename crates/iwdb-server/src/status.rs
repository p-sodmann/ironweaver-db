//! Errors on the wire: an [`Error`]'s [`Code`] becomes the gRPC status that
//! `documentation/api/errors.md` lists for it, and travels itself, as its
//! string, in the trailing metadata key [`CODE_KEY`]. Over REST it becomes
//! the HTTP status errors.md lists, with the code in the body (the `Error`
//! message). Clients branch on the code, never on the message: several
//! codes share a gRPC or HTTP status.
//!
//! This is the only place that maps codes (design rule 8).

use iwdb_query::{Code, Error};
use tonic::metadata::{MetadataMap, MetadataValue};

/// The metadata key that carries the error's code (`budget_exceeded`, ...).
pub const CODE_KEY: &str = "iwdb-code";

/// The gRPC status of `code` (the "gRPC" column of errors.md).
pub fn grpc_code(code: Code) -> tonic::Code {
    match code {
        Code::InvalidArgument => tonic::Code::InvalidArgument,
        Code::NotFound => tonic::Code::NotFound,
        Code::Conflict => tonic::Code::Aborted,
        Code::ConstraintViolation => tonic::Code::FailedPrecondition,
        Code::BudgetExceeded => tonic::Code::ResourceExhausted,
        Code::Timeout => tonic::Code::DeadlineExceeded,
        Code::Cancelled => tonic::Code::Cancelled,
        Code::CursorExpired => tonic::Code::FailedPrecondition,
        Code::NotRetained => tonic::Code::OutOfRange,
        Code::ReadOnly | Code::Unavailable | Code::Io => tonic::Code::Unavailable,
        Code::Corrupt => tonic::Code::DataLoss,
        Code::Internal => tonic::Code::Internal,
        // A code added later: its own string still travels in `CODE_KEY`
        _ => tonic::Code::Unknown,
    }
}

/// The HTTP status of `code` (the "HTTP" column of errors.md). 499 is
/// nginx's "client closed request": no standard status says "cancelled".
pub fn http_status(code: Code) -> http::StatusCode {
    use http::StatusCode as S;
    match code {
        Code::InvalidArgument => S::BAD_REQUEST,
        Code::NotFound => S::NOT_FOUND,
        Code::Conflict | Code::ConstraintViolation => S::CONFLICT,
        Code::BudgetExceeded => S::UNPROCESSABLE_ENTITY,
        Code::Timeout => S::GATEWAY_TIMEOUT,
        Code::Cancelled => S::from_u16(499).unwrap_or(S::BAD_REQUEST),
        Code::CursorExpired | Code::NotRetained => S::GONE,
        Code::ReadOnly | Code::Unavailable | Code::Io => S::SERVICE_UNAVAILABLE,
        Code::Corrupt | Code::Internal => S::INTERNAL_SERVER_ERROR,
        // A code added later: its own string still travels in the body
        _ => S::INTERNAL_SERVER_ERROR,
    }
}

/// The error a REST answer stands for: the code of its body if it has
/// one this client knows, otherwise (a proxy's answer, a code added later)
/// the code closest to the HTTP status.
pub fn from_http(status: http::StatusCode, code: Option<&str>, message: &str) -> Error {
    let code = code.and_then(Code::parse).unwrap_or(match status.as_u16() {
        400 | 405 | 411 | 413 | 414 | 415 | 431 => Code::InvalidArgument,
        404 => Code::NotFound,
        409 => Code::Conflict,
        410 => Code::CursorExpired,
        422 => Code::BudgetExceeded,
        408 | 504 => Code::Timeout,
        499 => Code::Cancelled,
        429 | 502 | 503 => Code::Unavailable,
        _ => Code::Internal,
    });
    Error::new(code, message)
}

/// The status a failed call ends with.
pub fn to_status(e: &Error) -> tonic::Status {
    log_server_error(e);
    let mut metadata = MetadataMap::new();
    metadata.insert(CODE_KEY, MetadataValue::from_static(e.code().as_str()));
    tonic::Status::with_metadata(grpc_code(e.code()), e.message(), metadata)
}

/// Log the errors that are the server's, not the caller's: `internal`,
/// `corrupt` and `io` (ADR 0042). Called where an error becomes an answer,
/// inside the request's span, so the event carries its path.
pub(crate) fn log_server_error(e: &Error) {
    if matches!(e.code(), Code::Internal | Code::Corrupt | Code::Io) {
        tracing::error!(code = e.code().as_str(), error = e.message(), "a request failed on the server's side");
    }
}

/// The error a status stands for: the code in [`CODE_KEY`] if the server
/// sent one, otherwise (a status from the transport: a lost connection, a
/// client-side deadline, a message over the size limit) the code closest
/// to the gRPC status.
pub fn from_status(status: &tonic::Status) -> Error {
    let sent = status.metadata().get(CODE_KEY).and_then(|v| v.to_str().ok()).and_then(Code::parse);
    let code = sent.unwrap_or(match status.code() {
        tonic::Code::InvalidArgument => Code::InvalidArgument,
        tonic::Code::OutOfRange => Code::NotRetained,
        tonic::Code::NotFound => Code::NotFound,
        tonic::Code::Aborted | tonic::Code::AlreadyExists => Code::Conflict,
        tonic::Code::DeadlineExceeded => Code::Timeout,
        tonic::Code::Cancelled => Code::Cancelled,
        tonic::Code::Unavailable => Code::Unavailable,
        tonic::Code::DataLoss => Code::Corrupt,
        _ => Code::Internal,
    });
    Error::new(code, status.message())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table of `documentation/api/errors.md`, written out again: a
    /// change to either must change this test.
    const TABLE: [(Code, tonic::Code); 14] = [
        (Code::InvalidArgument, tonic::Code::InvalidArgument),
        (Code::NotFound, tonic::Code::NotFound),
        (Code::Conflict, tonic::Code::Aborted),
        (Code::ConstraintViolation, tonic::Code::FailedPrecondition),
        (Code::BudgetExceeded, tonic::Code::ResourceExhausted),
        (Code::Timeout, tonic::Code::DeadlineExceeded),
        (Code::Cancelled, tonic::Code::Cancelled),
        (Code::CursorExpired, tonic::Code::FailedPrecondition),
        (Code::NotRetained, tonic::Code::OutOfRange),
        (Code::ReadOnly, tonic::Code::Unavailable),
        (Code::Unavailable, tonic::Code::Unavailable),
        (Code::Io, tonic::Code::Unavailable),
        (Code::Corrupt, tonic::Code::DataLoss),
        (Code::Internal, tonic::Code::Internal),
    ];

    #[test]
    fn every_code_maps_to_the_status_of_errors_md_and_back() {
        assert_eq!(TABLE.map(|(c, _)| c), Code::ALL);
        for (code, grpc) in TABLE {
            let e = Error::new(code, format!("a {} error: ü ✓\nsecond line", code));
            let status = to_status(&e);
            assert_eq!(status.code(), grpc, "{}", code);
            assert_eq!(status.metadata().get(CODE_KEY).and_then(|v| v.to_str().ok()), Some(code.as_str()));
            assert_eq!(from_status(&status), e);
        }
    }

    /// The HTTP column of errors.md.
    const HTTP: [(Code, u16); 14] = [
        (Code::InvalidArgument, 400),
        (Code::NotFound, 404),
        (Code::Conflict, 409),
        (Code::ConstraintViolation, 409),
        (Code::BudgetExceeded, 422),
        (Code::Timeout, 504),
        (Code::Cancelled, 499),
        (Code::CursorExpired, 410),
        (Code::NotRetained, 410),
        (Code::ReadOnly, 503),
        (Code::Unavailable, 503),
        (Code::Io, 503),
        (Code::Corrupt, 500),
        (Code::Internal, 500),
    ];

    #[test]
    fn every_code_maps_to_the_http_status_of_errors_md_and_back() {
        assert_eq!(HTTP.map(|(c, _)| c), Code::ALL);
        let doc = include_str!("../../../documentation/api/errors.md");
        for (code, http) in HTTP {
            let status = http_status(code);
            assert_eq!(status.as_u16(), http, "{}", code);
            assert_eq!(from_http(status, Some(code.as_str()), "m"), Error::new(code, "m"));
            let row = doc.lines().find(|l| l.starts_with(&format!("| `{}` |", code))).expect("a row");
            assert!(row.trim_end().ends_with(&format!("| {} |", http)), "{}: {} not in {}", code, http, row);
        }
        // Without a code (or one this client doesn't know): by the status
        assert_eq!(from_http(http::StatusCode::BAD_GATEWAY, None, "proxy").code(), Code::Unavailable);
        assert_eq!(from_http(http::StatusCode::NOT_FOUND, Some("from_the_future"), "x").code(), Code::NotFound);
        assert_eq!(from_http(http::StatusCode::IM_A_TEAPOT, None, "?").code(), Code::Internal);
    }

    #[test]
    fn errors_md_lists_the_implemented_mapping() {
        let doc = include_str!("../../../documentation/api/errors.md");
        for (code, grpc) in TABLE {
            let row = doc
                .lines()
                .find(|l| l.starts_with(&format!("| `{}` |", code)))
                .unwrap_or_else(|| panic!("errors.md has no row for {}", code));
            let name = format!("{:?}", grpc);
            let screaming: String = name
                .chars()
                .enumerate()
                .flat_map(|(i, c)| {
                    let sep = (i > 0 && c.is_uppercase()).then_some('_');
                    sep.into_iter().chain(c.to_uppercase())
                })
                .collect();
            assert!(row.contains(&format!("`{}`", screaming)), "{}: {} not in {}", code, screaming, row);
        }
    }

    #[test]
    fn a_status_without_a_code_maps_by_its_grpc_status() {
        let cases = [
            (tonic::Status::unavailable("connection refused"), Code::Unavailable),
            (tonic::Status::deadline_exceeded("deadline"), Code::Timeout),
            (tonic::Status::cancelled("Timeout expired"), Code::Cancelled),
            (tonic::Status::resource_exhausted("message too large"), Code::Internal),
            (tonic::Status::unknown("?"), Code::Internal),
        ];
        for (status, code) in cases {
            assert_eq!(from_status(&status).code(), code, "{:?}", status);
        }
        // A code this client doesn't know falls back to the status as well
        let mut metadata = MetadataMap::new();
        metadata.insert(CODE_KEY, MetadataValue::from_static("from_the_future"));
        let status = tonic::Status::with_metadata(tonic::Code::NotFound, "x", metadata);
        assert_eq!(from_status(&status).code(), Code::NotFound);
    }
}
