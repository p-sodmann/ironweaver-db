//! The conformance suite of the `Database` trait (`iwdb_query::conformance`)
//! over REST: every case gets a fresh store behind a server on an ephemeral
//! port, and runs through a `RestRemote` client, once with streamed answers
//! as one JSON message and once as NDJSON.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

mod json {
    use super::support;
    iwdb_query::conformance_tests!(support::fresh_rest(false));
}

mod ndjson {
    use super::support;
    iwdb_query::conformance_tests!(support::fresh_rest(true));
}
