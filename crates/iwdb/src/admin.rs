//! [`Admin`] on the embedded store (step 16c, ADR 0051): the operator's
//! reads, and the admin writes of step 16e (ADR 0055), implemented once
//! here (design rule 8).
//!
//! Managed analytics jobs (step 16f, ADR 0056) run on the job registry's
//! own threads (`iwdb_query::jobs`); their methods here only queue, list,
//! cancel and page, on the caller's thread.
//!
//! The admin writes (checkpoint, backup, verify, pruning the archive) run
//! on threads of their own, not on the workers: a throttled backup, or a
//! checkpoint waiting for one, can take hours. Dropping their future
//! doesn't stop them.
//!
//! The registry, the log, the request list, cancel, the readers and the
//! metrics run on the caller's thread and take no namespace lock, so they
//! answer while every worker is busy and while a commit waits for an
//! fsync: the times an operator needs them. The server status reads every
//! namespace's status (a read lock each, for an instant) on a worker.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use iwdb_engine::metrics::HistogramSnapshot;
use iwdb_query::admin::{PruneReport, VerifyReport, check_backup_name, list_limit};
use iwdb_query::exec::{Pending, spawn};
use iwdb_query::jobs::Jobs;
use iwdb_query::log::{LogTail, MAX_READ};
use iwdb_query::metrics::{self as m, Metrics, Value};
use iwdb_query::requests::{ConsumerInfo, RequestInfo, Requests};
use iwdb_query::{Admin, Code, DiskStatus, Error, Listed, MemoryStatus, NamespaceStatus, RequestCounts, ServerStatus};
use iwdb_query::{AnalyticsRequest, JobInfo, JobOwner, JobPage, QueryOptions};
use iwdb_query::{BackupDone, BackupRequest, Checkpointed, MemoryState, VerifyTarget};
use iwdb_storage::FsyncPolicy;
use iwdb_storage::io::LogFs;

use crate::embedded::Monitor;
use crate::{Embedded, Store};

/// The server's version: the crates'.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn fsync_name(policy: FsyncPolicy) -> &'static str {
    match policy {
        FsyncPolicy::Always => "always",
        FsyncPolicy::Group { .. } => "group",
        FsyncPolicy::Off => "off",
    }
}

fn seconds(micros: i64) -> f64 {
    micros as f64 / 1e6
}

/// The request counts of the registry.
fn counts(requests: &Requests) -> RequestCounts {
    let mut counts = RequestCounts { active: requests.active() as u64, ..RequestCounts::default() };
    for op in requests.stats() {
        for (code, n) in op.outcomes {
            counts.total += n;
            match code {
                Some(Code::Timeout) => counts.timed_out += n,
                Some(Code::Cancelled) => counts.cancelled += n,
                Some(Code::Unavailable) => counts.rejected += n,
                Some(Code::Unauthenticated | Code::PermissionDenied) => counts.denied += n,
                _ => {}
            }
        }
    }
    counts
}

/// Every metric, now (see [`iwdb_query::metrics::METRICS`]).
fn metrics<F: LogFs + Clone + Send + Sync + 'static>(store: &Store<F>, monitor: &Monitor, jobs: &Jobs) -> Metrics
where
    F::File: Send,
{
    let mut out = Metrics::new();
    out.add(m::BUILD_INFO, &[VERSION], Value::Gauge(1.0));
    out.add(m::START_TIME, &[], Value::Gauge(seconds(monitor.started.micros())));
    out.add(m::READY, &[], Value::Gauge(if monitor.ready.load(Ordering::Acquire) { 1.0 } else { 0.0 }));
    out.add(m::REQUESTS_ACTIVE, &[], Value::Gauge(monitor.requests.active() as f64));
    for op in monitor.requests.stats() {
        for (code, n) in &op.outcomes {
            let code = code.map_or("ok", Code::as_str);
            out.add(m::REQUESTS, &[op.operation.name(), code], Value::Counter(*n));
        }
        if op.durations.count() > 0 {
            out.add(m::REQUEST_DURATION, &[op.operation.name()], Value::Histogram(op.durations));
        }
    }
    let (mut commits, mut fsyncs, mut checkpoints) = Default::default();
    let (mut reads, mut writes): (HistogramSnapshot, HistogramSnapshot) = Default::default();
    let merge = |into: &mut HistogramSnapshot, h: &HistogramSnapshot| into.merge(h);
    for ns in store.open_namespaces() {
        let name = ns.name();
        let h = ns.histograms();
        merge(&mut commits, &h.live.commits);
        merge(&mut fsyncs, &h.live.fsyncs);
        merge(&mut checkpoints, &h.checkpoints);
        merge(&mut reads, &h.live.read_holds);
        merge(&mut writes, &h.live.write_holds);
        let sizes = ns.sizes();
        let disk = ns.disk_usage();
        out.add(m::NAMESPACE_NODES, &[name], Value::Gauge(sizes.nodes as f64));
        out.add(m::NAMESPACE_EDGES, &[name], Value::Gauge(sizes.edges as f64));
        out.add(m::NAMESPACE_MEMORY, &[name], Value::Gauge(sizes.memory_bytes as f64));
        out.add(m::WAL_BYTES, &[name], Value::Gauge(disk.wal_bytes as f64));
        out.add(m::CHECKPOINT_BYTES, &[name], Value::Gauge(disk.checkpoint_bytes as f64));
        let lag = ns.seq().saturating_sub(ns.checkpoint_seq().unwrap_or(0));
        out.add(m::CHECKPOINT_LAG, &[name], Value::Gauge(lag as f64));
        if let Some(t) = ns.checkpoint_seq().and(ns.last_checkpoint()) {
            out.add(m::LAST_CHECKPOINT, &[name], Value::Gauge(seconds(t.micros())));
        }
        if let Some(n) = ns.unsynced() {
            out.add(m::UNSYNCED, &[name], Value::Gauge(n as f64));
        }
        let flag = |on: bool| Value::Gauge(if on { 1.0 } else { 0.0 });
        out.add(m::READ_ONLY, &[name], flag(ns.read_only().is_some()));
        out.add(m::CHECKPOINT_FAILED, &[name], flag(ns.checkpoint_failure().is_some()));
    }
    out.add(m::COMMIT_DURATION, &[], Value::Histogram(commits));
    out.add(m::WAL_FSYNC_DURATION, &[], Value::Histogram(fsyncs));
    out.add(m::CHECKPOINT_DURATION, &[], Value::Histogram(checkpoints));
    out.add(m::LOCK_HOLD, &["read"], Value::Histogram(reads));
    out.add(m::LOCK_HOLD, &["write"], Value::Histogram(writes));
    if let Some(free) = store.disk_free() {
        out.add(m::DISK_FREE, &[], Value::Gauge(free as f64));
    }
    let memory = store.memory();
    for (part, bytes) in [("graph", memory.graph), ("checkpoint", memory.checkpoint), ("working", memory.working)] {
        out.add(m::MEMORY_USED, &[part], Value::Gauge(bytes as f64));
    }
    if let Some(limit) = memory.limit {
        out.add(m::MEMORY_LIMIT, &[], Value::Gauge(limit.bytes as f64));
        out.add(m::MEMORY_WARN, &[], Value::Gauge(limit.warn as f64));
        out.add(m::MEMORY_REFUSE_WRITES, &[], Value::Gauge(limit.refuse_writes as f64));
    }
    out.add(m::MEMORY_STATE, &[], Value::Gauge(memory.state as u8 as f64));
    let backups = store.backup_stats();
    out.add(m::BACKUP_RUNNING, &[], Value::Gauge(backups.running as f64));
    out.add(m::BACKUP_BYTES, &[], Value::Counter(backups.bytes));
    out.add(m::BACKUPS, &["ok"], Value::Counter(backups.ok));
    out.add(m::BACKUPS, &["failed"], Value::Counter(backups.failed));
    if let Some(t) = backups.last {
        out.add(m::LAST_BACKUP, &[], Value::Gauge(seconds(t.micros())));
    }
    let jobs = jobs.counts();
    out.add(m::JOBS_QUEUED, &[], Value::Gauge(jobs.queued as f64));
    out.add(m::JOBS_RUNNING, &[], Value::Gauge(jobs.running as f64));
    out.add(m::JOBS, &["done"], Value::Counter(jobs.done_total));
    out.add(m::JOBS, &["failed"], Value::Counter(jobs.failed_total));
    out.add(m::JOBS, &["cancelled"], Value::Counter(jobs.cancelled_total));
    out.add(m::JOB_RESULT_BYTES, &[], Value::Gauge(jobs.result_bytes as f64));
    let (exported, queue_full, export_failed) = monitor.spans.read();
    out.add(m::TRACE_SPANS_EXPORTED, &[], Value::Counter(exported));
    out.add(m::TRACE_SPANS_DROPPED, &["queue_full"], Value::Counter(queue_full));
    out.add(m::TRACE_SPANS_DROPPED, &["export_failed"], Value::Counter(export_failed));
    out
}

impl<F: LogFs + Clone + Send + Sync + 'static> Admin for Embedded<F>
where
    F::File: Send,
{
    fn server_status(&self) -> impl Future<Output = Result<ServerStatus, Error>> + Send {
        let (monitor, jobs) = (self.monitor.clone(), self.jobs.clone());
        self.run(move |store, _| {
            let namespaces: Vec<NamespaceStatus> = store.open_namespaces().iter().map(|ns| ns.status()).collect();
            let mut disk = DiskStatus { free_bytes: store.disk_free(), ..DiskStatus::default() };
            for ns in store.open_namespaces() {
                let usage = ns.disk_usage();
                disk.wal_bytes += usage.wal_bytes;
                disk.checkpoint_bytes += usage.checkpoint_bytes;
            }
            Ok(ServerStatus {
                version: VERSION.to_owned(),
                started: monitor.started,
                ready: monitor.ready.load(Ordering::Acquire),
                fsync: fsync_name(store.fsync_policy()).to_owned(),
                memory: MemoryStatus::of(&store.memory()),
                disk,
                requests: counts(&monitor.requests),
                jobs: jobs.counts(),
                namespaces,
            })
        })
    }

    fn active_requests(
        &self,
        user: Option<String>,
        limit: Option<usize>,
    ) -> impl Future<Output = Result<Listed<RequestInfo>, Error>> + Send {
        let (items, truncated) = self.monitor.requests.list(user.as_deref(), list_limit(limit));
        std::future::ready(Ok(Listed { items, truncated }))
    }

    fn cancel_request(&self, id: u64, user: Option<String>) -> impl Future<Output = Result<RequestInfo, Error>> + Send {
        std::future::ready(self.monitor.requests.cancel(id, user.as_deref()))
    }

    fn consumers(&self) -> impl Future<Output = Result<Vec<ConsumerInfo>, Error>> + Send {
        let list = self
            .monitor
            .requests
            .consumers()
            .into_iter()
            .filter_map(|mut c| {
                // A reader of a namespace dropped since is gone too
                let streamable = self.store().namespace(&c.namespace).ok()?.streamable_seq();
                c.lag = streamable.saturating_sub(c.next_seq.saturating_sub(1));
                Some(c)
            })
            .collect();
        std::future::ready(Ok(list))
    }

    fn metrics(&self) -> impl Future<Output = Result<Metrics, Error>> + Send {
        std::future::ready(Ok(metrics(self.store(), &self.monitor, &self.jobs)))
    }

    fn log(&self, after: u64, limit: Option<usize>) -> impl Future<Output = Result<LogTail, Error>> + Send {
        let limit = limit.unwrap_or(MAX_READ).min(MAX_READ);
        std::future::ready(Ok(self.monitor.log.read(after, limit)))
    }

    fn checkpoint(&self, namespace: Option<String>) -> impl Future<Output = Result<Vec<Checkpointed>, Error>> + Send {
        let store = self.shared_store();
        spawn("iwdb-admin-checkpoint", move || {
            let outcomes = match namespace {
                Some(name) => vec![(name.clone(), store.namespace(&name)?.checkpoint()?)],
                None => store.checkpoint_all()?,
            };
            Ok(outcomes.into_iter().map(|(namespace, outcome)| Checkpointed { namespace, outcome }).collect())
        })
    }

    fn backup(&self, request: BackupRequest) -> impl Future<Output = Result<BackupDone, Error>> + Send {
        let dest = match self.backup_target(&request.name) {
            Ok(dest) => dest,
            Err(e) => return Pending::ready(Err(e)),
        };
        let store = self.shared_store();
        spawn("iwdb-admin-backup", move || {
            create_backup_dir(&dest)?;
            let written = match request.max_bytes_per_second {
                Some(rate) => store.backup_with(&dest, Some(rate)),
                None => store.backup(&dest),
            };
            let report = match written {
                Ok(report) => report,
                Err(e) => {
                    // The directory is ours, and a server's operator may have
                    // no shell to remove it
                    if let Err(removed) = std::fs::remove_dir_all(&dest) {
                        log::warn!("can't remove the failed backup '{}': {}", dest.display(), removed);
                    }
                    return Err(e.into());
                }
            };
            let verify = if request.verify { Some(crate::verify(&dest)?) } else { None };
            Ok(BackupDone { report, verify })
        })
    }

    fn verify(&self, target: VerifyTarget) -> impl Future<Output = Result<VerifyReport, Error>> + Send {
        let memory = self.store().memory();
        if memory.state == MemoryState::RefusingWrites {
            let limit = memory.limit.map_or(0, |l| l.refuse_writes);
            return Pending::ready(Err(Error::new(
                Code::ResourceExhausted,
                format!(
                    "memory limit: verify is refused above {} bytes ({} in use), since it replays each namespace into memory the limit doesn't count; checkpoints and backups are accepted",
                    limit,
                    memory.used()
                ),
            )));
        }
        let path = match &target {
            VerifyTarget::Store => None,
            VerifyTarget::Backup(name) => match self.existing_backup(name) {
                Ok(path) => Some(path),
                Err(e) => return Pending::ready(Err(e)),
            },
            VerifyTarget::Archive => match self.archive() {
                Ok(path) => Some(path),
                Err(e) => return Pending::ready(Err(e)),
            },
        };
        let store = self.shared_store();
        spawn("iwdb-admin-verify", move || match path {
            None => Ok(store.verify()?),
            Some(path) => Ok(crate::verify(&path)?),
        })
    }

    fn prune_archive(&self, before: String, dry_run: bool) -> impl Future<Output = Result<PruneReport, Error>> + Send {
        let paths = self.archive().and_then(|archive| Ok((archive, self.existing_backup(&before)?)));
        let (archive, backup) = match paths {
            Ok(paths) => paths,
            Err(e) => return Pending::ready(Err(e)),
        };
        spawn("iwdb-admin-prune", move || Ok(crate::prune_archive(&archive, &backup, dry_run)?))
    }

    fn start_job(
        &self,
        namespace: String,
        request: AnalyticsRequest,
        options: QueryOptions,
        owner: Option<JobOwner>,
    ) -> impl Future<Output = Result<JobInfo, Error>> + Send {
        std::future::ready(self.queue_job(namespace, request, options, owner))
    }

    fn jobs(
        &self,
        user: Option<String>,
        limit: Option<usize>,
    ) -> impl Future<Output = Result<Listed<JobInfo>, Error>> + Send {
        let (items, truncated) = self.jobs.list(user.as_deref(), list_limit(limit));
        std::future::ready(Ok(Listed { items, truncated }))
    }

    fn job(&self, id: u64, user: Option<String>) -> impl Future<Output = Result<JobInfo, Error>> + Send {
        std::future::ready(self.jobs.get(id, user.as_deref()))
    }

    fn cancel_job(&self, id: u64, user: Option<String>) -> impl Future<Output = Result<JobInfo, Error>> + Send {
        std::future::ready(self.jobs.cancel(id, user.as_deref()))
    }

    fn job_result(
        &self,
        id: u64,
        user: Option<String>,
        offset: u64,
        limit: Option<usize>,
    ) -> impl Future<Output = Result<JobPage, Error>> + Send {
        std::future::ready(self.jobs.page(id, user.as_deref(), offset, limit))
    }

    /// Not ready (the server drains): the jobs are cancelled and new ones
    /// refused (ADR 0056).
    fn set_ready(&self, ready: bool) {
        self.monitor.ready.store(ready, Ordering::Release);
        if ready {
            self.jobs.resume();
        } else {
            self.jobs.drain();
        }
    }

    fn registry(&self) -> Option<Arc<Requests>> {
        Some(self.monitor.requests.clone())
    }
}

impl<F: LogFs + Clone + Send + Sync + 'static> Embedded<F>
where
    F::File: Send,
{
    /// The backup directory; `invalid_argument` without one.
    fn backup_root(&self) -> Result<&std::path::Path, Error> {
        self.backup_dir().ok_or_else(|| {
            Error::invalid("the server has no backup directory: set [backup] dir (backups are written only there)")
        })
    }

    /// Where a new backup `name` goes: `<backup dir>/<name>`, checked.
    fn backup_target(&self, name: &str) -> Result<PathBuf, Error> {
        check_backup_name(name)?;
        Ok(self.backup_root()?.join(name))
    }

    /// The existing backup `name` in the backup directory: a directory, not
    /// a symlink. Errors: `invalid_argument`, `not_found`.
    fn existing_backup(&self, name: &str) -> Result<PathBuf, Error> {
        let path = self.backup_target(name)?;
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                Err(Error::invalid(format!("'{}' in the backup directory is a symbolic link", name)))
            }
            Ok(m) if m.is_dir() => Ok(path),
            Ok(_) => Err(Error::invalid(format!("'{}' in the backup directory is not a backup", name))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(Error::new(Code::NotFound, format!("no backup '{}' in the backup directory", name)))
            }
            Err(e) => Err(Error::new(Code::Io, format!("can't read the backup '{}': {}", name, e))),
        }
    }

    /// The store's WAL archive; `invalid_argument` without one.
    fn archive(&self) -> Result<PathBuf, Error> {
        self.store()
            .archive_dir()
            .map(std::path::Path::to_path_buf)
            .ok_or_else(|| Error::invalid("the store has no WAL archive: set [store] archive"))
    }
}

/// Create the directory of a new backup, refusing anything that is there
/// already (a file, a directory, a symlink: `create_dir` doesn't follow
/// one in the last component), and make its entry durable.
fn create_backup_dir(dest: &std::path::Path) -> Result<(), Error> {
    match std::fs::create_dir(dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let name = dest.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            return Err(Error::new(
                Code::Conflict,
                format!("'{}' exists in the backup directory: a backup never overwrites anything", name),
            ));
        }
        Err(e) => return Err(Error::new(Code::Io, format!("can't create '{}': {}", dest.display(), e))),
    }
    if let Some(parent) = dest.parent() {
        #[cfg(unix)]
        std::fs::File::open(parent)
            .and_then(|d| d.sync_all())
            .map_err(|e| Error::new(Code::Io, format!("can't sync '{}': {}", parent.display(), e)))?;
        #[cfg(not(unix))]
        let _ = parent;
    }
    Ok(())
}
