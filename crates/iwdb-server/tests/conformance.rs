//! The conformance suite of the `Database` trait (`iwdb_query::conformance`)
//! over gRPC: every case gets a fresh store behind a server on an ephemeral
//! port, and runs through a `Remote` client.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

iwdb_query::conformance_tests!(support::fresh());

mod admin {
    use super::support;
    iwdb_query::admin_conformance_tests!(support::fresh());
}
