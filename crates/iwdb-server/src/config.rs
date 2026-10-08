//! The `iwdb-server` configuration: a TOML file, environment overrides of
//! its settings (`IWDB_*`, [`KEYS`]), or both (ADR 0039). Only `data_dir`
//! is required, in the file or as `IWDB_DATA_DIR`:
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
//! unready_delay_ms = 0              # on shutdown: serve unready this long before draining
//! plaintext_public = false          # with [tls] enabled = false: allow a non-loopback listen address
//!
//! [memory]                          # step 16d, ADR 0054
//! # limit_bytes = 4294967296        # what the server may use; unset: the cgroup's limit (Linux), else none; 0: none
//! warn_at = 0.8                     # warn from this fraction of the limit on
//! refuse_writes_at = 0.9            # refuse writes (resource_exhausted) from here on; deletes and reads go on
//!
//! [tls]                             # step 15b
//! enabled = false                   # true by default: then cert and key are required
//! # cert = "tls/server.pem"         # PEM: the certificate chain, the server's first
//! # key = "tls/server.key"          # PEM: its private key (PKCS#8, PKCS#1 or SEC1)
//! # client_ca = "tls/ca.pem"        # mTLS: verify client certificates against these CAs
//! client_auth = "optional"          # optional | required: whether a client certificate is required
//!
//! [auth]                            # step 15a
//! enabled = true                    # every call but login and health needs a token
//! session_lifetime_secs = 43200     # how long a login's session lasts
//! login_max_failures = 5            # failed logins per user and per address ...
//! login_window_secs = 60            # ... within this window, then refused until it has passed
//! login_table_size = 10000          # users and addresses the slowdown remembers
//!
//! [log]
//! format = "auto"                   # auto (json unless on a terminal) | json | text
//! level = "info"                    # a filter: "warn,iwdb_storage=debug"
//! tail_events = 1000                # log events kept for GetLog and the console
//!
//! [console]                         # the operator console at /console/ (feature `console`)
//! enabled = false
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
//! url_env = "ORDERS_DB_URL"         # or url = "postgresql://..." (connects without TLS)
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
//! **Environment overrides.** Every setting above but the projections can
//! be set as `IWDB_<KEY>`: the key's path in upper case, `.` as `_`
//! (`IWDB_LISTEN`, `IWDB_STORE_FSYNC`, `IWDB_LIMITS_MAX_TIMEOUT_MS`). A
//! variable wins over the file. A variable that starts like a section
//! (`IWDB_STORE_`, `IWDB_SERVER_`, `IWDB_MEMORY_`, `IWDB_LIMITS_`, `IWDB_LOG_`,
//! `IWDB_CONSOLE_`, `IWDB_AUTH_`, `IWDB_TLS_`) but names no setting is an
//! error, so a typo isn't ignored. With `IWDB_DATA_DIR` set, the file is optional.
//!
//! **Bootstrap** (ADR 0047): `IWDB_AUTH_BOOTSTRAP_PASSWORD` (and
//! optionally `IWDB_AUTH_BOOTSTRAP_USER`, default `admin`) creates the first
//! admin on a start that finds no users; with users it is ignored with a
//! warning. Variables only (a password doesn't belong in a file), never
//! printed by `--check-config`.
//!
//! **Validation.** [`Config::from_sources`] reports every problem at once,
//! each with the variable it came from; a TOML syntax or type error in the
//! file stops the file's checks at the first. All of this happens before
//! the store opens.
//!
//! **TLS** (step 15b, ADR 0048) is on by default, so `[tls] cert` and
//! `key` are required unless `[tls] enabled = false`; relative paths from
//! the file are relative to its directory. Plaintext on a non-loopback
//! listen address needs a second flag, `[server] plaintext_public = true`.
//! With `[tls] client_ca`, clients may authenticate with a certificate
//! (mTLS): its subject's common name is the user.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use iwdb::auth::AuthSettings;
#[cfg(feature = "postgres")]
use iwdb::projection::OnError;
#[cfg(feature = "postgres")]
use iwdb::projection::postgres::{PostgresConfig, PostgresSource};
use iwdb::projection::{Projection, ProjectionHandle, ProjectionOptions, Rules};
use iwdb::{
    CheckpointOptions, FsyncPolicy, LimitSource, MarkName, MemoryOptions, QueryConfig, Store, StoreOptions,
    WalRetention,
};
use iwdb_query::{Bounds, LimitConfig, Secret};
use serde::{Deserialize, Serialize};

use crate::DEFAULT_MAX_MESSAGE_BYTES;
use crate::logging::LogFormat;

/// The default listen address.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7600";

/// A config file that can't be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("can't read the config file {}: {source}", path.display())]
    Read { path: PathBuf, source: std::io::Error },
    /// Every problem found, each saying where (the file, a variable).
    #[error("invalid configuration{}:\n  - {}", path.as_ref().map(|p| format!(" ({})", p.display())).unwrap_or_default(), problems.join("\n  - "))]
    Invalid { path: Option<PathBuf>, problems: Vec<String> },
}

/// The server's configuration (see the module docs for the file).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The store's data directory, created if missing. Required (empty
    /// means missing).
    #[serde(default)]
    pub data_dir: PathBuf,
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    #[serde(default)]
    pub store: StoreSection,
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub memory: MemorySection,
    #[serde(default)]
    pub limits: LimitsSection,
    #[serde(default)]
    pub log: LogSection,
    #[serde(default)]
    pub console: ConsoleSection,
    #[serde(default)]
    pub auth: AuthSection,
    #[serde(default)]
    pub tls: TlsSection,
    #[serde(default)]
    pub audit: AuditSection,
    #[serde(default)]
    pub backup: BackupSection,
    #[serde(default)]
    pub jobs: JobsSection,
    #[serde(default)]
    pub tracing: TracingSection,
    /// `[[projection]]` sections (ADR 0032).
    #[serde(default, rename = "projection")]
    pub projections: Vec<ProjectionSection>,
    /// Where each setting set by the file or the environment came from.
    #[serde(skip)]
    sources: BTreeMap<&'static str, Source>,
    /// The first admin to create on a start without users
    /// (`IWDB_AUTH_BOOTSTRAP_USER` and `IWDB_AUTH_BOOTSTRAP_PASSWORD`).
    #[serde(skip)]
    pub bootstrap: Option<(String, Secret)>,
    /// The `OTEL_*` variables that were set: not read (ADR 0057), so the
    /// server warns about them.
    #[serde(skip)]
    otel_vars: Vec<String>,
}

/// `[tracing]`: OpenTelemetry traces over OTLP (step 16g, ADR 0057; feature
/// `otel`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TracingSection {
    pub enabled: bool,
    /// The collector. Unset: the protocol's default on this host
    /// ([`OtlpProtocol::default_endpoint`]).
    pub endpoint: Option<String>,
    pub protocol: OtlpProtocol,
    /// The share of new traces sampled, 0 to 1; a request with a parent
    /// follows the parent's decision.
    pub sample_ratio: f64,
    pub service_name: String,
    /// `name=value,name=value`, sent with every export.
    pub headers: Option<String>,
}

impl Default for TracingSection {
    fn default() -> Self {
        TracingSection {
            enabled: false,
            endpoint: None,
            protocol: OtlpProtocol::Grpc,
            sample_ratio: 1.0,
            service_name: "iwdb-server".into(),
            headers: None,
        }
    }
}

/// How spans are exported: OTLP over gRPC, or protobuf over HTTP.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum OtlpProtocol {
    #[default]
    #[serde(rename = "grpc")]
    Grpc,
    #[serde(rename = "http/protobuf")]
    HttpProtobuf,
}

impl OtlpProtocol {
    /// The collector's conventional address on this host.
    pub fn default_endpoint(self) -> &'static str {
        match self {
            OtlpProtocol::Grpc => "http://127.0.0.1:4317",
            OtlpProtocol::HttpProtobuf => "http://127.0.0.1:4318/v1/traces",
        }
    }
}

/// `[auth]`: authentication (step 15a, ADRs 0044 and 0047).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthSection {
    /// Every call but login and health needs a token. Off, every caller is
    /// a server-wide admin.
    pub enabled: bool,
    pub session_lifetime_secs: u64,
    pub login_max_failures: u32,
    pub login_window_secs: u64,
    pub login_table_size: usize,
}

impl Default for AuthSection {
    fn default() -> Self {
        let d = AuthSettings::default();
        AuthSection {
            enabled: true,
            session_lifetime_secs: d.session_lifetime.as_secs(),
            login_max_failures: d.max_failures,
            login_window_secs: d.failure_window.as_secs(),
            login_table_size: d.table_size,
        }
    }
}

/// `[tls]`: TLS and mTLS (step 15b, ADR 0048).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TlsSection {
    /// Serve TLS only. Off, the server speaks plaintext, which a
    /// non-loopback listen address also needs `[server] plaintext_public`
    /// for.
    pub enabled: bool,
    /// The certificate chain (PEM), the server's certificate first.
    pub cert: Option<PathBuf>,
    /// The certificate's private key (PEM).
    pub key: Option<PathBuf>,
    /// The CAs (PEM) that client certificates are verified against: turns
    /// mTLS on.
    pub client_ca: Option<PathBuf>,
    pub client_auth: ClientAuth,
}

impl Default for TlsSection {
    fn default() -> Self {
        TlsSection { enabled: true, cert: None, key: None, client_ca: None, client_auth: ClientAuth::Optional }
    }
}

/// `[audit]`: the audit log's files (step 15c, ADR 0049). The entries
/// always go to the log (target `iwdb::audit`); with `dir`, also to a
/// file per UTC day there.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuditSection {
    /// The directory of the audit files (`audit-YYYY-MM-DD.jsonl`), made
    /// if missing. `None`: only the log.
    pub dir: Option<PathBuf>,
    /// Days of audit files to keep, today included; older ones are
    /// deleted at start and at each new day. 0: keep them all.
    pub retention_days: u32,
}

/// `[backup]`: online backups (step 16e, ADR 0055).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BackupSection {
    /// The backup directory: the only place remote backups are written
    /// (as `<dir>/<name>`), and where `Verify` and `PruneArchive` find them.
    /// It must exist, and not be inside the data directory. `None`: remote
    /// backups are refused.
    pub dir: Option<PathBuf>,
    /// Copy backups at most this fast, in bytes per second (0: as fast as
    /// the disks go). Checkpoints wait for a backup's whole copy.
    pub max_bytes_per_second: u64,
}

/// `[jobs]`: managed analytics jobs (step 16f, ADR 0056).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct JobsSection {
    /// Jobs running at once, each on a thread of its own (not a query
    /// worker).
    pub running: usize,
    /// Jobs that may wait; more are refused with `unavailable`.
    pub queued: usize,
    /// A user's queued and running jobs at once; more are refused.
    pub per_user: usize,
    /// The longest a job runs, from when it leaves the queue.
    pub timeout_secs: u64,
    /// How long an ended job and its result are kept.
    pub retention_secs: u64,
    /// Ended jobs kept at most (the oldest go first).
    pub max_finished: usize,
    /// Stored results' estimated bytes, together, at most (the oldest are
    /// dropped first).
    pub result_bytes: u64,
}

impl Default for JobsSection {
    fn default() -> Self {
        let d = iwdb_query::jobs::JobsConfig::default();
        JobsSection {
            running: d.running,
            queued: d.queued,
            per_user: d.per_user,
            timeout_secs: d.timeout.as_secs(),
            retention_secs: d.retention.as_secs(),
            max_finished: d.max_finished,
            result_bytes: d.result_bytes,
        }
    }
}

impl JobsSection {
    /// The job registry's bounds.
    pub fn config(&self) -> iwdb_query::jobs::JobsConfig {
        iwdb_query::jobs::JobsConfig {
            running: self.running,
            queued: self.queued,
            per_user: self.per_user,
            timeout: Duration::from_secs(self.timeout_secs),
            retention: Duration::from_secs(self.retention_secs),
            max_finished: self.max_finished,
            result_bytes: self.result_bytes,
        }
    }
}

/// The default of `[audit] retention_days`.
pub const DEFAULT_AUDIT_RETENTION_DAYS: u32 = 30;

impl Default for AuditSection {
    fn default() -> Self {
        AuditSection { dir: None, retention_days: DEFAULT_AUDIT_RETENTION_DAYS }
    }
}

/// Whether a client certificate is required (with `[tls] client_ca`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientAuth {
    /// A client may present one; one it presents must be valid.
    Optional,
    /// Every request but health and the console's pages needs one.
    Required,
}

/// The bootstrap variables (ADR 0047): read, but not settings.
pub const BOOTSTRAP_USER_VAR: &str = "IWDB_AUTH_BOOTSTRAP_USER";
pub const BOOTSTRAP_PASSWORD_VAR: &str = "IWDB_AUTH_BOOTSTRAP_PASSWORD";

/// Where a setting's value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Default,
    File,
    /// An environment variable.
    Env(&'static str),
}

/// `[log]`: structured logs (ADR 0042).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LogSection {
    pub format: LogFormat,
    /// A filter in `tracing-subscriber`'s `EnvFilter` syntax.
    pub level: String,
    /// The log events kept for `GetLog` and the console (step 16c); 0 keeps
    /// none.
    pub tail_events: usize,
}

impl Default for LogSection {
    fn default() -> Self {
        LogSection { format: LogFormat::Auto, level: "info".into(), tail_events: iwdb_query::log::DEFAULT_EVENTS }
    }
}

/// `[console]`: the operator console's pages at `/console/` (feature
/// `console`, ADR 0041).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ConsoleSection {
    pub enabled: bool,
    /// Replaced by `[server] plaintext_public` in step 15a; refused with a
    /// message that says so.
    #[serde(skip_serializing)]
    pub public: Option<bool>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            data_dir: PathBuf::new(),
            listen: default_listen(),
            store: StoreSection::default(),
            server: ServerSection::default(),
            memory: MemorySection::default(),
            limits: LimitsSection::default(),
            log: LogSection::default(),
            console: ConsoleSection::default(),
            auth: AuthSection::default(),
            tls: TlsSection::default(),
            audit: AuditSection::default(),
            backup: BackupSection::default(),
            jobs: JobsSection::default(),
            tracing: TracingSection::default(),
            projections: Vec::new(),
            sources: BTreeMap::new(),
            bootstrap: None,
            otel_vars: Vec::new(),
        }
    }
}

/// A setting that the environment can override ([`KEYS`]).
pub struct Key {
    /// Its path in the file (`store.fsync`).
    pub key: &'static str,
    /// Its variable (`IWDB_STORE_FSYNC`).
    pub var: &'static str,
    set: fn(&mut Config, &str) -> Result<(), String>,
    get: fn(&Config) -> Option<toml::Value>,
}

/// A setting's type, read from a variable's text.
trait FromEnv: Sized {
    fn from_env(text: &str) -> Result<Self, String>;
}

macro_rules! from_str_env {
    ($($t:ty: $what:literal),*) => {$(
        impl FromEnv for $t {
            fn from_env(text: &str) -> Result<Self, String> {
                text.trim().parse().map_err(|_| format!("expected {}, got {:?}", $what, text))
            }
        }
    )*};
}
from_str_env!(u64: "a whole number", u32: "a whole number", usize: "a whole number", f64: "a number like 0.9", SocketAddr: "an address like 127.0.0.1:7600");

impl FromEnv for bool {
    fn from_env(text: &str) -> Result<Self, String> {
        match text.trim() {
            "true" | "1" => Ok(true),
            "false" | "0" => Ok(false),
            _ => Err(format!("expected true or false, got {:?}", text)),
        }
    }
}

impl FromEnv for PathBuf {
    fn from_env(text: &str) -> Result<Self, String> {
        if text.is_empty() { Err("expected a path, got nothing".into()) } else { Ok(PathBuf::from(text)) }
    }
}

impl FromEnv for String {
    fn from_env(text: &str) -> Result<Self, String> {
        Ok(text.to_owned())
    }
}

impl<T: FromEnv> FromEnv for Option<T> {
    fn from_env(text: &str) -> Result<Self, String> {
        T::from_env(text).map(Some)
    }
}

/// The enums: their names, as in the file.
macro_rules! serde_env {
    ($($t:ty),*) => {$(
        impl FromEnv for $t {
            fn from_env(text: &str) -> Result<Self, String> {
                use serde::de::IntoDeserializer;
                let d: serde::de::value::StrDeserializer<'_, serde::de::value::Error> = text.trim().into_deserializer();
                <$t>::deserialize(d).map_err(|e| e.to_string())
            }
        }
    )*};
}
serde_env!(Fsync, LogFormat, ClientAuth, OtlpProtocol);

macro_rules! keys {
    ($($key:literal $var:literal => $($field:ident).+;)*) => {
        /// Every setting of the file but the projections, with its variable:
        /// the one list the docs and `--check-config` are checked against.
        pub const KEYS: &[Key] = &[$(
            Key {
                key: $key,
                var: $var,
                set: |c, text| { c.$($field).+ = FromEnv::from_env(text)?; Ok(()) },
                get: |c| toml::Value::try_from(&c.$($field).+).ok(),
            },
        )*];
    };
}

keys! {
    "data_dir" "IWDB_DATA_DIR" => data_dir;
    "listen" "IWDB_LISTEN" => listen;
    "store.fsync" "IWDB_STORE_FSYNC" => store.fsync;
    "store.group_max_delay_ms" "IWDB_STORE_GROUP_MAX_DELAY_MS" => store.group_max_delay_ms;
    "store.group_max_batch" "IWDB_STORE_GROUP_MAX_BATCH" => store.group_max_batch;
    "store.checkpoint_on_shutdown" "IWDB_STORE_CHECKPOINT_ON_SHUTDOWN" => store.checkpoint_on_shutdown;
    "store.retain_records" "IWDB_STORE_RETAIN_RECORDS" => store.retain_records;
    "store.retain_age_secs" "IWDB_STORE_RETAIN_AGE_SECS" => store.retain_age_secs;
    "store.archive" "IWDB_STORE_ARCHIVE" => store.archive;
    "server.drain_timeout_secs" "IWDB_SERVER_DRAIN_TIMEOUT_SECS" => server.drain_timeout_secs;
    "server.max_message_bytes" "IWDB_SERVER_MAX_MESSAGE_BYTES" => server.max_message_bytes;
    "server.workers" "IWDB_SERVER_WORKERS" => server.workers;
    "server.queue" "IWDB_SERVER_QUEUE" => server.queue;
    "server.unready_delay_ms" "IWDB_SERVER_UNREADY_DELAY_MS" => server.unready_delay_ms;
    "server.plaintext_public" "IWDB_SERVER_PLAINTEXT_PUBLIC" => server.plaintext_public;
    "memory.limit_bytes" "IWDB_MEMORY_LIMIT_BYTES" => memory.limit_bytes;
    "memory.warn_at" "IWDB_MEMORY_WARN_AT" => memory.warn_at;
    "memory.refuse_writes_at" "IWDB_MEMORY_REFUSE_WRITES_AT" => memory.refuse_writes_at;
    "limits.default.max_results" "IWDB_LIMITS_DEFAULT_MAX_RESULTS" => limits.default.max_results;
    "limits.default.max_visited" "IWDB_LIMITS_DEFAULT_MAX_VISITED" => limits.default.max_visited;
    "limits.default.max_edges" "IWDB_LIMITS_DEFAULT_MAX_EDGES" => limits.default.max_edges;
    "limits.default.timeout_ms" "IWDB_LIMITS_DEFAULT_TIMEOUT_MS" => limits.default.timeout_ms;
    "limits.max.max_results" "IWDB_LIMITS_MAX_MAX_RESULTS" => limits.max.max_results;
    "limits.max.max_visited" "IWDB_LIMITS_MAX_MAX_VISITED" => limits.max.max_visited;
    "limits.max.max_edges" "IWDB_LIMITS_MAX_MAX_EDGES" => limits.max.max_edges;
    "limits.max.timeout_ms" "IWDB_LIMITS_MAX_TIMEOUT_MS" => limits.max.timeout_ms;
    "log.format" "IWDB_LOG_FORMAT" => log.format;
    "log.level" "IWDB_LOG_LEVEL" => log.level;
    "log.tail_events" "IWDB_LOG_TAIL_EVENTS" => log.tail_events;
    "console.enabled" "IWDB_CONSOLE_ENABLED" => console.enabled;
    "auth.enabled" "IWDB_AUTH_ENABLED" => auth.enabled;
    "auth.session_lifetime_secs" "IWDB_AUTH_SESSION_LIFETIME_SECS" => auth.session_lifetime_secs;
    "auth.login_max_failures" "IWDB_AUTH_LOGIN_MAX_FAILURES" => auth.login_max_failures;
    "auth.login_window_secs" "IWDB_AUTH_LOGIN_WINDOW_SECS" => auth.login_window_secs;
    "auth.login_table_size" "IWDB_AUTH_LOGIN_TABLE_SIZE" => auth.login_table_size;
    "tls.enabled" "IWDB_TLS_ENABLED" => tls.enabled;
    "tls.cert" "IWDB_TLS_CERT" => tls.cert;
    "tls.key" "IWDB_TLS_KEY" => tls.key;
    "tls.client_ca" "IWDB_TLS_CLIENT_CA" => tls.client_ca;
    "tls.client_auth" "IWDB_TLS_CLIENT_AUTH" => tls.client_auth;
    "audit.dir" "IWDB_AUDIT_DIR" => audit.dir;
    "audit.retention_days" "IWDB_AUDIT_RETENTION_DAYS" => audit.retention_days;
    "backup.dir" "IWDB_BACKUP_DIR" => backup.dir;
    "backup.max_bytes_per_second" "IWDB_BACKUP_MAX_BYTES_PER_SECOND" => backup.max_bytes_per_second;
    "jobs.running" "IWDB_JOBS_RUNNING" => jobs.running;
    "jobs.queued" "IWDB_JOBS_QUEUED" => jobs.queued;
    "jobs.per_user" "IWDB_JOBS_PER_USER" => jobs.per_user;
    "jobs.timeout_secs" "IWDB_JOBS_TIMEOUT_SECS" => jobs.timeout_secs;
    "jobs.retention_secs" "IWDB_JOBS_RETENTION_SECS" => jobs.retention_secs;
    "jobs.max_finished" "IWDB_JOBS_MAX_FINISHED" => jobs.max_finished;
    "jobs.result_bytes" "IWDB_JOBS_RESULT_BYTES" => jobs.result_bytes;
    "tracing.enabled" "IWDB_TRACING_ENABLED" => tracing.enabled;
    "tracing.endpoint" "IWDB_TRACING_ENDPOINT" => tracing.endpoint;
    "tracing.protocol" "IWDB_TRACING_PROTOCOL" => tracing.protocol;
    "tracing.sample_ratio" "IWDB_TRACING_SAMPLE_RATIO" => tracing.sample_ratio;
    "tracing.service_name" "IWDB_TRACING_SERVICE_NAME" => tracing.service_name;
    "tracing.headers" "IWDB_TRACING_HEADERS" => tracing.headers;
}

/// Variables with these prefixes must name a setting.
const SECTION_PREFIXES: &[&str] = &[
    "IWDB_STORE_",
    "IWDB_SERVER_",
    "IWDB_MEMORY_",
    "IWDB_LIMITS_",
    "IWDB_LOG_",
    "IWDB_CONSOLE_",
    "IWDB_AUTH_",
    "IWDB_TLS_",
    "IWDB_AUDIT_",
    "IWDB_BACKUP_",
    "IWDB_JOBS_",
    "IWDB_TRACING_",
];

/// The settings that are paths, resolved against the file's directory when
/// they come from the file.
const PATH_KEYS: &[&str] =
    &["data_dir", "tls.cert", "tls.key", "tls.client_ca", "audit.dir", "store.archive", "backup.dir"];

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
    /// The WAL archive (ADR 0009): every segment is copied there, durably,
    /// before a checkpoint removes it. Created if missing; it must belong
    /// to the store's history.
    pub archive: Option<PathBuf>,
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
            archive: None,
        }
    }
}

/// The WAL's fsync policy (`documentation/guarantees.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
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
    /// On shutdown, serve this long with readiness off before draining
    /// (ADR 0040).
    pub unready_delay_ms: u64,
    /// With `[tls] enabled = false`, allow a non-loopback listen address:
    /// passwords, tokens and data cross the network in clear (ADRs 0047
    /// and 0048).
    pub plaintext_public: bool,
}

impl Default for ServerSection {
    fn default() -> Self {
        ServerSection {
            drain_timeout_secs: 30,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            workers: 0,
            queue: 1024,
            unready_delay_ms: 0,
            plaintext_public: false,
        }
    }
}

/// `[memory]`: the memory limit (step 16d, ADR 0054).
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MemorySection {
    /// What the server may use. Unset: the cgroup's limit on Linux, else
    /// none; 0: none.
    pub limit_bytes: Option<u64>,
    /// Warn from this fraction of the limit on.
    pub warn_at: f64,
    /// Refuse writes from this fraction of the limit on.
    pub refuse_writes_at: f64,
}

impl Default for MemorySection {
    fn default() -> Self {
        MemorySection {
            limit_bytes: None,
            warn_at: iwdb::memory::DEFAULT_WARN_AT,
            refuse_writes_at: iwdb::memory::DEFAULT_REFUSE_WRITES_AT,
        }
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

/// The value at the dotted `key` of a parsed file.
fn lookup<'a>(table: &'a toml::Table, key: &str) -> Option<&'a toml::Value> {
    let mut parts = key.split('.');
    let mut value = table.get(parts.next()?)?;
    for part in parts {
        value = value.as_table()?.get(part)?;
    }
    Some(value)
}

/// What a setting without a value means: a built-in limit, or nothing.
fn unset(key: &str) -> &'static str {
    match key {
        "memory.limit_bytes" => "the cgroup's, or none",
        "tracing.endpoint" => "the protocol's",
        _ if key.starts_with("limits.") => "built in",
        _ => "unset",
    }
}

/// An OTLP endpoint: an `http://` or `https://` URL with a host.
fn check_endpoint(endpoint: &str) -> Result<(), String> {
    let uri: http::Uri = endpoint.parse().map_err(|_| format!("{:?} isn't a URL", endpoint))?;
    match (uri.scheme_str(), uri.host()) {
        (Some("http" | "https"), Some(_)) => Ok(()),
        _ => Err(format!("{:?} must be an http:// or https:// URL with a host", endpoint)),
    }
}

fn default_listen() -> SocketAddr {
    // A constant that parses
    DEFAULT_LISTEN.parse().unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 7600)))
}

impl Config {
    /// Read and check the file at `path` alone (no environment); a relative
    /// `data_dir` is resolved against the file's directory.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        Config::from_sources(Some(path), std::iter::empty())
    }

    /// The configuration from the file at `path` (if any) and the
    /// variables of `env` (pass `std::env::vars()`), checked: every problem
    /// at once. A relative `data_dir` from the file is resolved against the
    /// file's directory; one from `IWDB_DATA_DIR` against the working
    /// directory.
    pub fn from_sources(
        path: Option<&Path>,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Config, ConfigError> {
        let text = match path {
            Some(path) => {
                Some(std::fs::read_to_string(path).map_err(|source| ConfigError::Read { path: path.into(), source })?)
            }
            None => None,
        };
        let env: Vec<(String, String)> = env.into_iter().collect();
        let mut config = Config::build(text.as_deref(), &env)
            .map_err(|problems| ConfigError::Invalid { path: path.map(Path::to_path_buf), problems })?;
        if let Some(dir) = path.and_then(Path::parent) {
            for key in PATH_KEYS {
                if config.source(key) != Source::File {
                    continue;
                }
                let field = match *key {
                    "data_dir" => Some(&mut config.data_dir),
                    "tls.cert" => config.tls.cert.as_mut(),
                    "tls.key" => config.tls.key.as_mut(),
                    "audit.dir" => config.audit.dir.as_mut(),
                    "store.archive" => config.store.archive.as_mut(),
                    "backup.dir" => config.backup.dir.as_mut(),
                    _ => config.tls.client_ca.as_mut(),
                };
                if let Some(field) = field
                    && field.is_relative()
                {
                    *field = dir.join(&*field);
                }
            }
        }
        Ok(config)
    }

    /// Parse and check a config file's text (no environment).
    pub fn parse(text: &str) -> Result<Config, String> {
        Config::build(Some(text), &[]).map_err(|problems| problems.join("\n"))
    }

    fn build(text: Option<&str>, env: &[(String, String)]) -> Result<Config, Vec<String>> {
        let mut problems = Vec::new();
        let mut config = Config::default();
        // A file that doesn't parse leaves nothing to check beyond the
        // variables
        let mut parsed = true;
        if let Some(text) = text {
            match toml::from_str::<Config>(text) {
                Ok(c) => config = c,
                Err(e) => {
                    parsed = false;
                    problems.push(e.to_string().trim_end().to_owned());
                }
            }
            if let Ok(table) = toml::from_str::<toml::Table>(text) {
                for k in KEYS {
                    if lookup(&table, k.key).is_some() {
                        config.sources.insert(k.key, Source::File);
                    }
                }
            }
        }
        let mut vars: Vec<&(String, String)> = env.iter().filter(|(var, _)| var.starts_with("IWDB_")).collect();
        vars.sort();
        for (var, value) in vars {
            if var == BOOTSTRAP_USER_VAR || var == BOOTSTRAP_PASSWORD_VAR {
                continue;
            }
            if var == "IWDB_CONSOLE_PUBLIC" {
                problems.push(format!(
                    "{}: gone since step 15a; the server speaks TLS ([tls] cert and key), and plaintext on a \
                     non-loopback address needs IWDB_TLS_ENABLED=false and IWDB_SERVER_PLAINTEXT_PUBLIC=true",
                    var
                ));
                continue;
            }
            match KEYS.iter().find(|k| k.var == var) {
                Some(k) => match (k.set)(&mut config, value) {
                    Ok(()) => {
                        config.sources.insert(k.key, Source::Env(k.var));
                    }
                    Err(e) => problems.push(format!("{}: {}", var, e)),
                },
                None if SECTION_PREFIXES.iter().any(|p| var.starts_with(p)) => {
                    problems.push(format!("{}: no such setting (the settings: documentation/api/config.md)", var))
                }
                None => {}
            }
        }
        config.otel_vars = env.iter().filter(|(var, _)| var.starts_with("OTEL_")).map(|(var, _)| var.clone()).collect();
        config.otel_vars.sort();
        // An empty variable is an unset one (compose's `${VAR:-}`)
        let get = |name: &str| env.iter().find(|(var, v)| var == name && !v.is_empty()).map(|(_, v)| v.clone());
        match (get(BOOTSTRAP_USER_VAR), get(BOOTSTRAP_PASSWORD_VAR)) {
            (user, Some(password)) => {
                config.bootstrap = Some((user.unwrap_or_else(|| "admin".into()), Secret::new(password)));
            }
            (Some(_), None) => problems.push(format!("{}: needs {} too", BOOTSTRAP_USER_VAR, BOOTSTRAP_PASSWORD_VAR)),
            (None, None) => {}
        }
        if parsed {
            problems.extend(config.check());
        }
        if problems.is_empty() { Ok(config) } else { Err(problems) }
    }

    /// Where `key`'s value came from ([`Key::key`]).
    pub fn source(&self, key: &str) -> Source {
        self.sources.get(key).copied().unwrap_or(Source::Default)
    }

    /// `key` for a message: `[section] name`, and the variable it came from.
    fn at(&self, key: &str) -> String {
        let named = match key.rsplit_once('.') {
            Some((section, name)) => format!("[{}] {}", section, name),
            None => key.to_owned(),
        };
        match self.source(key) {
            Source::Env(var) => format!("{} (from {})", named, var),
            _ => named,
        }
    }

    /// The checks beyond the types: every problem.
    fn check(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.data_dir.as_os_str().is_empty() {
            problems.push("data_dir is required: set it in the config file or as IWDB_DATA_DIR".to_owned());
        }
        if let Err(e) = self.limit_config().check() {
            let vars: Vec<&str> = KEYS
                .iter()
                .filter(|k| k.key.starts_with("limits."))
                .filter_map(|k| match self.source(k.key) {
                    Source::Env(var) => Some(var),
                    _ => None,
                })
                .collect();
            let from = if vars.is_empty() { String::new() } else { format!(" (from {})", vars.join(", ")) };
            problems.push(format!("[limits]: {}{}", e, from));
        }
        if self.store.fsync == Fsync::Group && self.store.group_max_batch == 0 {
            problems.push(format!("{} must be at least 1", self.at("store.group_max_batch")));
        }
        if let Err(e) = self.memory_options_with(None).check() {
            let at = |k| self.at(k);
            problems.push(format!("{}, {}: {}", at("memory.warn_at"), at("memory.refuse_writes_at"), e));
        }
        if self.server.max_message_bytes < 1024 {
            problems.push(format!("{} must be at least 1024", self.at("server.max_message_bytes")));
        }
        if self.log.tail_events > iwdb_query::log::MAX_EVENTS {
            problems.push(format!(
                "{}: at most {} events, not {}",
                self.at("log.tail_events"),
                iwdb_query::log::MAX_EVENTS,
                self.log.tail_events
            ));
        }
        if let Err(e) = crate::logging::check_level(&self.log.level) {
            problems.push(format!("{}: {}", self.at("log.level"), e));
        }
        if self.console.enabled && cfg!(not(feature = "console")) {
            problems.push(format!(
                "{}: this iwdb-server was built without the console feature",
                self.at("console.enabled")
            ));
        }
        if self.console.public.is_some() {
            problems.push(
                "[console] public: gone since step 15a; the server speaks TLS ([tls] cert and key), and plaintext \
                 on a non-loopback address needs [tls] enabled = false and [server] plaintext_public = true"
                    .to_owned(),
            );
        }
        problems.extend(self.check_tls());
        problems.extend(self.check_tracing());
        if self.auth.session_lifetime_secs == 0 {
            problems.push(format!("{} must be at least 1", self.at("auth.session_lifetime_secs")));
        }
        let j = &self.jobs;
        let counts = [
            ("jobs.running", j.running as u64),
            ("jobs.queued", j.queued as u64),
            ("jobs.per_user", j.per_user as u64),
            ("jobs.timeout_secs", j.timeout_secs),
            ("jobs.max_finished", j.max_finished as u64),
        ];
        for (key, n) in counts {
            if n == 0 {
                problems.push(format!("{} must be at least 1", self.at(key)));
            }
        }
        if self.auth.login_max_failures == 0 {
            problems.push(format!("{} must be at least 1", self.at("auth.login_max_failures")));
        }
        if self.auth.login_table_size == 0 {
            problems.push(format!("{} must be at least 1", self.at("auth.login_table_size")));
        }
        if let Some((user, password)) = &self.bootstrap {
            let n = password.expose().len();
            if !(iwdb::auth::MIN_PASSWORD_BYTES..=iwdb::auth::MAX_PASSWORD_BYTES).contains(&n) {
                problems.push(format!(
                    "{}: a password must be {} to {} bytes long",
                    BOOTSTRAP_PASSWORD_VAR,
                    iwdb::auth::MIN_PASSWORD_BYTES,
                    iwdb::auth::MAX_PASSWORD_BYTES
                ));
            }
            if iwdb::NamespaceName::new(user.as_str()).map_or(true, |n| n.is_reserved()) {
                problems.push(format!("{}: invalid user name {:?}", BOOTSTRAP_USER_VAR, user));
            }
        }
        let mut names = std::collections::BTreeSet::new();
        for p in &self.projections {
            let at = format!("[[projection]] {:?}", p.name);
            if let Err(e) = MarkName::new(p.name.as_str()) {
                problems.push(format!("{}: {}", at, e));
                continue;
            }
            if !names.insert((p.namespace.as_str(), p.name.as_str())) {
                problems.push(format!("{}: the name is used twice in namespace {:?}", at, p.namespace));
            }
            if p.batch == 0 {
                problems.push(format!("{}: batch must be at least 1", at));
            }
            let SourceSection::Postgres { url, url_env, .. } = &p.source;
            if url.is_some() == url_env.is_some() {
                problems.push(format!("{}: the source needs one of url and url_env", at));
            } else if cfg!(not(feature = "postgres")) {
                problems.push(no_postgres(&at));
            }
        }
        problems
    }

    /// TLS: on, with a certificate and key; or off, and then on a loopback
    /// address or with the second flag (ADR 0048).
    fn check_tls(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let tls = &self.tls;
        if tls.enabled {
            let missing: Vec<String> = [("tls.cert", tls.cert.is_none()), ("tls.key", tls.key.is_none())]
                .into_iter()
                .filter(|(_, missing)| *missing)
                .map(|(key, _)| self.at(key))
                .collect();
            if !missing.is_empty() {
                problems.push(format!(
                    "{} {} not set, and TLS is on ({}, the default): give the certificate and its key as PEM \
                     files ({}; docker/dev-cert.sh makes a pair for development), or set [tls] enabled = false \
                     (IWDB_TLS_ENABLED=false) to serve plaintext",
                    missing.join(" and "),
                    if missing.len() == 1 { "is" } else { "are" },
                    self.at("tls.enabled"),
                    "IWDB_TLS_CERT, IWDB_TLS_KEY",
                ));
            }
            if tls.client_auth == ClientAuth::Required && tls.client_ca.is_none() {
                problems.push(format!(
                    "{} is required, but {} isn't set: client certificates need the CAs to verify them against",
                    self.at("tls.client_auth"),
                    self.at("tls.client_ca")
                ));
            }
        } else {
            // Client certificates were meant to authenticate: not without TLS.
            // A certificate and key are only ignored (a warning), so that an
            // image's config with them can run in plaintext by a variable
            if tls.client_ca.is_some() {
                problems.push(format!("{} is set, but {} is false", self.at("tls.client_ca"), self.at("tls.enabled")));
            }
            if !self.listen.ip().is_loopback() && !self.server.plaintext_public {
                let exposed = if self.auth.enabled {
                    "passwords, tokens and data would cross the network in clear"
                } else {
                    "authentication is off, so anyone who reaches the port could read and change everything, in \
                     clear"
                };
                problems.push(format!(
                    "{} is {}, not a loopback address, and TLS is off ({}): {}; turn TLS on ([tls] cert and key), \
                     or set [server] plaintext_public = true (IWDB_SERVER_PLAINTEXT_PUBLIC) to listen there in \
                     plaintext anyway",
                    self.at("listen"),
                    self.listen,
                    self.at("tls.enabled"),
                    exposed
                ));
            }
        }
        problems
    }

    /// `[tracing]`: the feature, the ratio, the endpoint and the headers
    /// (never quoting a header's value: it may be a key).
    fn check_tracing(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let t = &self.tracing;
        if t.enabled && cfg!(not(feature = "otel")) {
            problems.push(format!(
                "{}: this iwdb-server was built without the otel feature, so it can't export traces",
                self.at("tracing.enabled")
            ));
        }
        if !(0.0..=1.0).contains(&t.sample_ratio) {
            problems.push(format!(
                "{} must be between 0 and 1, not {}",
                self.at("tracing.sample_ratio"),
                t.sample_ratio
            ));
        }
        if t.service_name.trim().is_empty() {
            problems.push(format!("{} must not be empty", self.at("tracing.service_name")));
        }
        if let Some(endpoint) = &t.endpoint
            && let Err(e) = check_endpoint(endpoint)
        {
            problems.push(format!("{}: {}", self.at("tracing.endpoint"), e));
        }
        if let Err(e) = self.tracing_headers() {
            problems.push(format!("{}: {}", self.at("tracing.headers"), e));
        }
        problems
    }

    /// The collector's address: `[tracing] endpoint`, or the protocol's
    /// default.
    pub fn tracing_endpoint(&self) -> String {
        self.tracing.endpoint.clone().unwrap_or_else(|| self.tracing.protocol.default_endpoint().to_owned())
    }

    /// `[tracing] headers` as names and values. Errors never quote a value.
    pub fn tracing_headers(&self) -> Result<Vec<(String, String)>, String> {
        let Some(text) = &self.tracing.headers else { return Ok(Vec::new()) };
        let mut headers = Vec::new();
        for (i, pair) in text.split(',').map(str::trim).filter(|p| !p.is_empty()).enumerate() {
            let Some((name, value)) = pair.split_once('=') else {
                return Err(format!("header {} isn't name=value", i + 1));
            };
            let name = name.trim().to_ascii_lowercase();
            if http::HeaderName::from_bytes(name.as_bytes()).is_err() {
                return Err(format!("header {}: {:?} isn't a valid header name", i + 1, name));
            }
            if http::HeaderValue::from_str(value.trim()).is_err() {
                return Err(format!("header {} ({}): the value isn't valid in a header", i + 1, name));
            }
            headers.push((name, value.trim().to_owned()));
        }
        Ok(headers)
    }

    /// Settings that are allowed but probably not meant; the server logs
    /// them as warnings at start.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.tls.enabled && self.server.plaintext_public {
            warnings.push(format!(
                "{} is set, but TLS is on, so it has no effect (remove it)",
                self.at("server.plaintext_public")
            ));
        }
        if !self.tls.enabled {
            warnings.push(format!(
                "TLS is off ({}): passwords, tokens and data cross the network in clear",
                self.at("tls.enabled")
            ));
            for (key, set) in [("tls.cert", self.tls.cert.is_some()), ("tls.key", self.tls.key.is_some())] {
                if set {
                    warnings.push(format!("{} is set, but TLS is off: ignored", self.at(key)));
                }
            }
        }
        if !self.otel_vars.is_empty() {
            warnings.push(format!(
                "{} {} set but not read: [tracing] and IWDB_TRACING_* configure traces (documentation/api/config.md#traces)",
                self.otel_vars.join(", "),
                if self.otel_vars.len() == 1 { "is" } else { "are" }
            ));
        }
        warnings
    }

    /// The effective configuration as TOML, each setting with where it
    /// came from (`iwdb-server --check-config`). It parses back to the same
    /// settings; projections are only counted.
    pub fn describe(&self) -> String {
        let mut out = String::from("# iwdb-server: the effective configuration\n");
        let mut section = "";
        for k in KEYS {
            let (head, name) = k.key.rsplit_once('.').unwrap_or(("", k.key));
            if head != section {
                out.push_str(&format!("\n[{}]\n", head));
                section = head;
            }
            let from = match self.source(k.key) {
                Source::Default => "default".to_owned(),
                Source::File => "file".to_owned(),
                Source::Env(var) => var.to_owned(),
            };
            let line = match (k.get)(self) {
                // A header's value may be a key: only the names are shown
                Some(_) if k.key == "tracing.headers" => {
                    let names: Vec<String> =
                        self.tracing_headers().unwrap_or_default().into_iter().map(|h| h.0).collect();
                    format!("# {} = (set: {}; values not shown)", name, names.join(", "))
                }
                Some(value) => format!("{} = {}", name, value),
                None => format!("# {} = ({})", name, unset(k.key)),
            };
            out.push_str(&format!("{:<44} # {}\n", line, from));
        }
        if !self.projections.is_empty() {
            out.push_str(&format!("\n# {} [[projection]] section(s) from the file\n", self.projections.len()));
        }
        out
    }

    /// The store's options: the defaults, with the fsync policy, checkpoint
    /// on shutdown, WAL retention, the archive and the backup rate from the
    /// file.
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
        options.memory = self.memory_options_with(iwdb::cgroup_limit());
        options.archive = self.store.archive.clone();
        options.backup.max_bytes_per_second = Some(self.backup.max_bytes_per_second).filter(|r| *r > 0);
        options
    }

    /// The memory limit: `[memory] limit_bytes`, or else `cgroup` (the
    /// process's cgroup limit, [`iwdb::cgroup_limit`]); 0 is none.
    pub fn memory_options_with(&self, cgroup: Option<(u64, LimitSource)>) -> MemoryOptions {
        let (limit_bytes, source) = match self.memory.limit_bytes {
            Some(0) => (None, LimitSource::Config),
            Some(bytes) => (Some(bytes), LimitSource::Config),
            None => cgroup.map_or((None, LimitSource::Config), |(bytes, source)| (Some(bytes), source)),
        };
        MemoryOptions {
            limit_bytes,
            source,
            warn_at: self.memory.warn_at,
            refuse_writes_at: self.memory.refuse_writes_at,
        }
    }

    pub fn limit_config(&self) -> LimitConfig {
        let built_in = LimitConfig::default();
        let (default_limits, default_timeout) =
            self.limits.default.over(built_in.default_limits, built_in.default_timeout);
        let (max_limits, max_timeout) = self.limits.max.over(built_in.max_limits, built_in.max_timeout);
        LimitConfig { default_limits, max_limits, default_timeout, max_timeout }
    }

    /// How the embedded database logs in (step 15a).
    pub fn auth_settings(&self) -> AuthSettings {
        AuthSettings {
            session_lifetime: Duration::from_secs(self.auth.session_lifetime_secs),
            max_failures: self.auth.login_max_failures,
            failure_window: Duration::from_secs(self.auth.login_window_secs),
            table_size: self.auth.login_table_size,
            ..AuthSettings::default()
        }
    }

    /// How the embedded database runs requests.
    pub fn query_config(&self) -> QueryConfig {
        let defaults = QueryConfig::default();
        QueryConfig {
            limits: self.limit_config(),
            workers: if self.server.workers == 0 { defaults.workers } else { self.server.workers },
            queue: self.server.queue,
            jobs: self.jobs.config(),
        }
    }

    /// The server's TLS files, if TLS is on (with its certificate and key,
    /// which the checks made sure of).
    pub fn tls_files(&self) -> Option<crate::tls::TlsFiles> {
        match (&self.tls, &self.tls.cert, &self.tls.key) {
            (tls, Some(cert), Some(key)) if tls.enabled => Some(crate::tls::TlsFiles {
                cert: cert.clone(),
                key: key.clone(),
                client_ca: tls.client_ca.clone(),
                client_auth: tls.client_auth,
            }),
            _ => None,
        }
    }

    /// Where `iwdb-server --probe` asks: the listen address (a wildcard
    /// one as loopback), and whether over TLS.
    pub fn probe_target(&self) -> (String, bool) {
        let mut address = self.listen;
        if address.ip().is_unspecified() {
            address.set_ip(match address {
                SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
            });
        }
        (address.to_string(), self.tls.enabled)
    }

    pub fn drain_timeout(&self) -> Duration {
        Duration::from_secs(self.server.drain_timeout_secs)
    }

    pub fn unready_delay(&self) -> Duration {
        Duration::from_millis(self.server.unready_delay_ms)
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

    /// `text` with TLS off: most tests are about other settings.
    fn plain(text: &str) -> Result<Config, String> {
        Config::parse(&format!("{}\n[tls]\nenabled = false\n", text))
    }

    #[test]
    fn only_the_data_directory_is_required() {
        let config = plain("data_dir = \"/tmp/x\"").unwrap();
        assert_eq!(config.listen.to_string(), DEFAULT_LISTEN);
        assert_eq!(config.limit_config(), LimitConfig::default());
        // The memory limit is the cgroup's where there is one
        assert_eq!(
            StoreOptions { memory: MemoryOptions::default(), ..config.store_options() },
            StoreOptions::default()
        );
        assert_eq!(config.memory_options_with(None), MemoryOptions::default());
        assert_eq!(config.drain_timeout(), Duration::from_secs(30));
        assert!(plain("listen = \"127.0.0.1:1\"").is_err());
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
        assert!(!config.tls.enabled);
        // The projection example, after the first block
        let second: String = doc
            .lines()
            .skip_while(|l| !l.starts_with("//! [[projection]]"))
            .take_while(|l| !l.starts_with("//! ```"))
            .map(|l| l.trim_start_matches("//!").trim_start().to_owned() + "\n")
            .collect();
        let projections = plain(&format!("data_dir = \"d\"\n{}", second));
        #[cfg(feature = "postgres")]
        {
            let projections = projections.unwrap().projections;
            assert_eq!((projections.len(), projections[0].rules.0.len(), projections[0].poll_ms), (1, 1, 500));
        }
        #[cfg(not(feature = "postgres"))]
        assert!(projections.unwrap_err().contains("without the postgres feature"));
        let minimal = plain("data_dir = \"/var/lib/iwdb\"").unwrap();
        // The example shows the defaults
        assert_eq!((&config.store, &config.server), (&minimal.store, &minimal.server));
        assert_eq!(config.limit_config(), minimal.limit_config());
        let partial =
            plain("data_dir = \"d\"\n[limits.max]\nmax_results = 5\n[limits.default]\nmax_results = 5").unwrap();
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
            let e = plain(text).unwrap_err();
            assert!(e.contains(why), "{}: {}", why, e);
        }
        let config = plain("data_dir = \"d\"\n[store]\nfsync = \"group\"\ncheckpoint_on_shutdown = false").unwrap();
        let options = config.store_options();
        assert_matches!(options.wal.fsync, FsyncPolicy::Group { max_batch: 64, .. });
        assert!(!options.checkpoint.on_close);
        let config = plain("data_dir = \"d\"\n[store]\nretain_records = 500\nretain_age_secs = 3600").unwrap();
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
        let config = plain(&section("")).unwrap();
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
            let e = plain(&text).unwrap_err();
            assert!(e.contains(why), "{}: {}", why, e);
        }
        let from_env = section("").replace("url = \"host=x\"", "url_env = \"IWDB_TEST_UNSET_VARIABLE\"");
        let e = plain(&from_env).unwrap().projections().err().unwrap();
        assert!(e.contains("IWDB_TEST_UNSET_VARIABLE"), "{}", e);
    }

    /// A server built without Postgres refuses a Postgres source when it
    /// reads its config, before it opens the store (ADR 0034).
    #[cfg(not(feature = "postgres"))]
    #[test]
    fn a_postgres_source_needs_the_postgres_feature() {
        let text = "data_dir = \"d\"\n[[projection]]\nname = \"p\"\n[projection.source]\nkind = \"postgres\"\nurl = \"host=x\"\ntable = \"t\"\nposition = \"id\"\n";
        let e = plain(text).unwrap_err();
        assert!(e.contains("[[projection]] \"p\"") && e.contains("postgres feature"), "{}", e);
        // Checked before the feature: a section that is wrong anyway says so
        let e = plain(&text.replace("url = \"host=x\"\n", "")).unwrap_err();
        assert!(e.contains("url_env"), "{}", e);
        // A config without projections is fine
        assert!(plain("data_dir = \"d\"").unwrap().projections().unwrap().is_empty());
    }

    fn env(vars: &[(&str, &str)]) -> Vec<(String, String)> {
        vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn every_setting_has_a_variable_that_overrides_the_file() {
        for k in KEYS {
            assert_eq!(k.var, format!("IWDB_{}", k.key.replace('.', "_").to_uppercase()), "{}", k.key);
        }
        let file = "data_dir = \"/from/file\"\n[store]\nfsync = \"always\"\n[server]\nqueue = 5\n";
        let config = Config::build(
            Some(file),
            &env(&[
                ("IWDB_STORE_FSYNC", "group"),
                ("IWDB_LISTEN", "0.0.0.0:9000"),
                ("IWDB_LIMITS_MAX_TIMEOUT_MS", "60000"),
                ("IWDB_LOG_FORMAT", "text"),
                ("IWDB_SERVER_PLAINTEXT_PUBLIC", "true"),
                ("IWDB_AUTH_ENABLED", "false"),
                ("IWDB_TLS_ENABLED", "0"),
                // Not settings: ignored (the test suites' and serve.py's)
                ("IWDB_SERVER", "target/debug/iwdb-server"),
                ("IWDB_URL", "http://x"),
                ("PATH", "/bin"),
            ]),
        )
        .unwrap();
        assert_eq!(config.store.fsync, Fsync::Group);
        assert_eq!(config.listen.to_string(), "0.0.0.0:9000");
        assert_eq!(config.limits.max.timeout_ms, Some(60000));
        assert_eq!(config.log.format, LogFormat::Text);
        assert!(config.server.plaintext_public && !config.auth.enabled);
        assert_eq!(config.server.queue, 5);
        assert_eq!(config.source("store.fsync"), Source::Env("IWDB_STORE_FSYNC"));
        assert_eq!(config.source("server.queue"), Source::File);
        assert_eq!(config.source("server.workers"), Source::Default);
        // Without a file
        let config = Config::build(None, &env(&[("IWDB_DATA_DIR", "rel/dir"), ("IWDB_TLS_ENABLED", "false")])).unwrap();
        assert_eq!(config.data_dir, PathBuf::from("rel/dir"));
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let problems = Config::build(
            Some("[store]\nfsync = \"group\"\ngroup_max_batch = 0\n[log]\nlevel = \"loud=?=\"\n"),
            &env(&[
                ("IWDB_STORE_FSYNC", "sometimes"),
                ("IWDB_SERVER_WORKRES", "4"),
                ("IWDB_CONSOLE_ENABLED", "yes"),
                ("IWDB_SERVER_MAX_MESSAGE_BYTES", "12"),
            ]),
        )
        .unwrap_err();
        let all = problems.join("\n");
        for expected in [
            "IWDB_STORE_FSYNC: unknown variant `sometimes`",
            "IWDB_SERVER_WORKRES: no such setting",
            "IWDB_CONSOLE_ENABLED: expected true or false",
            "data_dir is required",
            "[store] group_max_batch must be at least 1",
            "[server] max_message_bytes (from IWDB_SERVER_MAX_MESSAGE_BYTES) must be at least 1024",
            "[log] level",
            "[tls] cert and [tls] key are not set, and TLS is on",
        ] {
            assert!(all.contains(expected), "{:?} in\n{}", expected, all);
        }
        assert_eq!(problems.len(), 8, "{}", all);
        // A file that doesn't parse: its error (with the line), and the
        // variables' problems
        let problems = Config::build(Some("data_dir = 3\n"), &env(&[("IWDB_LISTEN", "x")])).unwrap_err();
        assert_eq!(problems.len(), 2, "{:?}", problems);
        assert!(problems.iter().any(|p| p.contains("line 1")), "{:?}", problems);
    }

    #[test]
    fn tracing_is_off_by_default_and_needs_its_feature() {
        let config = plain("data_dir = \"d\"\n").unwrap();
        assert!(!config.tracing.enabled);
        assert_eq!(config.tracing_endpoint(), "http://127.0.0.1:4317");
        assert_eq!((config.tracing.sample_ratio, config.tracing.service_name.as_str()), (1.0, "iwdb-server"));
        let on = plain("data_dir = \"d\"\n[tracing]\nenabled = true\n");
        if cfg!(feature = "otel") {
            assert!(on.is_ok(), "{:?}", on);
        } else {
            let e = on.unwrap_err();
            assert!(e.contains("[tracing] enabled") && e.contains("without the otel feature"), "{}", e);
        }
        // From a variable too, and the message names it
        let e =
            Config::build(Some("data_dir = \"d\"\n[tls]\nenabled = false\n"), &env(&[("IWDB_TRACING_ENABLED", "1")]));
        if cfg!(not(feature = "otel")) {
            assert!(e.unwrap_err().iter().any(|p| p.contains("(from IWDB_TRACING_ENABLED)")));
        }
    }

    #[test]
    fn tracing_settings_from_the_file_and_the_environment() {
        let text =
            "data_dir = \"d\"\n[tls]\nenabled = false\n[tracing]\nprotocol = \"http/protobuf\"\nsample_ratio = 0.25\n";
        let config = Config::build(
            Some(text),
            &env(&[
                ("IWDB_TRACING_SERVICE_NAME", "graph-1"),
                ("IWDB_TRACING_HEADERS", "Authorization=Bearer s3cret, x-team = db"),
            ]),
        )
        .unwrap();
        assert_eq!(config.tracing.protocol, OtlpProtocol::HttpProtobuf);
        assert_eq!(config.tracing_endpoint(), "http://127.0.0.1:4318/v1/traces");
        assert_eq!(config.tracing.sample_ratio, 0.25);
        assert_eq!(config.tracing.service_name, "graph-1");
        assert_eq!(
            config.tracing_headers().unwrap(),
            [("authorization".to_owned(), "Bearer s3cret".to_owned()), ("x-team".to_owned(), "db".to_owned())]
        );
        let env_protocol = Config::build(Some(text), &env(&[("IWDB_TRACING_PROTOCOL", "grpc")])).unwrap();
        assert_eq!(env_protocol.tracing.protocol, OtlpProtocol::Grpc);
        // --check-config names the headers, never their values
        let described = config.describe();
        assert!(described.contains("authorization, x-team; values not shown"), "{}", described);
        assert!(!described.contains("s3cret"), "{}", described);
        // A typo under the section is an error
        let e = Config::build(Some(text), &env(&[("IWDB_TRACING_SAMPLE", "1")])).unwrap_err();
        assert!(e.iter().any(|p| p.contains("IWDB_TRACING_SAMPLE: no such setting")), "{:?}", e);
    }

    #[test]
    fn tracing_settings_are_checked_without_quoting_secrets() {
        let base = "data_dir = \"d\"\n[tls]\nenabled = false\n";
        let problems = |vars: &[(&str, &str)]| Config::build(Some(base), &env(vars)).unwrap_err().join("\n");
        assert!(problems(&[("IWDB_TRACING_SAMPLE_RATIO", "1.5")]).contains("between 0 and 1"));
        assert!(problems(&[("IWDB_TRACING_SAMPLE_RATIO", "-0.1")]).contains("between 0 and 1"));
        assert!(problems(&[("IWDB_TRACING_PROTOCOL", "http/json")]).contains("IWDB_TRACING_PROTOCOL"));
        assert!(problems(&[("IWDB_TRACING_SERVICE_NAME", " ")]).contains("must not be empty"));
        for bad in ["collector:4317", "ftp://c", "http://"] {
            assert!(problems(&[("IWDB_TRACING_ENDPOINT", bad)]).contains("[tracing] endpoint"), "{}", bad);
        }
        let e = problems(&[("IWDB_TRACING_HEADERS", "authorization=Bearer s3cret\u{7f}")]);
        assert!(e.contains("authorization") && !e.contains("s3cret"), "{}", e);
        assert!(problems(&[("IWDB_TRACING_HEADERS", "no-equals-sign")]).contains("isn't name=value"));
    }

    #[test]
    fn otel_variables_are_not_read_but_warned_about() {
        let base = "data_dir = \"d\"\n[tls]\nenabled = false\n";
        let config = Config::build(
            Some(base),
            &env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://elsewhere:4317"), ("OTEL_SERVICE_NAME", "other")]),
        )
        .unwrap();
        assert_eq!(config.tracing_endpoint(), "http://127.0.0.1:4317");
        assert_eq!(config.tracing.service_name, "iwdb-server");
        let warnings = config.warnings();
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_SERVICE_NAME are set but not read")),
            "{:?}",
            warnings
        );
    }

    #[test]
    fn the_console_needs_its_feature() {
        let console = plain("data_dir = \"d\"\n[console]\nenabled = true\n");
        if cfg!(feature = "console") {
            assert!(console.is_ok());
        } else {
            assert!(console.unwrap_err().contains("console feature"));
        }
        // `public` is gone: TLS, or plaintext with two flags
        let e = plain("data_dir = \"d\"\n[console]\npublic = true\n").unwrap_err();
        assert!(e.contains("[tls] cert and key") && e.contains("[server] plaintext_public = true"), "{}", e);
        let e = Config::build(
            None,
            &env(&[("IWDB_DATA_DIR", "d"), ("IWDB_TLS_ENABLED", "false"), ("IWDB_CONSOLE_PUBLIC", "true")]),
        )
        .unwrap_err();
        assert!(e.join("").contains("IWDB_TLS_ENABLED=false and IWDB_SERVER_PLAINTEXT_PUBLIC=true"), "{:?}", e);
    }

    /// TLS is on by default and needs a certificate and key; plaintext is
    /// one explicit flag on a loopback address, and two elsewhere, with
    /// authentication on or off (ADRs 0047 and 0048).
    #[test]
    fn tls_is_the_default_and_plaintext_needs_explicit_flags() {
        let e = Config::parse("data_dir = \"d\"").unwrap_err();
        assert!(e.contains("[tls] cert and [tls] key are not set") && e.contains("enabled = false"), "{}", e);
        let e = Config::parse("data_dir = \"d\"\n[tls]\ncert = \"c.pem\"").unwrap_err();
        assert!(e.contains("[tls] key is not set"), "{}", e);
        let tls =
            Config::parse("data_dir = \"d\"\nlisten = \"0.0.0.0:7600\"\n[tls]\ncert = \"c\"\nkey = \"k\"").unwrap();
        assert!(tls.tls.enabled && tls.warnings().is_empty(), "{:?}", tls.warnings());
        for auth in ["true", "false"] {
            let base = format!("data_dir = \"d\"\nlisten = \"0.0.0.0:7600\"\n[auth]\nenabled = {}\n", auth);
            let e = plain(&base).unwrap_err();
            assert!(e.contains("plaintext_public = true") && e.contains("TLS is off"), "{}", e);
            let why = if auth == "true" { "passwords, tokens and data" } else { "authentication is off" };
            assert!(e.contains(why), "{}", e);
            assert!(plain(&format!("{}[server]\nplaintext_public = true\n", base)).is_ok());
            // The flag alone isn't enough: TLS is on, without a certificate
            let e = Config::parse(&format!("{}[server]\nplaintext_public = true\n", base)).unwrap_err();
            assert!(e.contains("[tls] cert and [tls] key are not set"), "{}", e);
        }
        let ok = "data_dir = \"d\"\nlisten = \"0.0.0.0:7600\"\n[server]\nplaintext_public = true\n";
        let config = plain(ok).unwrap();
        assert!(config.warnings().iter().any(|w| w.contains("in clear")), "{:?}", config.warnings());
        assert!(plain("data_dir = \"d\"\nlisten = \"[::1]:7600\"\n").is_ok());
        // The flag with TLS on: allowed, with a warning
        let both = "data_dir = \"d\"\n[server]\nplaintext_public = true\n[tls]\ncert = \"c\"\nkey = \"k\"\n";
        let warnings = Config::parse(both).unwrap().warnings();
        assert!(warnings.len() == 1 && warnings[0].contains("no effect"), "{:?}", warnings);
    }

    /// The image's configs (`docker/`): TLS with the mounted certificate;
    /// plaintext by two variables, with the certificate ignored.
    #[test]
    fn the_images_configs_speak_tls() {
        let image = include_str!("../../../docker/iwdb.toml");
        let config = Config::parse(image).unwrap();
        let files = config.tls_files().unwrap();
        assert_eq!(files.cert, PathBuf::from("/etc/iwdb/tls/server.pem"));
        assert_eq!(config.probe_target(), ("127.0.0.1:7600".to_owned(), true));
        let plaintext = [("IWDB_TLS_ENABLED", "false"), ("IWDB_SERVER_PLAINTEXT_PUBLIC", "true")];
        let config = Config::build(Some(image), &env(&plaintext)).unwrap();
        assert_eq!(config.probe_target(), ("127.0.0.1:7600".to_owned(), false));
        let e = Config::build(Some(image), &env(&plaintext[..1])).unwrap_err().join("\n");
        assert!(e.contains("IWDB_SERVER_PLAINTEXT_PUBLIC"), "{}", e);
        #[cfg(feature = "postgres")]
        assert!(Config::parse(include_str!("../../../docker/projection.example.toml")).unwrap().tls.enabled);
    }

    #[test]
    fn mtls_settings_are_checked() {
        let base = "data_dir = \"d\"\n[tls]\ncert = \"c\"\nkey = \"k\"\n";
        let config = Config::parse(base).unwrap();
        assert_eq!((config.tls.client_ca.as_ref(), config.tls.client_auth), (None, ClientAuth::Optional));
        let e = Config::parse(&format!("{}client_auth = \"required\"\n", base)).unwrap_err();
        assert!(e.contains("[tls] client_auth is required, but [tls] client_ca isn't set"), "{}", e);
        let config = Config::parse(&format!("{}client_auth = \"required\"\nclient_ca = \"ca\"\n", base)).unwrap();
        assert_eq!(config.tls.client_auth, ClientAuth::Required);
        let e = Config::parse(&format!("{}client_auth = \"sometimes\"\n", base)).unwrap_err();
        assert!(e.contains("sometimes"), "{}", e);
        let e = Config::parse("data_dir = \"d\"\n[tls]\nenabled = false\nclient_ca = \"ca\"\n").unwrap_err();
        assert!(e.contains("[tls] client_ca is set, but [tls] enabled is false"), "{}", e);
        // A certificate with TLS off (an image's config, plaintext by a
        // variable): ignored, with a warning
        let config = Config::build(Some(base), &env(&[("IWDB_TLS_ENABLED", "false")])).unwrap();
        let warnings = config.warnings().join("\n");
        assert!(warnings.contains("[tls] cert is set, but TLS is off: ignored"), "{}", warnings);
        assert!(warnings.contains("[tls] key is set") && config.tls_files().is_none(), "{}", warnings);
        let config = Config::build(
            None,
            &env(&[
                ("IWDB_DATA_DIR", "d"),
                ("IWDB_TLS_CERT", "/c"),
                ("IWDB_TLS_KEY", "/k"),
                ("IWDB_TLS_CLIENT_CA", "/ca"),
                ("IWDB_TLS_CLIENT_AUTH", "required"),
            ]),
        )
        .unwrap();
        assert_eq!(config.tls.client_ca, Some(PathBuf::from("/ca")));
        assert_eq!(config.source("tls.client_auth"), Source::Env("IWDB_TLS_CLIENT_AUTH"));
        let e = Config::build(None, &env(&[("IWDB_DATA_DIR", "d"), ("IWDB_TLS_CERTS", "/c")])).unwrap_err();
        assert!(e.join("").contains("IWDB_TLS_CERTS: no such setting"), "{:?}", e);
    }

    #[test]
    fn auth_is_on_by_default_and_checked() {
        let config = plain("data_dir = \"d\"").unwrap();
        assert!(config.auth.enabled);
        assert_eq!(config.auth_settings().session_lifetime, Duration::from_secs(43200));
        for (text, why) in [
            ("[auth]\nsession_lifetime_secs = 0", "session_lifetime_secs"),
            ("[auth]\nlogin_max_failures = 0", "login_max_failures"),
            ("[auth]\nlogin_table_size = 0", "login_table_size"),
            ("[auth]\ncolour = 1", "colour"),
        ] {
            let e = plain(&format!("data_dir = \"d\"\n{}", text)).unwrap_err();
            assert!(e.contains(why), "{}: {}", why, e);
        }
    }

    #[test]
    fn the_bootstrap_password_comes_from_the_environment_only_and_is_never_printed() {
        let config = Config::build(
            None,
            &env(&[
                ("IWDB_DATA_DIR", "d"),
                ("IWDB_TLS_ENABLED", "false"),
                ("IWDB_AUTH_BOOTSTRAP_PASSWORD", "first-admin-pw"),
            ]),
        )
        .unwrap();
        let (user, password) = config.bootstrap.clone().unwrap();
        assert_eq!((user.as_str(), password.expose()), ("admin", "first-admin-pw"));
        assert!(!config.describe().contains("first-admin-pw"));
        assert!(!format!("{:?}", config).contains("first-admin-pw"));
        let named = Config::build(
            None,
            &env(&[
                ("IWDB_DATA_DIR", "d"),
                ("IWDB_TLS_ENABLED", "false"),
                ("IWDB_AUTH_BOOTSTRAP_USER", "root"),
                ("IWDB_AUTH_BOOTSTRAP_PASSWORD", "first-admin-pw"),
            ]),
        )
        .unwrap();
        assert_eq!(named.bootstrap.unwrap().0, "root");
        let problems = Config::build(
            None,
            &env(&[
                ("IWDB_DATA_DIR", "d"),
                ("IWDB_TLS_ENABLED", "false"),
                ("IWDB_AUTH_BOOTSTRAP_USER", "_x"),
                ("IWDB_AUTH_BOOTSTRAP_PASSWORD", "short"),
            ]),
        )
        .unwrap_err()
        .join("\n");
        assert!(problems.contains("8 to 1024 bytes") && problems.contains("invalid user name"), "{}", problems);
        assert!(!problems.contains("short\""), "{}", problems);
        let empty = Config::build(
            None,
            &env(&[("IWDB_DATA_DIR", "d"), ("IWDB_TLS_ENABLED", "false"), ("IWDB_AUTH_BOOTSTRAP_PASSWORD", "")]),
        )
        .unwrap();
        assert!(empty.bootstrap.is_none(), "an empty variable is an unset one");
        let alone = Config::build(
            None,
            &env(&[("IWDB_DATA_DIR", "d"), ("IWDB_TLS_ENABLED", "false"), ("IWDB_AUTH_BOOTSTRAP_USER", "root")]),
        );
        assert!(alone.unwrap_err().join("").contains("needs IWDB_AUTH_BOOTSTRAP_PASSWORD"));
    }

    #[test]
    fn the_described_configuration_reads_back() {
        let config = Config::build(
            Some("data_dir = \"/d\"\n[limits.max]\nmax_results = 50\n[limits.default]\nmax_results = 10\n"),
            &env(&[
                ("IWDB_LOG_LEVEL", "warn,iwdb_storage=debug"),
                ("IWDB_TLS_CERT", "/tls/c.pem"),
                ("IWDB_TLS_KEY", "/tls/k.pem"),
            ]),
        )
        .unwrap();
        let text = config.describe();
        let back = Config::parse(&text).unwrap();
        assert_eq!(
            (&back.store, &back.server, &back.log, &back.console, &back.auth, &back.tls),
            (&config.store, &config.server, &config.log, &config.console, &config.auth, &config.tls)
        );
        assert_eq!(back.limit_config(), config.limit_config());
        assert!(text.contains("# max_visited = (built in)"), "{}", text);
        assert!(text.contains("# client_ca = (unset)"), "{}", text);
    }

    /// `documentation/api/config.md` lists exactly the settings, with their
    /// variables and defaults.
    #[test]
    fn the_documentation_lists_every_setting() {
        let doc = include_str!("../../../documentation/api/config.md");
        let rows: Vec<Vec<String>> = doc
            .lines()
            .filter(|l| l.starts_with("| `"))
            .map(|l| l.trim_matches('|').split(" | ").map(|c| c.trim().trim_matches('`').to_owned()).collect())
            .collect();
        let documented: Vec<(&str, &str)> = rows.iter().map(|r| (r[0].as_str(), r[1].as_str())).collect();
        let keys: Vec<(&str, &str)> = KEYS.iter().map(|k| (k.key, k.var)).collect();
        assert_eq!(documented, keys);
        let defaults = Config::default();
        for (row, k) in rows.iter().zip(KEYS) {
            let value = (k.get)(&defaults).map(|v| v.to_string()).unwrap_or_else(|| unset(k.key).into());
            let expected = if k.key == "data_dir" { "required".to_owned() } else { value };
            assert_eq!(row[2], expected, "{}", k.key);
        }
    }

    #[test]
    fn relative_paths_are_relative_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.toml");
        let text = "data_dir = \"data\"\n[tls]\ncert = \"tls/c.pem\"\nkey = \"/abs/k.pem\"\nclient_ca = \"ca.pem\"\n";
        std::fs::write(&path, text).unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.data_dir, dir.path().join("data"));
        assert_eq!(config.tls.cert, Some(dir.path().join("tls/c.pem")));
        assert_eq!(config.tls.key, Some(PathBuf::from("/abs/k.pem")));
        assert_eq!(config.tls.client_ca, Some(dir.path().join("ca.pem")));
        // From a variable: relative to the working directory
        let config = Config::from_sources(Some(&path), env(&[("IWDB_TLS_CERT", "rel.pem")])).unwrap();
        assert_eq!(config.tls.cert, Some(PathBuf::from("rel.pem")));
        assert_matches!(Config::load(&dir.path().join("missing.toml")), Err(ConfigError::Read { .. }));
    }
}
