//! Exercise the adapter without building a wheel. The binding library is a
//! cdylib with its Rust test harness disabled, so include its error adapter.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[allow(dead_code)]
#[path = "../src/errors.rs"]
mod errors;

use iwdb::Error;
use iwdb_query::Code;
use pyo3::prelude::*;

fn assert_exception(py: Python<'_>, error: PyErr, name: &str, message: &str) {
    assert_eq!(error.get_type(py).name().unwrap().to_string(), name);
    assert_eq!(error.value(py).str().unwrap().to_string(), message);
    assert!(error.is_instance_of::<errors::exc::Error>(py));
}

#[test]
fn storage_errors_use_shared_classification_and_keep_messages() {
    Python::initialize();
    Python::attach(|py| {
        // These exercise the old fallback cases as well as nested engine errors.
        let cases = [
            (Error::NotRetained { from: 1, first_seq: 9 }, "NotRetainedError"),
            (Error::OutOfOrder { expected: 2, found: 3 }, "InternalError"),
            (
                Error::Engine(iwdb_engine::Error::NotFound { target: iwdb_engine::Target::Node("missing".into()) }),
                "NotFoundError",
            ),
            (Error::Engine(iwdb_engine::Error::MissingVersion { entity: iwdb_engine::Entity::Graph }), "CorruptError"),
            (Error::Engine(iwdb_engine::Error::Poisoned), "ReadOnlyError"),
            (Error::Engine(iwdb_engine::Error::EmptyTransaction), "InvalidError"),
            (Error::Cancelled, "Error"),
        ];
        for (error, exception) in cases {
            let message = error.to_string();
            assert_exception(py, errors::to_py(error), exception, &message);
        }
    });
}

#[test]
fn locked_store_keeps_its_python_specific_exception() {
    Python::initialize();
    Python::attach(|py| {
        let error = Error::Locked { path: "store".into() };
        let message = error.to_string();
        assert_exception(py, errors::to_py(error), "LockedError", &message);
        // A query-level unavailable error remains the remote exception.
        assert_exception(
            py,
            errors::query_to_py(iwdb_query::Error::unavailable(&message)),
            "UnavailableError",
            &message,
        );
    });
}

#[test]
fn every_query_code_has_its_python_exception_and_keeps_its_message() {
    Python::initialize();
    Python::attach(|py| {
        let cases = [
            (Code::InvalidArgument, "InvalidError"),
            (Code::Unauthenticated, "UnauthenticatedError"),
            (Code::PermissionDenied, "PermissionDeniedError"),
            (Code::NotFound, "NotFoundError"),
            (Code::Conflict, "ConflictError"),
            (Code::ConstraintViolation, "ConstraintError"),
            (Code::BudgetExceeded, "BudgetExceededError"),
            (Code::Timeout, "TimeoutError"),
            (Code::Cancelled, "Error"),
            (Code::CursorExpired, "CursorExpiredError"),
            (Code::NotRetained, "NotRetainedError"),
            (Code::ReadOnly, "ReadOnlyError"),
            (Code::Unavailable, "UnavailableError"),
            (Code::ResourceExhausted, "ResourceExhaustedError"),
            (Code::Io, "IoError"),
            (Code::Corrupt, "CorruptError"),
            (Code::Internal, "InternalError"),
        ];
        assert_eq!(cases.len(), Code::ALL.len());
        for (code, exception) in cases {
            let message = format!("original {code} message");
            assert_exception(py, errors::query_to_py(iwdb_query::Error::new(code, &message)), exception, &message);
        }
    });
}
