//! Helpers for the server's tests: a server on an ephemeral port with a
//! store in a temporary directory, and a `Remote` connected to it.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use iwdb::{CheckpointOptions, Embedded, FsyncPolicy, LogFs, QueryConfig, Store, StoreOptions, WalOptions};
use iwdb_query::Database;
use iwdb_server::client::Remote;
#[cfg(feature = "rest")]
use iwdb_server::client::RestRemote;
use iwdb_server::{Drain, Server};
use tokio::runtime::Runtime;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// `always`, small segments, no background threads, a checkpoint on close.
pub fn options() -> StoreOptions {
    StoreOptions {
        wal: WalOptions { fsync: FsyncPolicy::Always, segment_size: iwdb_storage::MIN_SEGMENT_SIZE },
        checkpoint: CheckpointOptions { wal_size: None, interval: None, on_close: true, keep: 2, background: false },
        create_if_missing: true,
        archive: None,
        retention: Default::default(),
    }
}

/// A server of `D` on its own runtime, listening on 127.0.0.1 with an
/// ephemeral port.
pub struct Running<D: Database + 'static> {
    pub addr: SocketAddr,
    runtime: Option<Runtime>,
    stop: Option<oneshot::Sender<Duration>>,
    task: Option<JoinHandle<(Drain, Server<D>)>>,
}

impl<D: Database + 'static> Running<D> {
    pub fn start(db: D) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("test-server")
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Server::new(Arc::new(db));
        let (stop, stopped) = oneshot::channel::<Duration>();
        let task = runtime.spawn(async move {
            let (tx, rx) = oneshot::channel::<Duration>();
            let stop = async move {
                let drain = stopped.await.unwrap_or(Duration::ZERO);
                let _ = tx.send(drain);
            };
            let drain = move || async move {
                let drain = rx.await.unwrap_or(Duration::ZERO);
                tokio::time::sleep(drain).await;
            };
            let report = server.serve(listener, stop, drain).await.unwrap();
            (report, server)
        });
        Running { addr, runtime: Some(runtime), stop: Some(stop), task: Some(task) }
    }

    pub fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn client(&self) -> Remote {
        Remote::connect(&self.endpoint()).unwrap()
    }

    #[cfg(feature = "rest")]
    pub fn rest_client(&self) -> RestRemote {
        RestRemote::connect(&self.endpoint()).unwrap()
    }

    /// Shut down with a drain of `drain`; the report and the database.
    pub fn shutdown(mut self, drain: Duration) -> (Drain, D) {
        let runtime = self.runtime.take().unwrap();
        let _ = self.stop.take().unwrap().send(drain);
        let task = self.task.take().unwrap();
        let (report, db) = runtime.block_on(async {
            let (report, server) = task.await.unwrap();
            let db = server.into_database(Duration::from_secs(10)).await;
            (report, db.unwrap_or_else(|_| panic!("a call still holds the database")))
        });
        runtime.shutdown_timeout(Duration::from_secs(5));
        (report, db)
    }

    /// Run `f` on the server's runtime.
    pub fn block_on<T>(&self, f: impl Future<Output = T>) -> T {
        self.runtime.as_ref().unwrap().block_on(f)
    }
}

impl<D: Database + 'static> Drop for Running<D> {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(Duration::ZERO);
        }
        if let Some(runtime) = self.runtime.take() {
            if let Some(task) = self.task.take() {
                let _ = runtime.block_on(task);
            }
            runtime.shutdown_timeout(Duration::from_secs(5));
        }
    }
}

/// A store in `dir`, served as a `Database` on its own worker pool.
pub fn embedded<F: LogFs + Clone + Send + Sync + 'static>(
    fs: F,
    dir: &std::path::Path,
    options: StoreOptions,
) -> Embedded<F>
where
    F::File: Send,
{
    let store = Store::open_with(fs, dir, options).unwrap();
    Embedded::new(store, QueryConfig::default()).unwrap()
}

/// A fresh store behind a server, and a client of it (`C`: gRPC or REST):
/// the conformance fixture.
pub struct Fresh<C = Remote> {
    remote: C,
    pub server: Running<Embedded>,
    _dir: tempfile::TempDir,
}

impl<C> Deref for Fresh<C> {
    type Target = C;

    fn deref(&self) -> &C {
        &self.remote
    }
}

fn served() -> (Running<Embedded>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options()).unwrap();
    (Running::start(Embedded::new(store, QueryConfig::default()).unwrap()), dir)
}

/// Over gRPC.
pub fn fresh() -> Fresh {
    let (server, dir) = served();
    Fresh { remote: server.client(), server, _dir: dir }
}

/// Over REST; streamed answers as NDJSON or as one JSON message.
#[cfg(feature = "rest")]
pub fn fresh_rest(ndjson: bool) -> Fresh<RestRemote> {
    let (server, dir) = served();
    Fresh { remote: server.rest_client().ndjson(ndjson), server, _dir: dir }
}
