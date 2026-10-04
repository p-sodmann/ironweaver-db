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
//! retain_records = 0                # keep the WAL of the last N commits for the change stream
//! retain_age_secs = 0               # ... and of commits younger than this (0: none)
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
//! Projections (step 13, ADR 0032; `documentation/api/projections.md`):
//! each `[[projection]]` follows a source and commits its events into a
//! namespace, with its high-water mark in the same commit. They start with
//! the server and stop when it shuts down.
//!
//! ```toml
//! [[projection]]
//! name = "orders"                   # the mark's name: unique in its namespace
//! namespace = "default"             # must exist
//! batch = 100                       # events per commit
//! poll_ms = 500                     # how often to look for new events when caught up
//! on_error = "stop"                 # stop | skip: an event that fails to map or commit
//!
//! [projection.source]
//! kind = "postgres"
//! url_env = "ORDERS_DB_URL"         # or url = "postgresql://..." (no TLS until step 15)
//! table = "public.order_events"
//! position = "id"                   # a bigint that increases with every event
//! gap_timeout_ms = 5000             # how long a hole in the positions is waited for
//!
//! [[projection.rule]]
//! when = { kind = "order_placed" }
//! mutations = [
//!   { upsert_node = { id = "order:${id}", labels = ["Order"], attr = { total = "${payload.total}" } } },
//! ]
//! ```
//!
//! Environment overrides come with step 16. TLS and authentication with
//! step 15: until then the server listens on plain TCP, so bind it to
//! localhost or a private network.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(feature = "postgres")]
use iwdb::projection::OnError;
#[cfg(feature = "postgres")]
use iwdb::projection::postgres::{PostgresConfig, PostgresSource};
use iwdb::projection::{Projection, ProjectionHandle, ProjectionOptions, Rules};
use iwdb::{CheckpointOptions, FsyncPolicy, MarkName, QueryConfig, Store, StoreOptions, WalRetention};
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
#[derive(Clone, Debug, PartialEq, Deserialize)]
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
    /// `[[projection]]` sections (ADR 0032).
    #[serde(default, rename = "projection")]
    pub projections: Vec<ProjectionSection>,
}

/// A projection the server runs (ADR 0032).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionSection {
    /// The name of its mark, unique in its namespace.
    pub name: String,
    #[serde(default = "default_namespace")]
    pub namespace: String,
    pub source: SourceSection,
    #[serde(default = "default_batch")]
    pub batch: usize,
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
    #[serde(default)]
    pub on_error: OnErrorSection,
    /// The mapping: `[[projection.rule]]`.
    #[serde(default, rename = "rule")]
    pub rules: Rules,
}

/// Where a projection reads.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum SourceSection {
    Postgres {
        /// The connection string, or `url_env`: the environment variable
        /// that holds it (so a password needn't be in the file).
        url: Option<String>,
        url_env: Option<String>,
        table: String,
        position: String,
        columns: Option<Vec<String>>,
        #[serde(default = "default_gap_timeout_ms")]
        gap_timeout_ms: u64,
    },
}

impl ProjectionSection {
    /// The projection, ready to run: its source (the URL read from the
    /// environment now, for `url_env`), rules and options.
    #[cfg(feature = "postgres")]
    fn projection(&self) -> Result<Projection, String> {
        let at = format!("[[projection]] {:?}", self.name);
        let SourceSection::Postgres { url, url_env, table, position, columns, gap_timeout_ms } = &self.source;
        let url = match (url, url_env) {
            (Some(url), _) => url.clone(),
            (None, Some(var)) => {
                std::env::var(var).map_err(|_| format!("{}: the environment variable {} is not set", at, var))?
            }
            (None, None) => return Err(format!("{}: the source needs one of url and url_env", at)),
        };
        let config = PostgresConfig {
            url,
            table: table.clone(),
            position: position.clone(),
            columns: columns.clone(),
            gap_timeout: Duration::from_millis(*gap_timeout_ms),
        };
        let source = PostgresSource::new(config).map_err(|e| format!("{}: {}", at, e))?;
        let name = MarkName::new(self.name.as_str()).map_err(|e| format!("{}: {}", at, e))?;
        Ok(Projection::new(name, source, self.rules.clone(), self.options()))
    }

    /// Without the `postgres` feature there is no source to read.
    #[cfg(not(feature = "postgres"))]
    fn projection(&self) -> Result<Projection, String> {
        Err(no_postgres(&format!("[[projection]] {:?}", self.name)))
    }

    #[cfg(feature = "postgres")]
    fn options(&self) -> ProjectionOptions {
        ProjectionOptions {
            batch: self.batch,
            poll: Duration::from_millis(self.poll_ms),
            on_error: match self.on_error {
                OnErrorSection::Stop => OnError::Stop,
                OnErrorSection::Skip => OnError::Skip,
            },
            ..ProjectionOptions::default()
        }
    }
}

/// The error for a Postgres source in a server built without them (ADR 0034).
fn no_postgres(at: &str) -> String {
    format!("{}: this iwdb-server was built without the postgres feature, so it has no Postgres source", at)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnErrorSection {
    #[default]
    Stop,
    Skip,
}

fn default_namespace() -> String {
    iwdb::NAMESPACE.to_owned()
}

fn default_batch() -> usize {
    ProjectionOptions::default().batch
}

fn default_poll_ms() -> u64 {
    ProjectionOptions::default().poll.as_millis() as u64
}

fn default_gap_timeout_ms() -> u64 {
    5000
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StoreSection {
    pub fsync: Fsync,
    pub group_max_delay_ms: u64,
    pub group_max_batch: u32,
    pub checkpoint_on_shutdown: bool,
    /// WAL retention for the change stream (ADR 0031): the last this many
    /// commits ...
    pub retain_records: u64,
    /// ... and commits younger than this many seconds (0: none).
    pub retain_age_secs: u64,
}

impl Default for StoreSection {
    fn default() -> Self {
        StoreSection {
            fsync: Fsync::Always,
            group_max_delay_ms: 10,
            group_max_batch: 64,
            checkpoint_on_shutdown: true,
            retain_records: 0,
            retain_age_secs: 0,
        }
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
        let mut names = std::collections::BTreeSet::new();
        for p in &config.projections {
            let at = format!("[[projection]] {:?}", p.name);
            MarkName::new(p.name.as_str()).map_err(|e| format!("{}: {}", at, e))?;
            if !names.insert((p.namespace.as_str(), p.name.as_str())) {
                return Err(format!("{}: the name is used twice in namespace {:?}", at, p.namespace));
            }
            if p.batch == 0 {
                return Err(format!("{}: batch must be at least 1", at));
            }
            let SourceSection::Postgres { url, url_env, .. } = &p.source;
            if url.is_some() == url_env.is_some() {
                return Err(format!("{}: the source needs one of url and url_env", at));
            }
            if cfg!(not(feature = "postgres")) {
                return Err(no_postgres(&at));
            }
        }
        Ok(config)
    }

    /// The store's options: the defaults, with the fsync policy, checkpoint
    /// on shutdown and WAL retention from the file.
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
        options.retention = WalRetention {
            records: self.store.retain_records,
            age: (self.store.retain_age_secs > 0).then(|| Duration::from_secs(self.store.retain_age_secs)),
        };
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

    /// The projections of the file, ready to run: a source each (its URL
    /// read from the environment now, for `url_env`) and its rules, with
    /// the namespace to run in.
    pub fn projections(&self) -> Result<Vec<(String, Projection)>, String> {
        self.projections.iter().map(|p| p.projection().map(|projection| (p.namespace.clone(), projection))).collect()
    }

    /// Start the file's projections on `store` (each on a thread of its
    /// own; closing the store stops them). Fails, starting none, if one
    /// can't be made or its namespace doesn't exist.
    pub fn start_projections<F>(&self, store: &Store<F>) -> Result<Vec<ProjectionHandle>, String>
    where
        F: iwdb::LogFs + Clone + Send + Sync + 'static,
        F::File: Send,
    {
        let projections = self.projections()?;
        for (namespace, p) in &projections {
            if store.namespace(namespace).is_err() {
                return Err(format!("[[projection]] {:?}: there is no namespace {:?}", p.name().as_str(), namespace));
            }
        }
        let mut handles = Vec::new();
        for (namespace, p) in projections {
            let name = p.name().as_str().to_owned();
            handles.push(store.project(&namespace, p).map_err(|e| format!("[[projection]] {:?}: {}", name, e))?);
        }
        Ok(handles)
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
        // The projection example, after the first block
        let second: String = doc
            .lines()
            .skip_while(|l| !l.starts_with("//! [[projection]]"))
            .take_while(|l| !l.starts_with("//! ```"))
            .map(|l| l.trim_start_matches("//!").trim_start().to_owned() + "\n")
            .collect();
        let projections = Config::parse(&format!("data_dir = \"d\"\n{}", second));
        #[cfg(feature = "postgres")]
        {
            let projections = projections.unwrap().projections;
            assert_eq!((projections.len(), projections[0].rules.0.len(), projections[0].poll_ms), (1, 1, 500));
        }
        #[cfg(not(feature = "postgres"))]
        assert!(projections.unwrap_err().contains("without the postgres feature"));
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
        let config = Config::parse("data_dir = \"d\"\n[store]\nretain_records = 500\nretain_age_secs = 3600").unwrap();
        let retention = WalRetention { records: 500, age: Some(Duration::from_secs(3600)) };
        assert_eq!(config.store_options().retention, retention);
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn projections_are_checked() {
        let base = "data_dir = \"d\"\n";
        let section = |extra: &str| {
            format!(
                "{}[[projection]]\nname = \"p\"\n{}\n[projection.source]\nkind = \"postgres\"\nurl = \"host=x\"\ntable = \"t\"\nposition = \"id\"\n",
                base, extra
            )
        };
        let config = Config::parse(&section("")).unwrap();
        let p = &config.projections[0];
        assert_eq!((p.namespace.as_str(), p.batch, p.on_error), ("default", 100, OnErrorSection::Stop));
        assert!(p.rules.0.is_empty());
        assert_eq!(config.projections().unwrap()[0].1.name().as_str(), "p");
        for (text, why) in [
            (section("batch = 0"), "batch"),
            (section("on_error = \"retry\""), "retry"),
            (section("color = 1"), "color"),
            (format!("{}{}", section(""), &section("")[base.len()..]), "twice"),
            (section("").replace("url = \"host=x\"\n", ""), "url_env"),
            (section("").replace("kind = \"postgres\"", "kind = \"kafka\""), "kafka"),
            (section("").replace("name = \"p\"", "name = \"\""), "mark name"),
        ] {
            let e = Config::parse(&text).unwrap_err();
            assert!(e.contains(why), "{}: {}", why, e);
        }
        let from_env = section("").replace("url = \"host=x\"", "url_env = \"IWDB_TEST_UNSET_VARIABLE\"");
        let e = Config::parse(&from_env).unwrap().projections().err().unwrap();
        assert!(e.contains("IWDB_TEST_UNSET_VARIABLE"), "{}", e);
    }

    /// A server built without Postgres refuses a Postgres source when it
    /// reads its config, before it opens the store (ADR 0034).
    #[cfg(not(feature = "postgres"))]
    #[test]
    fn a_postgres_source_needs_the_postgres_feature() {
        let text = "data_dir = \"d\"\n[[projection]]\nname = \"p\"\n[projection.source]\nkind = \"postgres\"\nurl = \"host=x\"\ntable = \"t\"\nposition = \"id\"\n";
        let e = Config::parse(text).unwrap_err();
        assert!(e.contains("[[projection]] \"p\"") && e.contains("postgres feature"), "{}", e);
        // Checked before the feature: a section that is wrong anyway says so
        let e = Config::parse(&text.replace("url = \"host=x\"\n", "")).unwrap_err();
        assert!(e.contains("url_env"), "{}", e);
        // A config without projections is fine
        assert!(Config::parse("data_dir = \"d\"").unwrap().projections().unwrap().is_empty());
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
