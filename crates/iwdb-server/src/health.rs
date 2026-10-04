//! The server's health (step 16b, ADR 0040): where it is in its life
//! ([`Phase`]), decided once here and served three ways:
//!
//! - gRPC: the standard `grpc.health.v1.Health` service (Check and Watch),
//!   for service `""` and `ironweaver_db.v1.DatabaseService`: `SERVING` when
//!   ready, `NOT_SERVING` otherwise.
//! - REST: `GET /v1/health/live` (200 whenever the process answers) and
//!   `GET /v1/health/ready` (200 when ready, 503 otherwise), with a `Health`
//!   message as the body. Served in every build, also without the `rest`
//!   feature, since probes need them.
//! - [`probe`]: the readiness route over HTTP/1.1, for `iwdb-server --probe`
//!   (the Docker image's `HEALTHCHECK`).
//!
//! Health is the server's, not the [`Database`](iwdb_query::Database)
//! trait's: while recovery runs there is no database to ask yet.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_server::HealthServer;
use tonic_health::pb::{HealthCheckRequest, HealthCheckResponse};

/// The gRPC service whose readiness the health service reports, besides
/// the whole server's (`""`).
pub const DATABASE_SERVICE: &str = "ironweaver_db.v1.DatabaseService";
/// The REST liveness route.
pub const LIVE_PATH: &str = "/v1/health/live";
/// The REST readiness route.
pub const READY_PATH: &str = "/v1/health/ready";
/// The path prefix of the gRPC health service.
pub(crate) const GRPC_PREFIX: &str = "/grpc.health.v1.Health/";

/// Where the server is in its life. Only [`Ready`](Phase::Ready) serves
/// database calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The port is open and the store is being opened (recovery).
    Recovering,
    /// Recovery has finished and shutdown hasn't begun.
    Ready,
    /// Shutdown has begun.
    Draining,
}

impl Phase {
    /// The `HealthState` enum value's name in the proto (proto3 JSON).
    pub fn as_proto_name(self) -> &'static str {
        match self {
            Phase::Recovering => "HEALTH_STATE_RECOVERING",
            Phase::Ready => "HEALTH_STATE_READY",
            Phase::Draining => "HEALTH_STATE_DRAINING",
        }
    }

    /// The word logs and messages use.
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Recovering => "recovering",
            Phase::Ready => "ready",
            Phase::Draining => "draining",
        }
    }

    fn serving(self) -> ServingStatus {
        if self == Phase::Ready { ServingStatus::Serving } else { ServingStatus::NotServing }
    }
}

/// The server's health: one per server, cloned into everything that
/// reports or reads it.
#[derive(Clone, Debug)]
pub struct Health {
    phase: watch::Sender<Phase>,
}

impl Health {
    /// A health in `phase`.
    pub fn new(phase: Phase) -> Health {
        Health { phase: watch::Sender::new(phase) }
    }

    pub fn phase(&self) -> Phase {
        *self.phase.borrow()
    }

    pub fn ready(&self) -> bool {
        self.phase() == Phase::Ready
    }

    /// Move to `phase`: the REST routes, gRPC checks and gRPC watchers see
    /// it at once.
    pub fn set(&self, phase: Phase) {
        self.phase.send_replace(phase);
    }

    /// Follow the phase.
    pub fn subscribe(&self) -> watch::Receiver<Phase> {
        self.phase.subscribe()
    }

    /// The gRPC health service over this health.
    pub(crate) fn grpc_service(&self) -> HealthServer<GrpcHealth> {
        HealthServer::new(GrpcHealth { phase: self.phase.subscribe() })
    }

    /// The REST answer for `path` (one of [`LIVE_PATH`] and [`READY_PATH`]):
    /// the status and the `Health` message in proto3 JSON.
    pub(crate) fn rest_answer(&self, path: &str) -> (http::StatusCode, String) {
        let phase = self.phase();
        let status = if path == READY_PATH && phase != Phase::Ready {
            http::StatusCode::SERVICE_UNAVAILABLE
        } else {
            http::StatusCode::OK
        };
        (status, health_json(phase))
    }
}

/// `grpc.health.v1.Health` over the phase.
pub(crate) struct GrpcHealth {
    phase: watch::Receiver<Phase>,
}

impl GrpcHealth {
    fn known(service: &str) -> bool {
        service.is_empty() || service == DATABASE_SERVICE
    }
}

#[tonic::async_trait]
impl tonic_health::pb::health_server::Health for GrpcHealth {
    async fn check(
        &self,
        request: tonic::Request<HealthCheckRequest>,
    ) -> Result<tonic::Response<HealthCheckResponse>, tonic::Status> {
        if !Self::known(&request.get_ref().service) {
            return Err(tonic::Status::not_found("unknown service"));
        }
        let status = self.phase.borrow().serving();
        Ok(tonic::Response::new(HealthCheckResponse { status: status as i32 }))
    }

    type WatchStream = ReceiverStream<Result<HealthCheckResponse, tonic::Status>>;

    /// The status now, then each change, until the client goes away or the
    /// server begins to shut down (after sending `NOT_SERVING`); the
    /// protocol's `SERVICE_UNKNOWN` for other services.
    async fn watch(
        &self,
        request: tonic::Request<HealthCheckRequest>,
    ) -> Result<tonic::Response<Self::WatchStream>, tonic::Status> {
        let known = Self::known(&request.get_ref().service);
        let mut phase = self.phase.clone();
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            loop {
                let now = *phase.borrow_and_update();
                let status = if known { now.serving() } else { ServingStatus::ServiceUnknown };
                if tx.send(Ok(HealthCheckResponse { status: status as i32 })).await.is_err() {
                    return;
                }
                // Shutting down: the watcher has seen NOT_SERVING; ending
                // the stream lets the drain finish (like the change streams)
                if now == Phase::Draining {
                    return;
                }
                tokio::select! {
                    changed = phase.changed() => if changed.is_err() { return },
                    () = tx.closed() => return,
                }
            }
        });
        Ok(tonic::Response::new(ReceiverStream::new(rx)))
    }
}

/// The `Health` message of `phase` in proto3 JSON, as pbjson writes it
/// (written by hand, so it needs no `rest` feature; a test compares).
pub(crate) fn health_json(phase: Phase) -> String {
    let ready = if phase == Phase::Ready { r#","ready":true"# } else { "" };
    format!(r#"{{"state":"{}"{}}}"#, phase.as_proto_name(), ready)
}

/// Whether the server at `address` is ready: a `GET /v1/health/ready` over
/// HTTP/1.1 answered 200 within `timeout`. Errors say why not (no answer,
/// another status).
pub fn probe(address: &str, timeout: Duration) -> Result<(), String> {
    let addresses: Vec<SocketAddr> = address.to_socket_addrs().map_err(|e| format!("{}: {}", address, e))?.collect();
    let target = addresses.first().ok_or_else(|| format!("{}: no address", address))?;
    let mut stream = TcpStream::connect_timeout(target, timeout).map_err(|e| format!("{}: {}", address, e))?;
    stream.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    let request = format!("GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", READY_PATH, address);
    stream.write_all(request.as_bytes()).map_err(|e| format!("{}: {}", address, e))?;
    let mut answer = Vec::new();
    // A small answer: read until the server closes, at most 4 KiB
    let _ = stream.take(4096).read_to_end(&mut answer);
    let text = String::from_utf8_lossy(&answer);
    let status = text.lines().next().unwrap_or_default();
    if status.split_whitespace().nth(1) == Some("200") {
        Ok(())
    } else if status.is_empty() {
        Err(format!("{}: no answer", address))
    } else {
        let body = text.split("\r\n\r\n").nth(1).unwrap_or_default().trim();
        Err(format!("{}: {} {}", address, status, body))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[cfg(feature = "rest")]
    #[test]
    fn the_hand_written_json_is_pbjsons() {
        use crate::proto as pb;
        for (phase, state) in [
            (Phase::Recovering, pb::HealthState::Recovering),
            (Phase::Ready, pb::HealthState::Ready),
            (Phase::Draining, pb::HealthState::Draining),
        ] {
            let message = pb::Health { state: state as i32, ready: phase == Phase::Ready };
            assert_eq!(health_json(phase), serde_json::to_string(&message).unwrap());
        }
    }

    #[test]
    fn readiness_is_the_ready_phase_only() {
        let health = Health::new(Phase::Recovering);
        assert_eq!(health.rest_answer(READY_PATH).0, http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(health.rest_answer(LIVE_PATH).0, http::StatusCode::OK);
        health.set(Phase::Ready);
        assert_eq!(health.rest_answer(READY_PATH), (http::StatusCode::OK, health_json(Phase::Ready)));
        health.set(Phase::Draining);
        assert_eq!(health.rest_answer(READY_PATH).0, http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(health.rest_answer(LIVE_PATH).0, http::StatusCode::OK);
    }

    #[test]
    fn probing_nothing_fails() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        drop(listener);
        assert!(probe(&address, Duration::from_millis(500)).is_err());
        assert!(probe("not an address", Duration::from_millis(500)).is_err());
    }
}
