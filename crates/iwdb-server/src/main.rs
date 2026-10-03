//! `iwdb-server --config <file>`: open the data directory the config file
//! names and serve it over gRPC until SIGINT or SIGTERM, then shut down
//! gracefully (ADR 0027). A second signal ends the drain early.
//!
//! Exit codes: 0 after a clean shutdown, 1 if serving or closing the store
//! failed, 2 for a bad command line or config file.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use iwdb::{Embedded, Store};
use iwdb_server::Server;
use iwdb_server::config::Config;
use tokio::net::TcpListener;
use tokio::sync::watch;

const USAGE: &str = "usage: iwdb-server --config <file>

Serves an Ironweaver DB data directory over gRPC (proto/ironweaver_db/v1).
The config file is TOML; only data_dir is required (see the iwdb_server::config
docs or documentation/api/grpc.md). SIGINT or SIGTERM shuts down gracefully;
a second one cancels the calls still running.";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let path = match (args.next().as_deref(), args.next(), args.next()) {
        (Some("--config"), Some(path), None) => PathBuf::from(path),
        (Some("--help" | "-h"), None, None) => {
            println!("{}", USAGE);
            return ExitCode::SUCCESS;
        }
        (Some("--version" | "-V"), None, None) => {
            println!("iwdb-server {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("{}", USAGE);
            return ExitCode::from(2);
        }
    };
    let config = match Config::load(&path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("iwdb-server: {}", e);
            return ExitCode::from(2);
        }
    };
    match run(&config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("iwdb-server: {}", message);
            ExitCode::FAILURE
        }
    }
}

fn run(config: &Config) -> Result<(), String> {
    let dir = config.data_dir.display();
    let store = Store::open(&config.data_dir, config.store_options()).map_err(|e| format!("opening {}: {}", dir, e))?;
    let db = Embedded::new(store, config.query_config()).map_err(|e| e.to_string())?;
    let server = Server::new(Arc::new(db)).max_message_bytes(config.server.max_message_bytes);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("iwdb-server")
        .build()
        .map_err(|e| format!("starting the runtime: {}", e))?;
    let drain = config.drain_timeout();
    let served = runtime.block_on(async {
        let listener =
            TcpListener::bind(config.listen).await.map_err(|e| format!("listening on {}: {}", config.listen, e))?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        eprintln!("iwdb-server: serving {} on {}", dir, address);
        let signals = signals()?;
        let (mut first, mut second) = (signals.clone(), signals);
        let stop = async move {
            let _ = first.wait_for(|n| *n >= 1).await;
            eprintln!("iwdb-server: shutting down; running calls may finish for {:?}", drain);
        };
        let give_up = move || async move {
            tokio::select! {
                () = tokio::time::sleep(drain) => {}
                _ = second.wait_for(|n| *n >= 2) => {}
            }
        };
        let report = server.serve(listener, stop, give_up).await.map_err(|e| e.to_string())?;
        if !report.complete {
            eprintln!("iwdb-server: cancelled the calls of {} connection(s) at the end of the drain", report.cancelled);
        }
        server.into_database(Duration::from_secs(10)).await.map_err(|_| "a call still holds the database".to_owned())
    });
    drop(runtime);
    let db = served?;
    // Finishes the queued requests, flushes every WAL, checkpoints (if
    // configured) and releases the data directory
    db.close().map_err(|e| format!("closing {}: {}", dir, e))?;
    eprintln!("iwdb-server: closed {}", dir);
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
