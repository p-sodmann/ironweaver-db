//! Helpers for the server's tests: a server on an ephemeral port with a
//! store in a temporary directory, and a `Remote` connected to it. The
//! conformance fixtures run with authentication on (step 15a): an admin
//! is created before the server starts, and the client logs in.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use iwdb::auth::{AuthSettings, HashParams};
use iwdb::{CheckpointOptions, Embedded, FsyncPolicy, LogFs, QueryConfig, Secret, Store, StoreOptions, WalOptions};
use iwdb_server::auth::AuthMode;
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

/// Cheap password hashes for tests.
pub const FAST: HashParams = HashParams { memory_kib: 64, iterations: 1, parallelism: 1 };
/// The conformance fixtures' admin.
pub const ADMIN: (&str, &str) = ("admin", "admin-password");

/// Authentication settings for tests: cheap hashes.
pub fn auth_settings() -> AuthSettings {
    AuthSettings { hash: FAST, ..AuthSettings::default() }
}

/// A server of `D` on its own runtime, listening on 127.0.0.1 with an
/// ephemeral port.
pub struct Running<D: iwdb_server::auth::Served> {
    pub addr: SocketAddr,
    runtime: Option<Runtime>,
    stop: Option<oneshot::Sender<Duration>>,
    task: Option<JoinHandle<(Drain, Server<D>)>>,
}

impl<D: iwdb_server::auth::Served> Running<D> {
    pub fn start(db: D) -> Self {
        Self::start_with(db, AuthMode::default())
    }

    /// With authentication on.
    pub fn start_auth(db: D) -> Self {
        Self::start_with(db, AuthMode { enabled: true })
    }

    pub fn start_with(db: D, auth: AuthMode) -> Self {
        Self::start_built(db, |server| server.auth(auth))
    }

    /// With the server as `build` makes it (console, auth, limits).
    pub fn start_built(db: D, build: impl FnOnce(Server<D>) -> Server<D>) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("test-server")
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = build(Server::new(Arc::new(db)));
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

impl<D: iwdb_server::auth::Served> Drop for Running<D> {
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

/// A store served without authentication, and a gRPC client: for the
/// tests of what the adapters add (raw requests carry no token).
pub fn fresh_open() -> Fresh {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options()).unwrap();
    let server = Running::start(Embedded::new(store, QueryConfig::default()).unwrap());
    Fresh { remote: server.client(), server, _dir: dir }
}

/// The same, with a REST client.
#[cfg(feature = "rest")]
pub fn fresh_open_rest() -> Fresh<RestRemote> {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options()).unwrap();
    let server = Running::start(Embedded::new(store, QueryConfig::default()).unwrap());
    Fresh { remote: server.rest_client(), server, _dir: dir }
}

/// A store with the admin [`ADMIN`], served with authentication on.
pub fn served() -> (Running<Embedded>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options()).unwrap();
    store.users().with_params(FAST).create(ADMIN.0, &Secret::new(ADMIN.1), true).unwrap();
    let db = Embedded::new(store, QueryConfig::default()).unwrap().with_auth(auth_settings());
    (Running::start_auth(db), dir)
}

/// Over gRPC, logged in as the admin.
pub fn fresh() -> Fresh {
    let (server, dir) = served();
    let remote = server.client();
    iwdb_query::exec::block_on(remote.login(ADMIN.0, Secret::new(ADMIN.1))).unwrap();
    Fresh { remote, server, _dir: dir }
}

/// Over REST, logged in as the admin; streamed answers as NDJSON or as one
/// JSON message.
#[cfg(feature = "rest")]
pub fn fresh_rest(ndjson: bool) -> Fresh<RestRemote> {
    let (server, dir) = served();
    let remote = server.rest_client().ndjson(ndjson);
    iwdb_query::exec::block_on(remote.login(ADMIN.0, Secret::new(ADMIN.1))).unwrap();
    Fresh { remote, server, _dir: dir }
}
