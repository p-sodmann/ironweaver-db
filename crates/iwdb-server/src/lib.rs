//! Ironweaver DB's gRPC server: the [`Database`](iwdb_query::Database)
//! trait over gRPC, with `proto/ironweaver_db/v1` as the contract.
//!
//! - [`Server`]: serves any `D: Database`. Each RPC translates its request
//!   into one trait call and the answer back (design rule 8): limits,
//!   deadlines, cursors and errors are the trait's.
//! - [`status`]: the error mapping, `Code` to gRPC status and back, with the
//!   code string in the `iwdb-code` trailer (`documentation/api/errors.md`).
//! - [`config`]: the `iwdb-server` binary's config file.
//! - [`proto`]: the generated messages, server and client.
//! - `client` (feature `client`): `client::Remote`, the trait over gRPC.
//!
//! Values, filters and patterns travel in the core's serde form, encoded
//! with postcard (ADR 0023). Streaming RPCs send one answer in chunks
//! (ADR 0025). Reads honour `grpc-timeout` (ADR 0026). Shutdown drains
//! running calls, then cancels the rest (ADR 0027).
//!
//! Pure Rust (design rule 1). tokio is a dependency of this crate only
//! (ADR 0020).

pub mod config;
mod convert;
mod serve;
mod service;
pub mod status;

#[cfg(feature = "client")]
pub mod client;

pub use serve::Drain;
pub use service::{Adapter, Server, CHUNK_BYTES, DEFAULT_MAX_MESSAGE_BYTES};

/// The generated messages and services of `ironweaver_db.v1`.
#[allow(clippy::all, clippy::pedantic, missing_docs, rustdoc::all)]
pub mod proto {
    tonic::include_proto!("ironweaver_db.v1");
}
