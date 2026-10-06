//! `iwdb-server [--config <file>]`: serve the data directory the
//! configuration names (file and `IWDB_*` variables, ADR 0039) over gRPC
//! and REST (one port), over TLS unless the configuration turns it off
//! (ADR 0048), until SIGINT or SIGTERM, then shut down gracefully (ADR
//! 0027). A second signal ends the drain early. SIGHUP reloads the TLS
//! certificate, key and client CA.
//!
//! The port opens first and answers health while the store recovers; the
//! server is ready once recovery has finished (ADR 0040). Logs are JSON
//! lines on stderr unless it is a terminal (ADR 0042).
//!
//! Exit codes: 0 after a clean shutdown (or a ready `--probe`, a valid
//! `--check-config`), 1 if serving or closing the store failed (or the
//! probe found the server not ready), 2 for a bad command line or
//! configuration.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iwdb::projection::ProjectionHandle;
use iwdb::{Embedded, Store};
use iwdb_query::log::LogRing;
use iwdb_server::auth::AuthMode;
use iwdb_server::config::Config;
use iwdb_server::health::{Health, Phase};
use iwdb_server::tls::ServerTls;
use iwdb_server::{LaunchOptions, launch, logging};
use tokio::net::TcpListener;
use tokio::sync::watch;

const USAGE: &str = "usage: iwdb-server [--config <file>]
       iwdb-server --check-config [--config <file>]
       iwdb-server --probe [--config <file> | https://<host:port> | http://<host:port>]

Serves an Ironweaver DB data directory over gRPC (proto/ironweaver_db/v1) and
REST/JSON (/v1/..., OpenAPI at /v1/openapi.json) on one port; --version lists
what this build has (a build without the rest feature serves gRPC only).

The configuration is a TOML file, IWDB_* environment variables, or both (a
variable wins); only data_dir is required, so IWDB_DATA_DIR alone is enough.

TLS is on by default: [tls] cert and key (IWDB_TLS_CERT, IWDB_TLS_KEY) are PEM
files; plaintext needs [tls] enabled = false, and on a non-loopback address
also [server] plaintext_public = true. With [tls] client_ca, a client
certificate's common name authenticates as that user (mTLS). SIGHUP reloads
the certificate, key and client CA.

Authentication is on by default: every call but login and health needs a
token. A store without users refuses to start; create the first admin with
`iwctl user create <dir> <name> --admin`, or start once with
IWDB_AUTH_BOOTSTRAP_PASSWORD (and IWDB_AUTH_BOOTSTRAP_USER, default admin).
Settings: documentation/api/config.md. --check-config validates it and prints
the effective settings with where each came from.

Health: GET /v1/health/live and /v1/health/ready, or grpc.health.v1.Health;
ready once recovery has finished. --probe asks a server's readiness and exits
0 if it is ready: the server of the configuration (file and IWDB_* variables;
a wildcard listen address as loopback, TLS as configured), or the URL given.
It doesn't verify the server's certificate and sends no credentials.

SIGINT or SIGTERM shuts down gracefully; a second one cancels the calls
still running.";

/// What this build serves and reads (ADR 0034), for `--version`.
const FEATURES: &[&str] = &[
    "grpc",
    #[cfg(feature = "rest")]
    "rest",
    #[cfg(feature = "postgres")]
    "postgres",
    #[cfg(feature = "console")]
    "console",
];

enum Command {
    Serve(Option<PathBuf>),
    Check(Option<PathBuf>),
    /// The configuration's server, or a URL's.
    Probe(Probe),
}

enum Probe {
    Config(Option<PathBuf>),
    Address { address: String, tls: bool },
}

fn command() -> Result<Command, ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let file = |rest: &[&str]| match rest {
        [] => Ok(None),
        ["--config", path] => Ok(Some(PathBuf::from(path))),
        _ => Err(ExitCode::from(2)),
    };
    let command = match args.as_slice() {
        ["--help" | "-h"] => {
            println!("{}", USAGE);
            return Err(ExitCode::SUCCESS);
        }
        ["--version" | "-V"] => {
            println!("iwdb-server {} ({})", env!("CARGO_PKG_VERSION"), FEATURES.join(", "));
            return Err(ExitCode::SUCCESS);
        }
        ["--probe", url] if url.contains("://") => match url.split_once("://") {
            Some(("https", address)) => Ok(Command::Probe(Probe::Address { address: address.into(), tls: true })),
            Some(("http", address)) => Ok(Command::Probe(Probe::Address { address: address.into(), tls: false })),
            _ => Err(ExitCode::from(2)),
        },
        ["--probe", rest @ ..] => file(rest).map(|path| Command::Probe(Probe::Config(path))),
        ["--check-config", rest @ ..] => file(rest).map(Command::Check),
        rest => file(rest).map(Command::Serve),
    };
    command.inspect_err(|_| eprintln!("{}", USAGE))
}

fn main() -> ExitCode {
    let (path, check) = match command() {
        Ok(Command::Serve(path)) => (path, false),
        Ok(Command::Check(path)) => (path, true),
        Ok(Command::Probe(probe)) => {
            let (address, tls) = match probe {
                Probe::Address { address, tls } => (address, tls),
                Probe::Config(path) => match Config::from_sources(path.as_deref(), std::env::vars()) {
                    Ok(config) => config.probe_target(),
                    Err(e) => {
                        eprintln!("iwdb-server: {}", e);
                        return ExitCode::from(2);
                    }
                },
            };
            return match iwdb_server::health::probe(&address, tls, Duration::from_secs(3)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("iwdb-server: not ready: {}", e);
                    ExitCode::FAILURE
                }
            };
        }
        Err(code) => return code,
    };
    let config = match Config::from_sources(path.as_deref(), std::env::vars()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("iwdb-server: {}", e);
            return ExitCode::from(2);
        }
    };
    if check {
        print!("{}", config.describe());
        return ExitCode::SUCCESS;
    }
    let audit = match &config.audit.dir {
        Some(dir) => match iwdb_server::audit::AuditFiles::open(dir, config.audit.retention_days) {
            Ok(files) => Some(files),
            Err(e) => {
                eprintln!("iwdb-server: {}", e);
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    // The log tail (step 16c): the events the logger writes, for GetLog
    let ring = Arc::new(LogRing::new(config.log.tail_events));
    if let Err(e) = logging::init(config.log.format, &config.log.level, audit, ring.clone()) {
        eprintln!("iwdb-server: {}", e);
        return ExitCode::from(2);
    }
    for warning in config.warnings() {
        tracing::warn!("{}", warning);
    }
    // The certificate and key are read before the port opens: a server
    // that can't speak TLS doesn't start
    let tls = match config.tls_files().map(ServerTls::load).transpose() {
        Ok(tls) => tls.map(Arc::new),
        Err(e) => {
            tracing::error!(error = %e, "can't set up TLS");
            return ExitCode::from(2);
        }
    };
    match run(&config, tls, ring) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            tracing::error!(error = %message, "iwdb-server failed");
            ExitCode::FAILURE
        }
    }
}

fn run(config: &Config, tls: Option<Arc<ServerTls>>, ring: Arc<LogRing>) -> Result<(), String> {
    let dir = config.data_dir.display().to_string();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("iwdb-server")
        .build()
        .map_err(|e| format!("starting the runtime: {}", e))?;
    let drain = config.drain_timeout();
    // Projections (ADR 0032) run until the store closes, at the end
    let projections: Arc<Mutex<Vec<ProjectionHandle>>> = Arc::default();
    let open = {
        let (config, projections) = (config.clone(), projections.clone());
        move || open(&config, &projections, ring)
    };
    let options = LaunchOptions {
        max_message_bytes: config.server.max_message_bytes,
        unready_delay: config.unready_delay(),
        console: config.console.enabled,
        auth: AuthMode { enabled: config.auth.enabled },
        tls: tls.clone(),
        name: dir.clone(),
    };
    let served = runtime.block_on(async {
        let listener =
            TcpListener::bind(config.listen).await.map_err(|e| format!("listening on {}: {}", config.listen, e))?;
        let signals = signals()?;
        if let Some(tls) = tls {
            reload_on_hangup(tls)?;
        }
        let (mut first, mut second) = (signals.clone(), signals);
        let stop = async move {
            let _ = first.wait_for(|n| *n >= 1).await;
            tracing::info!(drain_ms = drain.as_millis() as u64, "shutting down; running calls may finish");
        };
        let give_up = move || async move {
            tokio::select! {
                () = tokio::time::sleep(drain) => {}
                _ = second.wait_for(|n| *n >= 2) => {}
            }
        };
        let launched = launch(listener, Health::new(Phase::Recovering), options, open, stop, give_up).await?;
        if !launched.drain.complete {
            tracing::warn!(
                connections = launched.drain.cancelled,
                "cancelled the calls of connections still running at the end of the drain"
            );
        }
        launched
            .server
            .into_database(Duration::from_secs(10))
            .await
            .map_err(|_| "a call still holds the database".to_owned())
    });
    drop(runtime);
    // Stop the projections before the store closes, so that their last
    // commits are in its final checkpoint
    for p in projections.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default() {
        p.stop();
    }
    let db = served?;
    // Finishes the queued requests, flushes every WAL, checkpoints (if
    // configured) and releases the data directory
    db.close().map_err(|e| format!("closing {}: {}", dir, e))?;
    tracing::info!(data_dir = %dir, "closed {}", dir);
    Ok(())
}

/// Open the store (recovery), start the projections, and serve it.
fn open(config: &Config, projections: &Mutex<Vec<ProjectionHandle>>, ring: Arc<LogRing>) -> Result<Embedded, String> {
    let dir = config.data_dir.display();
    let started = Instant::now();
    let store = Store::open(&config.data_dir, config.store_options()).map_err(|e| format!("opening {}: {}", dir, e))?;
    for info in store.namespaces() {
        if let Ok(ns) = store.namespace(info.name.as_str()) {
            let r = ns.status().recovery;
            tracing::info!(
                namespace = %info.name.as_str(),
                checkpoint = r.checkpoint,
                replayed = r.replayed,
                seq = r.seq,
                torn_tail = r.torn_tail.is_some(),
                "recovered"
            );
        }
    }
    tracing::info!(data_dir = %dir, took_ms = started.elapsed().as_millis() as u64, "recovery finished");
    users(config, &store)?;
    let started = config.start_projections(&store)?;
    for p in &started {
        let status = p.status();
        tracing::info!(name = %status.name, namespace = %status.namespace, mark = ?status.mark, "projection started");
    }
    if let Ok(mut list) = projections.lock() {
        list.extend(started);
    }
    let db = Embedded::new(store, config.query_config())
        .map(|db| db.with_auth(config.auth_settings()).with_log(ring))
        .map_err(|e| e.to_string())?;
    match &config.backup.dir {
        Some(dir) => db.with_backup_dir(dir).map_err(|e| format!("[backup] dir: {}", e.message())),
        None => Ok(db),
    }
}

/// The first admin from the bootstrap variables, on a store without users;
/// and with authentication on, a store must have users (ADR 0047).
fn users(config: &Config, store: &Store) -> Result<(), String> {
    let users = store.users();
    let exist = users.exist().map_err(|e| format!("reading the users: {}", e))?;
    match (&config.bootstrap, exist) {
        (Some((name, password)), false) => {
            users.create(name, password, true).map_err(|e| format!("creating the first admin: {}", e))?;
            tracing::info!(user = %name, "created the first admin from {}", iwdb_server::config::BOOTSTRAP_PASSWORD_VAR);
        }
        (Some(_), true) => tracing::warn!(
            "{} is set, but the store has users already: ignored (unset it)",
            iwdb_server::config::BOOTSTRAP_PASSWORD_VAR
        ),
        (None, false) if config.auth.enabled => {
            return Err(format!(
                "authentication is on ([auth] enabled) and {} has no users, so no one could log in. Create the \
                 first admin with `iwctl user create {} <name> --admin` while the server is stopped, or start once \
                 with {} set (and {}, default admin); or turn authentication off with [auth] enabled = false \
                 (IWDB_AUTH_ENABLED=false)",
                config.data_dir.display(),
                config.data_dir.display(),
                iwdb_server::config::BOOTSTRAP_PASSWORD_VAR,
                iwdb_server::config::BOOTSTRAP_USER_VAR,
            ));
        }
        _ => {}
    }
    if !config.auth.enabled {
        tracing::warn!("authentication is off: every caller may read and change everything");
    }
    Ok(())
}

/// Reload the TLS files on SIGHUP (ADR 0048); a reload that fails keeps
/// the certificate in use.
#[cfg(unix)]
fn reload_on_hangup(tls: Arc<ServerTls>) -> Result<(), String> {
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .map_err(|e| format!("listening for SIGHUP: {}", e))?;
    tokio::spawn(async move {
        while hangup.recv().await.is_some() {
            match tls.reload() {
                Ok(()) => tracing::info!(cert = %tls.files().cert.display(), "reloaded the TLS certificate (SIGHUP)"),
                Err(e) => {
                    tracing::error!(error = %e, "reloading TLS failed (SIGHUP): still serving the certificate in use")
                }
            }
        }
    });
    Ok(())
}

#[cfg(not(unix))]
fn reload_on_hangup(_tls: Arc<ServerTls>) -> Result<(), String> {
    Ok(())
}

/// A counter of the shutdown signals received (SIGINT, and SIGTERM on Unix).
fn signals() -> Result<watch::Receiver<u32>, String> {
    let (count, received) = watch::channel(0u32);
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| format!("listening for SIGTERM: {}", e))?;
    tokio::spawn(async move {
        loop {
            #[cfg(unix)]
            let got = tokio::select! {
                r = tokio::signal::ctrl_c() => r.is_ok(),
                r = terminate.recv() => r.is_some(),
            };
            #[cfg(not(unix))]
            let got = tokio::signal::ctrl_c().await.is_ok();
            if !got {
                return;
            }
            count.send_modify(|n| *n += 1);
        }
    });
    Ok(received)
}
