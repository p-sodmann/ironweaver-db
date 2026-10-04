//! Health and readiness (step 16b, ADR 0040), in-process with
//! [`iwdb_server::launch`]: the port answers health while the store opens,
//! database calls fail with `unavailable` until recovery has finished,
//! readiness turns on only then, and turns off first when a shutdown
//! begins.
//!
//! The store's open is held at a gate, so the tests see the recovering
//! server deterministically; the binary's test (`binary.rs`) recovers a
//! large WAL instead.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use iwdb::{Embedded, Mutation, QueryConfig, Store};
use iwdb_query::exec::block_on;
use iwdb_query::{Code, Database, QueryOptions};
use iwdb_server::client::Remote;
use iwdb_server::health::{Health, Phase};
use iwdb_server::{LaunchOptions, Launched, launch};
use tokio::sync::oneshot;
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;

const NODES: usize = 300;

/// A store whose next open replays `NODES` commits (no checkpoint on
/// close).
fn store_to_recover(dir: &Path) {
    let mut options = support::options();
    options.checkpoint.on_close = false;
    let store = Store::open(dir, options).unwrap();
    for i in 0..NODES {
        let node = Mutation::UpsertNode {
            id: format!("n{}", i),
            labels: vec!["N".into()],
            attr: Default::default(),
            meta: Default::default(),
            expected_version: None,
        };
        store.commit(&[node]).unwrap();
    }
    store.close().unwrap();
}

/// `GET path` over HTTP/1.1: the status and the body.
fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(stream, "GET {} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n", path).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let status = answer.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = answer.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
    (status, body)
}

/// The gRPC health of service `service`.
async fn grpc_health(addr: SocketAddr, service: &str) -> ServingStatus {
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{}", addr)).unwrap().connect().await.unwrap();
    let mut client = HealthClient::new(channel);
    let answer = client.check(HealthCheckRequest { service: service.into() }).await.unwrap().into_inner();
    ServingStatus::try_from(answer.status).unwrap()
}

/// A server launched on its own runtime, with its open held until `release`.
struct Launch {
    addr: SocketAddr,
    runtime: tokio::runtime::Runtime,
    health: Health,
    release: Option<mpsc::Sender<()>>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<Launched<Embedded>, String>>,
}

impl Launch {
    fn start(dir: &Path, unready_delay: Duration, fail: bool) -> Launch {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let addr = listener.local_addr().unwrap();
        let health = Health::new(Phase::Recovering);
        let (release, gate) = mpsc::channel::<()>();
        let (stop, stopped) = oneshot::channel::<()>();
        let dir = dir.to_owned();
        let open = move || {
            let _ = gate.recv();
            if fail {
                return Err("the store can't be opened (test)".to_owned());
            }
            let store = Store::open(&dir, support::options()).map_err(|e| e.to_string())?;
            Embedded::new(store, QueryConfig::default()).map_err(|e| e.to_string())
        };
        let options = LaunchOptions { unready_delay, name: "test".into(), ..LaunchOptions::default() };
        let task = runtime.spawn(launch(
            listener,
            health.clone(),
            options,
            open,
            async move {
                let _ = stopped.await;
            },
            || tokio::time::sleep(Duration::from_secs(5)),
        ));
        Launch { addr, runtime, health, release: Some(release), stop: Some(stop), task }
    }

    fn release(&mut self) {
        let _ = self.release.take().unwrap().send(());
    }

    fn wait_for(&self, phase: Phase) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.health.phase() != phase {
            assert!(Instant::now() < deadline, "never {:?}", phase);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn stop(&mut self) -> Result<Launched<Embedded>, String> {
        let _ = self.stop.take().unwrap().send(());
        let task = std::mem::replace(&mut self.task, self.runtime.spawn(async { Err("taken".to_owned()) }));
        self.runtime.block_on(task).unwrap()
    }
}

#[test]
fn not_ready_until_recovery_has_finished() {
    let dir = tempfile::tempdir().unwrap();
    store_to_recover(dir.path());
    let mut server = Launch::start(dir.path(), Duration::ZERO, false);
    let addr = server.addr;
    let remote = Remote::connect(&format!("http://{}", addr)).unwrap();

    // Held in recovery: live, not ready, and database calls are refused
    assert_eq!(server.health.phase(), Phase::Recovering);
    assert_eq!(get(addr, "/v1/health/live"), (200, r#"{"state":"HEALTH_STATE_RECOVERING"}"#.into()));
    assert_eq!(get(addr, "/v1/health/ready"), (503, r#"{"state":"HEALTH_STATE_RECOVERING"}"#.into()));
    for service in ["", "ironweaver_db.v1.DatabaseService"] {
        assert_eq!(server.runtime.block_on(grpc_health(addr, service)), ServingStatus::NotServing);
    }
    let e = block_on(remote.get_nodes("default", vec!["n0".into()], QueryOptions::default())).unwrap_err();
    assert_eq!(e.code(), Code::Unavailable, "{}", e);
    assert!(e.message().contains("recovering"), "{}", e);
    #[cfg(feature = "rest")]
    {
        let (status, body) = get(addr, "/v1/namespaces");
        assert_eq!(status, 503);
        assert!(body.contains(r#""code":"unavailable""#), "{}", body);
    }
    assert!(iwdb_server::health::probe(&addr.to_string(), Duration::from_secs(2)).is_err());

    // Recovery finishes: ready, and every committed record is there at the
    // first ready answer
    server.release();
    let deadline = Instant::now() + Duration::from_secs(30);
    while get(addr, "/v1/health/ready").0 != 200 {
        assert!(Instant::now() < deadline, "never ready");
        std::thread::sleep(Duration::from_millis(1));
    }
    let status = block_on(remote.namespace_status("default")).unwrap();
    assert_eq!((status.nodes, status.seq), (NODES, NODES as u64));
    assert_eq!(get(addr, "/v1/health/ready"), (200, r#"{"state":"HEALTH_STATE_READY","ready":true}"#.into()));
    assert_eq!(server.runtime.block_on(grpc_health(addr, "")), ServingStatus::Serving);
    iwdb_server::health::probe(&addr.to_string(), Duration::from_secs(2)).unwrap();
    // An unknown service is the protocol's NOT_FOUND
    let unknown = server.runtime.block_on(async {
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{}", addr)).unwrap().connect().await;
        HealthClient::new(channel.unwrap()).check(HealthCheckRequest { service: "other".into() }).await
    });
    assert_eq!(unknown.unwrap_err().code(), tonic::Code::NotFound);

    let launched = server.stop().unwrap();
    assert!(launched.drain.complete);
    let db = server.runtime.block_on(launched.server.into_database(Duration::from_secs(5))).ok().unwrap();
    db.close().unwrap();
}

/// A shutdown turns readiness off first: during the unready delay the
/// server still answers database calls, a gRPC watcher sees NOT_SERVING,
/// and only then does it drain.
#[test]
fn a_shutdown_turns_readiness_off_first() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Launch::start(dir.path(), Duration::from_millis(400), false);
    let addr = server.addr;
    server.release();
    server.wait_for(Phase::Ready);
    let mut watch = server.runtime.block_on(async {
        let channel = tonic::transport::Endpoint::from_shared(format!("http://{}", addr)).unwrap().connect().await;
        let mut client = HealthClient::new(channel.unwrap());
        client.watch(HealthCheckRequest { service: String::new() }).await.unwrap().into_inner()
    });
    let first = server.runtime.block_on(watch.message()).unwrap().unwrap();
    assert_eq!(first.status, ServingStatus::Serving as i32);

    let _ = server.stop.take().unwrap().send(());
    server.wait_for(Phase::Draining);
    let next = server.runtime.block_on(watch.message()).unwrap().unwrap();
    assert_eq!(next.status, ServingStatus::NotServing as i32);
    // The watch ends, so it doesn't hold the drain
    assert!(server.runtime.block_on(watch.message()).unwrap().is_none());
    assert_eq!(get(addr, "/v1/health/ready").0, 503);
    assert_eq!(get(addr, "/v1/health/live").0, 200);
    // Still serving while unready
    let remote = Remote::connect(&format!("http://{}", addr)).unwrap();
    block_on(remote.namespace_status("default")).unwrap();

    let task = std::mem::replace(&mut server.task, server.runtime.spawn(async { Err(String::new()) }));
    let launched = server.runtime.block_on(task).unwrap().unwrap();
    assert!(launched.drain.complete, "{:?}", launched.drain);
    let db = server.runtime.block_on(launched.server.into_database(Duration::from_secs(5))).ok().unwrap();
    db.close().unwrap();
}

#[test]
fn a_failed_open_never_becomes_ready() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Launch::start(dir.path(), Duration::ZERO, true);
    assert_eq!(get(server.addr, "/v1/health/ready").0, 503);
    server.release();
    let task = std::mem::replace(&mut server.task, server.runtime.spawn(async { Err(String::new()) }));
    let e = server.runtime.block_on(task).unwrap().err().unwrap();
    assert!(e.contains("can't be opened"), "{}", e);
    assert_ne!(server.health.phase(), Phase::Ready);
    // The port is closed
    assert!(TcpStream::connect(server.addr).is_err());
}

/// A stop during recovery waits for the store to open, never becomes
/// ready, and hands the database back to be closed cleanly.
#[test]
fn a_stop_during_recovery_closes_the_store() {
    let dir = tempfile::tempdir().unwrap();
    store_to_recover(dir.path());
    let mut server = Launch::start(dir.path(), Duration::ZERO, false);
    let _ = server.stop.take().unwrap().send(());
    server.wait_for(Phase::Draining);
    server.release();
    let task = std::mem::replace(&mut server.task, server.runtime.spawn(async { Err(String::new()) }));
    let launched = server.runtime.block_on(task).unwrap().unwrap();
    let db = server.runtime.block_on(launched.server.into_database(Duration::from_secs(5))).ok().unwrap();
    db.close().unwrap();
    let store = Store::open(dir.path(), support::options()).unwrap();
    assert_eq!(store.default_namespace().status().nodes, NODES);
    store.close().unwrap();
}

/// The console's pages (feature `console`, ADR 0041): served when turned
/// on, also during recovery; not found when off.
#[cfg(feature = "console")]
#[test]
fn serves_the_console_when_turned_on() {
    use iwdb_server::Server;
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    for on in [true, false] {
        let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
        let addr = listener.local_addr().unwrap();
        let store = Store::open(&dir.path().join(on.to_string()), support::options()).unwrap();
        let server =
            Server::new(std::sync::Arc::new(Embedded::new(store, QueryConfig::default()).unwrap())).console(on);
        let (stop, stopped) = oneshot::channel::<()>();
        let task = runtime.spawn(async move {
            server
                .serve(
                    listener,
                    async move {
                        let _ = stopped.await;
                    },
                    || tokio::time::sleep(Duration::from_secs(5)),
                )
                .await
                .unwrap();
            server
        });
        let (status, body) = get(addr, "/console/index.html");
        if on {
            assert_eq!(status, 200);
            assert!(body.contains("<script src=\"src/rest.js\"></script>"), "{}", &body[..200.min(body.len())]);
            assert_eq!(get(addr, "/console/src/rest.js").0, 200);
            assert_eq!(get(addr, "/console/serve.py").0, 404);
            assert!(get(addr, "/console/console-config.json").1.contains("this server"));
        } else {
            assert_eq!(status, 404);
        }
        let _ = stop.send(());
        let server = runtime.block_on(task).unwrap();
        runtime.block_on(server.into_database(Duration::from_secs(5))).ok().unwrap().close().unwrap();
    }
}
