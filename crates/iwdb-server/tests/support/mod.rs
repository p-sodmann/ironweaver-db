//! Helpers for the server's tests: a server on an ephemeral port with a
//! store in a temporary directory, and a `Remote` connected to it. The
//! conformance fixtures run with authentication on (step 15a): an admin
//! is created before the server starts, and the client logs in; and over
//! TLS (step 15b), with the test certificates of `tests/fixtures/tls`.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::net::SocketAddr;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iwdb::auth::{AuthSettings, HashParams};
use iwdb::{CheckpointOptions, Embedded, FsyncPolicy, LogFs, QueryConfig, Secret, Store, StoreOptions, WalOptions};
use iwdb_server::auth::AuthMode;
#[cfg(feature = "rest")]
use iwdb_server::client::RestRemote;
use iwdb_server::client::{ClientTls, Remote};
use iwdb_server::config::ClientAuth;
use iwdb_server::tls::{ServerTls, TlsFiles};
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
        memory: Default::default(),
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

/// A test certificate or key (`tests/fixtures/tls`; test-only, public).
pub fn tls_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/tls").join(name)
}

/// The server's TLS: `server.pem`, and with `client_auth` client
/// certificates of the test CA.
pub fn server_tls(client_auth: Option<ClientAuth>) -> Arc<ServerTls> {
    let files = TlsFiles {
        cert: tls_fixture("server.pem"),
        key: tls_fixture("server.key"),
        client_ca: client_auth.map(|_| tls_fixture("ca.pem")),
        client_auth: client_auth.unwrap_or(ClientAuth::Optional),
    };
    Arc::new(ServerTls::load(files).unwrap())
}

/// A client that trusts the test CA, presenting `client-<name>.pem` if
/// `cert` is given.
pub fn client_tls(cert: Option<&str>) -> ClientTls {
    ClientTls {
        ca: Some(tls_fixture("ca.pem")),
        cert: cert.map(|c| tls_fixture(&format!("client-{}.pem", c))),
        key: cert.map(|c| tls_fixture(&format!("client-{}.key", c))),
    }
}

/// An audit sink that keeps the entries it is given (step 15c), unless
/// paused (while a test sets a case up).
#[derive(Default)]
pub struct Captured {
    entries: std::sync::Mutex<Vec<iwdb_query::audit::AuditEntry>>,
    paused: std::sync::atomic::AtomicBool,
}

impl Captured {
    pub fn new() -> Arc<Self> {
        Arc::new(Captured::default())
    }

    /// The entries recorded since the last call.
    pub fn take(&self) -> Vec<iwdb_query::audit::AuditEntry> {
        std::mem::take(&mut *self.entries.lock().unwrap())
    }

    /// Run `f` without recording.
    pub fn paused<T>(&self, f: impl FnOnce() -> T) -> T {
        use std::sync::atomic::Ordering;
        let was = self.paused.swap(true, Ordering::SeqCst);
        let result = f();
        self.paused.store(was, Ordering::SeqCst);
        result
    }
}

impl iwdb_query::audit::AuditSink for Captured {
    fn record(&self, entry: &iwdb_query::audit::AuditEntry) {
        if !self.paused.load(std::sync::atomic::Ordering::SeqCst) {
            self.entries.lock().unwrap().push(entry.clone());
        }
    }
}

/// A server of `D` on its own runtime, listening on 127.0.0.1 with an
/// ephemeral port.
pub struct Running<D: iwdb_server::auth::Served> {
    pub addr: SocketAddr,
    /// Over TLS: what its clients trust (the test CA).
    pub tls: Option<ClientTls>,
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
        let tls = server.serves_tls().then(|| client_tls(None));
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
        Running { addr, tls, runtime: Some(runtime), stop: Some(stop), task: Some(task) }
    }

    /// Over TLS with the test certificate, and authentication as `auth`.
    pub fn start_tls(db: D, auth: AuthMode, client_auth: Option<ClientAuth>) -> Self {
        Self::start_built(db, |server| server.auth(auth).tls(Some(server_tls(client_auth))))
    }

    /// `https://127.0.0.1:port` over TLS, `http://...` otherwise.
    pub fn endpoint(&self) -> String {
        format!("{}://{}", if self.tls.is_some() { "https" } else { "http" }, self.addr)
    }

    pub fn client(&self) -> Remote {
        Remote::connect_tls(&self.endpoint(), &self.tls.clone().unwrap_or_default()).unwrap()
    }

    #[cfg(feature = "rest")]
    pub fn rest_client(&self) -> RestRemote {
        RestRemote::connect_tls(&self.endpoint(), &self.tls.clone().unwrap_or_default()).unwrap()
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

/// A store with the admin [`ADMIN`], served with authentication on, over
/// TLS.
pub fn served() -> (Running<Embedded>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options()).unwrap();
    store.users().with_params(FAST).create(ADMIN.0, &Secret::new(ADMIN.1), true).unwrap();
    let db = Embedded::new(store, QueryConfig::default()).unwrap().with_auth(auth_settings());
    (Running::start_tls(db, AuthMode { enabled: true }, None), dir)
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
