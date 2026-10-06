//! [`Admin`] on the embedded store (step 16c, ADR 0051): the operator's
//! reads, implemented once here (design rule 8).
//!
//! The registry, the log, the request list, cancel, the readers and the
//! metrics run on the caller's thread and take no namespace lock, so they
//! answer while every worker is busy and while a commit waits for an
//! fsync: the times an operator needs them. The server status reads every
//! namespace's status (a read lock each, for an instant) on a worker.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use iwdb_engine::metrics::HistogramSnapshot;
use iwdb_query::admin::list_limit;
use iwdb_query::log::{LogTail, MAX_READ};
use iwdb_query::metrics::{self as m, Metrics, Value};
use iwdb_query::requests::{ConsumerInfo, RequestInfo, Requests};
use iwdb_query::{Admin, Code, DiskStatus, Error, Listed, MemoryStatus, NamespaceStatus, RequestCounts, ServerStatus};
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
fn metrics<F: LogFs + Clone + Send + Sync + 'static>(store: &Store<F>, monitor: &Monitor) -> Metrics
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
    for (part, bytes) in [
        ("graph", memory.graph),
        ("payload", memory.payload),
        ("checkpoint", memory.checkpoint),
        ("working", memory.working),
    ] {
        out.add(m::MEMORY_USED, &[part], Value::Gauge(bytes as f64));
    }
    if let Some(limit) = memory.limit {
        out.add(m::MEMORY_LIMIT, &[], Value::Gauge(limit.bytes as f64));
        out.add(m::MEMORY_WARN, &[], Value::Gauge(limit.warn as f64));
        out.add(m::MEMORY_REFUSE_WRITES, &[], Value::Gauge(limit.refuse_writes as f64));
    }
    out.add(m::MEMORY_STATE, &[], Value::Gauge(memory.state as u8 as f64));
    out
}

impl<F: LogFs + Clone + Send + Sync + 'static> Admin for Embedded<F>
where
    F::File: Send,
{
    fn server_status(&self) -> impl Future<Output = Result<ServerStatus, Error>> + Send {
        let monitor = self.monitor.clone();
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
        std::future::ready(Ok(metrics(self.store(), &self.monitor)))
    }

    fn log(&self, after: u64, limit: Option<usize>) -> impl Future<Output = Result<LogTail, Error>> + Send {
        let limit = limit.unwrap_or(MAX_READ).min(MAX_READ);
        std::future::ready(Ok(self.monitor.log.read(after, limit)))
    }

    fn set_ready(&self, ready: bool) {
        self.monitor.ready.store(ready, Ordering::Release);
    }

    fn registry(&self) -> Option<Arc<Requests>> {
        Some(self.monitor.requests.clone())
    }
}
