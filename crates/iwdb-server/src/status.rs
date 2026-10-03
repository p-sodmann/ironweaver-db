//! Errors on the wire: an [`Error`]'s [`Code`] becomes the gRPC status that
//! `documentation/api/errors.md` lists for it, and travels itself, as its
//! string, in the trailing metadata key [`CODE_KEY`]. Clients branch on
//! that, never on the message: several codes share a gRPC status.
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
        Code::ReadOnly | Code::Unavailable | Code::Io => tonic::Code::Unavailable,
        Code::Corrupt => tonic::Code::DataLoss,
        Code::Internal => tonic::Code::Internal,
        // A code added later: its own string still travels in `CODE_KEY`
        _ => tonic::Code::Unknown,
    }
}

/// The status a failed call ends with.
pub fn to_status(e: &Error) -> tonic::Status {
    let mut metadata = MetadataMap::new();
    metadata.insert(CODE_KEY, MetadataValue::from_static(e.code().as_str()));
    tonic::Status::with_metadata(grpc_code(e.code()), e.message(), metadata)
}

/// The error a status stands for: the code in [`CODE_KEY`] if the server
/// sent one, otherwise (a status from the transport: a lost connection, a
/// client-side deadline, a message over the size limit) the code closest
/// to the gRPC status.
pub fn from_status(status: &tonic::Status) -> Error {
    let sent = status.metadata().get(CODE_KEY).and_then(|v| v.to_str().ok()).and_then(Code::parse);
    let code = sent.unwrap_or(match status.code() {
        tonic::Code::InvalidArgument | tonic::Code::OutOfRange => Code::InvalidArgument,
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
    const TABLE: [(Code, tonic::Code); 13] = [
        (Code::InvalidArgument, tonic::Code::InvalidArgument),
        (Code::NotFound, tonic::Code::NotFound),
        (Code::Conflict, tonic::Code::Aborted),
        (Code::ConstraintViolation, tonic::Code::FailedPrecondition),
        (Code::BudgetExceeded, tonic::Code::ResourceExhausted),
        (Code::Timeout, tonic::Code::DeadlineExceeded),
        (Code::Cancelled, tonic::Code::Cancelled),
        (Code::CursorExpired, tonic::Code::FailedPrecondition),
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
