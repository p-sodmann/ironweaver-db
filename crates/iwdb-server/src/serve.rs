//! Serving a [`Server`] on a TCP listener, and shutting it down gracefully
//! (ADR 0027).
//!
//! The accept loop is ours rather than tonic's `transport::Server`, for two
//! reasons: tonic's server wraps every service in a `grpc-timeout` layer
//! that would race the database's own deadline and answer `CANCELLED`
//! instead of the trait's `timeout` (ADR 0026), and it spawns connections
//! where a shutdown can't reach them. Here every connection is a task of a
//! `JoinSet`: shutdown asks them to finish (HTTP/2 GOAWAY), and cancels the
//! ones still running at the end of the drain.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use iwdb_query::Database;
use tokio::net::TcpListener;
use tokio::task::JoinSet;

use crate::Server;

/// How a shutdown went ([`Server::serve`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drain {
    /// Every call finished within the drain.
    pub complete: bool,
    /// Connections that still had calls running at the end of the drain;
    /// they were closed, which cancelled their reads (commits that were
    /// accepted still run to the end on the database's workers).
    pub cancelled: usize,
}

impl<D: Database + 'static> Server<D> {
    /// Serve on `listener` until `stop` completes; then shut down (ADR
    /// 0027): close the listener, send every connection GOAWAY (new calls
    /// fail with `UNAVAILABLE`), and let running calls finish until the
    /// future that `drain` returns completes (a timeout, a second signal).
    /// Connections still open then are closed. Returns once every
    /// connection is gone.
    ///
    /// The database stays open: close it after this returns (the binary
    /// takes it back with [`into_database`](Self::into_database) and closes
    /// the store, which flushes the WAL).
    ///
    /// Errors: only from the listener's local address; failed accepts are
    /// logged and retried.
    pub async fn serve<S, F, G>(&self, listener: TcpListener, stop: S, drain: F) -> io::Result<Drain>
    where
        S: Future<Output = ()>,
        F: FnOnce() -> G,
        G: Future<Output = ()>,
    {
        let service = self.service();
        let builder = hyper::server::conn::http2::Builder::new(TokioExecutor::new());
        let graceful = GracefulShutdown::new();
        let mut connections = JoinSet::new();
        tokio::pin!(stop);
        loop {
            tokio::select! {
                () = &mut stop => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        // Small answers shouldn't wait for Nagle's algorithm
                        let _ = stream.set_nodelay(true);
                        let service = TowerToHyperService::new(service.clone());
                        let connection = graceful.watch(builder.serve_connection(TokioIo::new(stream), service));
                        connections.spawn(async move {
                            // A connection error (a client that went away)
                            // concerns that connection only
                            let _ = connection.await;
                        });
                    }
                    Err(e) => {
                        // Out of file descriptors, a connection reset before
                        // it was accepted, ...: keep serving the others
                        log::warn!("accepting a connection failed: {}", e);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                },
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
            }
        }
        drop(listener);
        let complete = tokio::select! {
            () = graceful.shutdown() => true,
            () = drain() => false,
        };
        while connections.try_join_next().is_some() {}
        let cancelled = connections.len();
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        Ok(Drain { complete, cancelled })
    }

    /// The database, once no call holds it any more: after
    /// [`serve`](Self::serve) returned, the calls of closed connections let
    /// go of it within moments. Waits at most `wait`; returns the shared
    /// database if something else still holds it then.
    pub async fn into_database(self, wait: Duration) -> Result<D, Arc<D>> {
        let deadline = Instant::now() + wait;
        let mut db = self.db;
        loop {
            match Arc::try_unwrap(db) {
                Ok(db) => return Ok(db),
                Err(shared) if Instant::now() >= deadline => return Err(shared),
                Err(shared) => {
                    db = shared;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
    }
}
