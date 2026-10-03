//! The `iwdb-server` config file (TOML). Only `data_dir` is required:
//!
//! ```toml
//! data_dir = "/var/lib/iwdb"        # relative paths are relative to this file
//! listen = "127.0.0.1:7600"           # gRPC and REST
//!
//! [store]
//! fsync = "always"                  # always | group | off
//! group_max_delay_ms = 10           # group commit: fsync at least this often
//! group_max_batch = 64              # ... or after this many commits
//! checkpoint_on_shutdown = true
//!
//! [server]
//! drain_timeout_secs = 30           # how long running calls may finish on shutdown
//! max_message_bytes = 67108864      # largest request or answer message (REST: body)
//! workers = 0                       # threads running requests (0: one per CPU)
//! queue = 1024                      # requests that may wait for a worker
//!
//! [limits.default]                  # what a read gets if it asks for nothing
//! max_results = 1000
//! max_visited = 100000
//! max_edges = 1000000
//! timeout_ms = 30000
//!
//! [limits.max]                      # what no read can exceed
//! max_results = 100000
//! max_visited = 10000000
//! max_edges = 100000000
//! timeout_ms = 300000
//! ```
//!
//! Environment overrides come with step 16. TLS and authentication with
//! step 15: until then the server listens on plain TCP, so bind it to
//! localhost or a private network.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use iwdb::{CheckpointOptions, FsyncPolicy, QueryConfig, StoreOptions};
use iwdb_query::{Bounds, LimitConfig};
use serde::Deserialize;

use crate::DEFAULT_MAX_MESSAGE_BYTES;

/// The default listen address.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7600";

/// A config file that can't be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("can't read the config file {}: {source}", path.display())]
    Read { path: PathBuf, source: std::io::Error },
    #[error("invalid config file {}: {message}", path.display())]
    Invalid { path: PathBuf, message: String },
}

/// The server's configuration (see the module docs for the file).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The store's data directory, created if missing.
    pub data_dir: PathBuf,
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    #[serde(default)]
    pub store: StoreSection,
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub limits: LimitsSection,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StoreSection {
    pub fsync: Fsync,
    pub group_max_delay_ms: u64,
    pub group_max_batch: u32,
    pub checkpoint_on_shutdown: bool,
}

impl Default for StoreSection {
    fn default() -> Self {
        StoreSection { fsync: Fsync::Always, group_max_delay_ms: 10, group_max_batch: 64, checkpoint_on_shutdown: true }
    }
}

/// The WAL's fsync policy (`documentation/guarantees.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fsync {
    Always,
    Group,
    Off,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerSection {
    pub drain_timeout_secs: u64,
    pub max_message_bytes: usize,
    /// 0: the available parallelism (at least 2).
    pub workers: usize,
    pub queue: usize,
}

impl Default for ServerSection {
    fn default() -> Self {
        ServerSection { drain_timeout_secs: 30, max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES, workers: 0, queue: 1024 }
    }
}

/// Read limits: `default` for reads that ask for nothing, `max` that no read
/// can exceed. A missing value is the built-in one (`LimitConfig::default`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsSection {
    pub default: LimitValues,
    pub max: LimitValues,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitValues {
    pub max_results: Option<usize>,
    pub max_visited: Option<usize>,
    pub max_edges: Option<usize>,
    pub timeout_ms: Option<u64>,
}

impl LimitValues {
    fn over(&self, bounds: Bounds, timeout: Duration) -> (Bounds, Duration) {
        let bounds = Bounds {
            max_results: self.max_results.unwrap_or(bounds.max_results),
            max_visited: self.max_visited.unwrap_or(bounds.max_visited),
            max_edges: self.max_edges.unwrap_or(bounds.max_edges),
        };
        (bounds, self.timeout_ms.map_or(timeout, Duration::from_millis))
    }
}

fn default_listen() -> SocketAddr {
    // A constant that parses
    DEFAULT_LISTEN.parse().unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 7600)))
}

impl Config {
    /// Read and check the file at `path`; a relative `data_dir` is resolved
    /// against the file's directory.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read { path: path.into(), source })?;
        let mut config = Config::parse(&text).map_err(|message| ConfigError::Invalid { path: path.into(), message })?;
        if config.data_dir.is_relative()
            && let Some(dir) = path.parent()
        {
            config.data_dir = dir.join(&config.data_dir);
        }
        Ok(config)
    }

    /// Parse and check a config file's text.
    pub fn parse(text: &str) -> Result<Config, String> {
        let config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        config.limit_config().check().map_err(|e| format!("[limits]: {}", e))?;
        if config.store.fsync == Fsync::Group && config.store.group_max_batch == 0 {
            return Err("[store] group_max_batch must be at least 1".into());
        }
        if config.server.max_message_bytes < 1024 {
            return Err("[server] max_message_bytes must be at least 1024".into());
        }
        Ok(config)
    }

    /// The store's options: the defaults, with the fsync policy and
    /// checkpoint on shutdown from the file.
    pub fn store_options(&self) -> StoreOptions {
        let mut options = StoreOptions::default();
        options.wal.fsync = match self.store.fsync {
            Fsync::Always => FsyncPolicy::Always,
            Fsync::Group => FsyncPolicy::Group {
                max_delay: Duration::from_millis(self.store.group_max_delay_ms),
                max_batch: self.store.group_max_batch,
            },
            Fsync::Off => FsyncPolicy::Off,
        };
        options.checkpoint = CheckpointOptions { on_close: self.store.checkpoint_on_shutdown, ..options.checkpoint };
        options
    }

    pub fn limit_config(&self) -> LimitConfig {
        let built_in = LimitConfig::default();
        let (default_limits, default_timeout) =
            self.limits.default.over(built_in.default_limits, built_in.default_timeout);
        let (max_limits, max_timeout) = self.limits.max.over(built_in.max_limits, built_in.max_timeout);
        LimitConfig { default_limits, max_limits, default_timeout, max_timeout }
    }

    /// How the embedded database runs requests.
    pub fn query_config(&self) -> QueryConfig {
        let defaults = QueryConfig::default();
        QueryConfig {
            limits: self.limit_config(),
            workers: if self.server.workers == 0 { defaults.workers } else { self.server.workers },
            queue: self.server.queue,
        }
    }

    pub fn drain_timeout(&self) -> Duration {
        Duration::from_secs(self.server.drain_timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::assert_matches;

    use super::*;

    #[test]
    fn only_the_data_directory_is_required() {
        let config = Config::parse("data_dir = \"/tmp/x\"").unwrap();
        assert_eq!(config.listen.to_string(), DEFAULT_LISTEN);
        assert_eq!(config.limit_config(), LimitConfig::default());
        assert_eq!(config.store_options(), StoreOptions::default());
        assert_eq!(config.drain_timeout(), Duration::from_secs(30));
        assert!(Config::parse("listen = \"127.0.0.1:1\"").is_err());
    }

    #[test]
    fn the_module_example_parses() {
        let doc = include_str!("config.rs");
        let example: String = doc
            .lines()
            .skip_while(|l| !l.starts_with("//! ```toml"))
            .skip(1)
            .take_while(|l| !l.starts_with("//! ```"))
            .map(|l| l.trim_start_matches("//!").trim_start().to_owned() + "\n")
            .collect();
        let config = Config::parse(&example).unwrap();
        let minimal = Config::parse("data_dir = \"/var/lib/iwdb\"").unwrap();
        // The example shows the defaults
        assert_eq!((&config.store, &config.server), (&minimal.store, &minimal.server));
        assert_eq!(config.limit_config(), minimal.limit_config());
        let partial =
            Config::parse("data_dir = \"d\"\n[limits.max]\nmax_results = 5\n[limits.default]\nmax_results = 5")
                .unwrap();
        assert_eq!(partial.limit_config().max_limits.max_visited, LimitConfig::MAX_LIMITS.max_visited);
    }

    #[test]
    fn invalid_files_say_why() {
        for (text, why) in [
            ("data_dir = \"d\"\nlisten = \"nowhere\"", "listen"),
            ("data_dir = \"d\"\n[store]\nfsync = \"sometimes\"", "sometimes"),
            ("data_dir = \"d\"\nunknown = 1", "unknown"),
            ("data_dir = \"d\"\n[limits.default]\nmax_results = 0", "max_results"),
            ("data_dir = \"d\"\n[limits.default]\nmax_results = 200000", "max_results"),
            ("data_dir = \"d\"\n[store]\nfsync = \"group\"\ngroup_max_batch = 0", "group_max_batch"),
        ] {
            let e = Config::parse(text).unwrap_err();
            assert!(e.contains(why), "{}: {}", why, e);
        }
        let config =
            Config::parse("data_dir = \"d\"\n[store]\nfsync = \"group\"\ncheckpoint_on_shutdown = false").unwrap();
        let options = config.store_options();
        assert_matches!(options.wal.fsync, FsyncPolicy::Group { max_batch: 64, .. });
        assert!(!options.checkpoint.on_close);
    }

    #[test]
    fn a_relative_data_directory_is_relative_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.toml");
        std::fs::write(&path, "data_dir = \"data\"").unwrap();
        assert_eq!(Config::load(&path).unwrap().data_dir, dir.path().join("data"));
        assert_matches!(Config::load(&dir.path().join("missing.toml")), Err(ConfigError::Read { .. }));
    }
}
