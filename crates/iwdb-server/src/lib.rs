//! Ironweaver DB's server: the [`Database`](iwdb_query::Database) trait
//! over gRPC and REST/JSON, with `proto/ironweaver_db/v1` as the contract of
//! both.
//!
//! - [`Server`]: serves any `D: Database`, gRPC and REST on one port. Each
//!   operation translates its request into one trait call and the answer
//!   back, once for both APIs (`ops`, design rule 8): limits, deadlines,
//!   cursors and errors are the trait's.
//! - `rest` (feature `rest`, on by default): the REST routes (ADR 0030)
//!   and their OpenAPI document.
//! - [`status`]: the error mapping, `Code` to gRPC status and back, with the
//!   code string in the `iwdb-code` trailer, and to HTTP status
//!   (`documentation/api/errors.md`).
//! - [`config`]: the `iwdb-server` binary's configuration: a file and
//!   `IWDB_*` environment overrides (ADR 0039).
//! - [`health`]: liveness and readiness (ADR 0040); [`launch`] binds first
//!   and serves health while the store opens.
//! - [`logging`]: structured logs (ADR 0042).
//! - `console` (feature `console`): the operator console's pages, served
//!   at `/console/` when the config turns it on (ADR 0041).
//! - [`proto`]: the generated messages (with their proto3 JSON serde),
//!   server and client.
//! - `client` (feature `client`): `client::Remote`, the trait over gRPC, and
//!   (with `rest`) `client::RestRemote`, over REST.
//!
//! Features (ADR 0034): `rest` and `postgres` (the Postgres source of
//! `[[projection]]`s) are on by default; without them the server speaks
//! gRPC only and refuses Postgres projections in its config. `console`
//! (off by default, implies `rest`) compiles the operator console in.
//!
//! Values, filters and patterns travel in the core's serde form, encoded
//! with postcard over gRPC (ADR 0023) and as JSON over REST (ADR 0030).
//! Streaming RPCs send one answer in chunks (ADR 0025), REST as one JSON
//! message or NDJSON. Reads honour `grpc-timeout` (ADR 0026). Shutdown
//! drains running calls, then cancels the rest (ADR 0027).
//!
//! Pure Rust (design rule 1). tokio is a dependency of this crate only
//! (ADR 0020).

pub mod config;
#[cfg(feature = "console")]
mod console;
mod convert;
pub mod health;
pub mod logging;
mod ops;
#[cfg(feature = "rest")]
pub mod rest;
mod serve;
mod service;
pub mod status;

#[cfg(feature = "client")]
pub mod client;

pub use ops::CHUNK_BYTES;
pub use serve::{Drain, LaunchOptions, Launched, launch};
pub use service::{Adapter, DEFAULT_MAX_MESSAGE_BYTES, Server};

/// The generated messages and services of `ironweaver_db.v1`, and (with
/// `rest`) the proto3 JSON serde of the messages (pbjson; `Value`, `Expr`
/// and `Pattern` in `rest::json`).
#[allow(clippy::all, clippy::pedantic, missing_docs, rustdoc::all)]
pub mod proto {
    tonic::include_proto!("ironweaver_db.v1");
    #[cfg(feature = "rest")]
    include!(concat!(env!("OUT_DIR"), "/ironweaver_db.v1.serde.rs"));

    /// The encoded `FileDescriptorSet` of the protos, with their comments.
    #[cfg(feature = "rest")]
    pub(crate) const DESCRIPTORS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/descriptors.bin"));
}
