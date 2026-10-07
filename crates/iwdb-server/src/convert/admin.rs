//! `admin.proto` (step 16c): the server's status, requests, readers,
//! metrics and log, the admin writes (step 16e) and the managed jobs (step
//! 16f), both ways.

use std::net::IpAddr;
use std::time::Duration;

use iwdb_engine::metrics::{BUCKETS, BUCKETS_MICROS, HistogramSnapshot};
use iwdb_query::admin::{
    BackupReport, CheckpointOutcome, Finding, Kind as VerifyKind, NamespaceBackup, NamespacePrune, NamespaceVerify,
    PruneReport, VerifyReport,
};
use iwdb_query::auth::Operation;
use iwdb_query::log::{Level, LogEvent, LogTail};
use iwdb_query::metrics::{Family, Kind, Metrics, Sample, Value};
use iwdb_query::requests::{ConsumerInfo, RequestInfo};
use iwdb_query::{
    AnalyticsRequest, BackupDone, Checkpointed, Code, DiskStatus, Error, JobCounts, JobInfo, JobPage, JobProgress,
    JobState, LimitSource, Listed, MemoryState, MemoryStatus, QueryOptions, RequestCounts, ServerStatus, VerifyTarget,
};
use iwdb_storage::HistoryId;

use super::{
    JobRows, analyze_from_pb, analyze_to_pb, job_result_from_pb, job_result_to_pb, missing, options_from_pb,
    status_from_pb, status_to_pb, time_from_pb,
};
use crate::proto as pb;

fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn client_from_pb(client: Option<String>) -> Result<Option<IpAddr>, Error> {
    client.map(|c| c.parse().map_err(|_| Error::invalid(format!("invalid client address '{}'", c)))).transpose()
}

pub(crate) fn server_status_to_pb(s: &ServerStatus) -> pb::ServerStatus {
    let r = &s.requests;
    pb::ServerStatus {
        version: s.version.clone(),
        started_micros: s.started.0,
        ready: s.ready,
        fsync: s.fsync.clone(),
        memory: Some(memory_to_pb(&s.memory)),
        disk: Some(pb::DiskStatus {
            wal_bytes: s.disk.wal_bytes,
            checkpoint_bytes: s.disk.checkpoint_bytes,
            free_bytes: s.disk.free_bytes,
        }),
        requests: Some(pb::RequestCounts {
            active: r.active,
            total: r.total,
            timed_out: r.timed_out,
            cancelled: r.cancelled,
            rejected: r.rejected,
            denied: r.denied,
        }),
        namespaces: s.namespaces.iter().map(status_to_pb).collect(),
        jobs: Some(job_counts_to_pb(&s.jobs)),
    }
}

fn job_counts_to_pb(c: &JobCounts) -> pb::JobCounts {
    pb::JobCounts {
        queued: c.queued,
        running: c.running,
        finished: c.finished,
        result_bytes: c.result_bytes,
        done_total: c.done_total,
        failed_total: c.failed_total,
        cancelled_total: c.cancelled_total,
    }
}

fn job_counts_from_pb(c: pb::JobCounts) -> JobCounts {
    JobCounts {
        queued: c.queued,
        running: c.running,
        finished: c.finished,
        result_bytes: c.result_bytes,
        done_total: c.done_total,
        failed_total: c.failed_total,
        cancelled_total: c.cancelled_total,
    }
}

fn memory_to_pb(m: &MemoryStatus) -> pb::MemoryStatus {
    let state = match m.state {
        MemoryState::Normal => pb::MemoryState::Normal,
        MemoryState::Warn => pb::MemoryState::Warn,
        MemoryState::RefusingWrites => pb::MemoryState::RefusingWrites,
    };
    let source = match m.limit_source {
        None => pb::MemoryLimitSource::Unspecified,
        Some(LimitSource::Config) => pb::MemoryLimitSource::Config,
        Some(LimitSource::CgroupV2) => pb::MemoryLimitSource::CgroupV2,
        Some(LimitSource::CgroupV1) => pb::MemoryLimitSource::CgroupV1,
    };
    pb::MemoryStatus {
        graph_bytes: m.graph_bytes,
        limit_bytes: m.limit_bytes,
        checkpoint_bytes: m.checkpoint_bytes,
        working_bytes: m.working_bytes,
        used_bytes: m.used_bytes,
        warn_bytes: m.warn_bytes,
        refuse_writes_bytes: m.refuse_writes_bytes,
        state: state.into(),
        limit_source: source.into(),
    }
}

fn memory_from_pb(m: pb::MemoryStatus) -> MemoryStatus {
    // An unknown state (a newer server) reads as the nearest known one
    let state = match pb::MemoryState::try_from(m.state) {
        Ok(pb::MemoryState::Warn) => MemoryState::Warn,
        Ok(pb::MemoryState::RefusingWrites) => MemoryState::RefusingWrites,
        _ => MemoryState::Normal,
    };
    let limit_source = match pb::MemoryLimitSource::try_from(m.limit_source) {
        Ok(pb::MemoryLimitSource::Config) => Some(LimitSource::Config),
        Ok(pb::MemoryLimitSource::CgroupV2) => Some(LimitSource::CgroupV2),
        Ok(pb::MemoryLimitSource::CgroupV1) => Some(LimitSource::CgroupV1),
        _ => None,
    };
    MemoryStatus {
        graph_bytes: m.graph_bytes,
        checkpoint_bytes: m.checkpoint_bytes,
        working_bytes: m.working_bytes,
        used_bytes: m.used_bytes,
        limit_bytes: m.limit_bytes,
        warn_bytes: m.warn_bytes,
        refuse_writes_bytes: m.refuse_writes_bytes,
        state,
        limit_source,
    }
}

pub(crate) fn server_status_from_pb(s: Option<pb::ServerStatus>) -> Result<ServerStatus, Error> {
    let s = s.ok_or_else(|| missing("the server status"))?;
    let memory = s.memory.unwrap_or_default();
    let disk = s.disk.unwrap_or_default();
    let r = s.requests.unwrap_or_default();
    Ok(ServerStatus {
        version: s.version,
        started: time_from_pb(s.started_micros),
        ready: s.ready,
        fsync: s.fsync,
        memory: memory_from_pb(memory),
        disk: DiskStatus {
            wal_bytes: disk.wal_bytes,
            checkpoint_bytes: disk.checkpoint_bytes,
            free_bytes: disk.free_bytes,
        },
        requests: RequestCounts {
            active: r.active,
            total: r.total,
            timed_out: r.timed_out,
            cancelled: r.cancelled,
            rejected: r.rejected,
            denied: r.denied,
        },
        namespaces: s.namespaces.into_iter().map(|n| status_from_pb(Some(n))).collect::<Result<_, _>>()?,
        jobs: job_counts_from_pb(s.jobs.unwrap_or_default()),
    })
}

pub(crate) fn request_to_pb(r: &RequestInfo) -> pb::RequestInfo {
    pb::RequestInfo {
        id: r.id,
        operation: r.operation.name().to_owned(),
        namespace: r.namespace.clone(),
        user: r.user.clone(),
        client: r.client.map(|c| c.to_string()),
        started_micros: r.started.0,
        elapsed_micros: micros(r.elapsed),
        cancellable: r.cancellable,
    }
}

pub(crate) fn request_from_pb(r: Option<pb::RequestInfo>) -> Result<RequestInfo, Error> {
    let r = r.ok_or_else(|| missing("the request"))?;
    let operation = Operation::from_rpc(&r.operation)
        .ok_or_else(|| Error::invalid(format!("unknown operation '{}'", r.operation)))?;
    Ok(RequestInfo {
        id: r.id,
        operation,
        namespace: r.namespace,
        user: r.user,
        client: client_from_pb(r.client)?,
        started: time_from_pb(r.started_micros),
        elapsed: Duration::from_micros(r.elapsed_micros),
        cancellable: r.cancellable,
    })
}

pub(crate) fn requests_to_pb(list: &Listed<RequestInfo>) -> pb::ListRequestsResponse {
    pb::ListRequestsResponse { requests: list.items.iter().map(request_to_pb).collect(), truncated: list.truncated }
}

pub(crate) fn requests_from_pb(r: pb::ListRequestsResponse) -> Result<Listed<RequestInfo>, Error> {
    let items = r.requests.into_iter().map(|r| request_from_pb(Some(r))).collect::<Result<_, _>>()?;
    Ok(Listed { items, truncated: r.truncated })
}

pub(crate) fn consumer_to_pb(c: &ConsumerInfo) -> pb::ConsumerInfo {
    pb::ConsumerInfo {
        namespace: c.namespace.clone(),
        user: c.user.clone(),
        client: c.client.map(|c| c.to_string()),
        next_seq: c.next_seq,
        lag: c.lag,
        last_poll_micros: c.last_poll.0,
        polls: c.polls,
    }
}

pub(crate) fn consumer_from_pb(c: pb::ConsumerInfo) -> Result<ConsumerInfo, Error> {
    Ok(ConsumerInfo {
        namespace: c.namespace,
        user: c.user,
        client: client_from_pb(c.client)?,
        next_seq: c.next_seq,
        lag: c.lag,
        last_poll: time_from_pb(c.last_poll_micros),
        polls: c.polls,
    })
}

fn kind_to_pb(kind: Kind) -> pb::MetricKind {
    match kind {
        Kind::Counter => pb::MetricKind::Counter,
        Kind::Gauge => pb::MetricKind::Gauge,
        Kind::Histogram => pb::MetricKind::Histogram,
    }
}

fn kind_from_pb(kind: i32) -> Result<Kind, Error> {
    match pb::MetricKind::try_from(kind) {
        Ok(pb::MetricKind::Counter) => Ok(Kind::Counter),
        Ok(pb::MetricKind::Gauge) => Ok(Kind::Gauge),
        Ok(pb::MetricKind::Histogram) => Ok(Kind::Histogram),
        _ => Err(Error::invalid(format!("invalid metric kind {}", kind))),
    }
}

fn histogram_to_pb(h: &HistogramSnapshot) -> pb::MetricHistogram {
    pb::MetricHistogram {
        bounds_seconds: BUCKETS_MICROS.iter().map(|&b| b as f64 / 1e6).collect(),
        counts: h.buckets.to_vec(),
        sum_micros: h.sum_micros,
    }
}

fn histogram_from_pb(h: pb::MetricHistogram) -> Result<HistogramSnapshot, Error> {
    let bounds: Vec<f64> = BUCKETS_MICROS.iter().map(|&b| b as f64 / 1e6).collect();
    let buckets: [u64; BUCKETS] =
        h.counts.try_into().map_err(|_| Error::invalid("a histogram has the wrong buckets"))?;
    if h.bounds_seconds != bounds {
        return Err(Error::invalid("a histogram has other buckets than this client's"));
    }
    Ok(HistogramSnapshot { buckets, sum_micros: h.sum_micros })
}

pub(crate) fn metrics_to_pb(m: &Metrics) -> pb::GetMetricsResponse {
    let families = m
        .families
        .iter()
        .map(|f| pb::MetricFamily {
            name: f.name.clone(),
            kind: kind_to_pb(f.kind) as i32,
            help: f.help.clone(),
            samples: f
                .samples
                .iter()
                .map(|s| pb::MetricSample {
                    labels: s
                        .labels
                        .iter()
                        .map(|(name, value)| pb::MetricLabel { name: name.clone(), value: value.clone() })
                        .collect(),
                    value: Some(match &s.value {
                        Value::Counter(n) => pb::metric_sample::Value::Counter(*n),
                        Value::Gauge(x) => pb::metric_sample::Value::Gauge(*x),
                        Value::Histogram(h) => pb::metric_sample::Value::Histogram(histogram_to_pb(h)),
                    }),
                })
                .collect(),
        })
        .collect();
    pb::GetMetricsResponse { families }
}

pub(crate) fn metrics_from_pb(r: pb::GetMetricsResponse) -> Result<Metrics, Error> {
    let families = r
        .families
        .into_iter()
        .map(|f| {
            let samples = f
                .samples
                .into_iter()
                .map(|s| {
                    let value = match s.value.ok_or_else(|| missing("a metric sample's value"))? {
                        pb::metric_sample::Value::Counter(n) => Value::Counter(n),
                        pb::metric_sample::Value::Gauge(x) => Value::Gauge(x),
                        pb::metric_sample::Value::Histogram(h) => Value::Histogram(histogram_from_pb(h)?),
                    };
                    Ok(Sample { labels: s.labels.into_iter().map(|l| (l.name, l.value)).collect(), value })
                })
                .collect::<Result<_, Error>>()?;
            Ok(Family { name: f.name, kind: kind_from_pb(f.kind)?, help: f.help, samples })
        })
        .collect::<Result<_, Error>>()?;
    Ok(Metrics { families })
}

fn level_to_pb(level: Level) -> pb::LogLevel {
    match level {
        Level::Trace => pb::LogLevel::Trace,
        Level::Debug => pb::LogLevel::Debug,
        Level::Info => pb::LogLevel::Info,
        Level::Warn => pb::LogLevel::Warn,
        Level::Error => pb::LogLevel::Error,
    }
}

fn level_from_pb(level: i32) -> Result<Level, Error> {
    match pb::LogLevel::try_from(level) {
        Ok(pb::LogLevel::Trace) => Ok(Level::Trace),
        Ok(pb::LogLevel::Debug) => Ok(Level::Debug),
        Ok(pb::LogLevel::Info) => Ok(Level::Info),
        Ok(pb::LogLevel::Warn) => Ok(Level::Warn),
        Ok(pb::LogLevel::Error) => Ok(Level::Error),
        _ => Err(Error::invalid(format!("invalid log level {}", level))),
    }
}

pub(crate) fn log_to_pb(t: &LogTail) -> pb::GetLogResponse {
    pb::GetLogResponse {
        events: t
            .events
            .iter()
            .map(|e| pb::LogEvent {
                seq: e.seq,
                time_micros: e.time.0,
                level: level_to_pb(e.level) as i32,
                target: e.target.clone(),
                message: e.message.clone(),
                fields: e
                    .fields
                    .iter()
                    .map(|(name, value)| pb::LogField { name: name.clone(), value: value.clone() })
                    .collect(),
            })
            .collect(),
        last_seq: t.last_seq,
        missed: t.missed,
    }
}

pub(crate) fn log_from_pb(r: pb::GetLogResponse) -> Result<LogTail, Error> {
    let events = r
        .events
        .into_iter()
        .map(|e| {
            Ok(LogEvent {
                seq: e.seq,
                time: time_from_pb(e.time_micros),
                level: level_from_pb(e.level)?,
                target: e.target,
                message: e.message,
                fields: e.fields.into_iter().map(|f| (f.name, f.value)).collect(),
            })
        })
        .collect::<Result<_, Error>>()?;
    Ok(LogTail { events, last_seq: r.last_seq, missed: r.missed })
}

// ---- the admin writes (step 16e, ADR 0055) ----

fn count(n: usize) -> u64 {
    n as u64
}

fn uncount(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

fn history_from_pb(text: &str) -> Result<HistoryId, Error> {
    text.parse().map_err(|_| Error::invalid(format!("invalid history id '{}'", text)))
}

pub(crate) fn checkpoints_to_pb(list: &[Checkpointed]) -> pb::CheckpointResponse {
    pb::CheckpointResponse {
        namespaces: list
            .iter()
            .map(|c| pb::NamespaceCheckpoint {
                namespace: c.namespace.clone(),
                seq: c.outcome.seq,
                written: c.outcome.written,
                removed_checkpoints: c.outcome.removed_checkpoints.clone(),
                removed_segments: c.outcome.removed_segments.clone(),
            })
            .collect(),
    }
}

pub(crate) fn checkpoints_from_pb(r: pb::CheckpointResponse) -> Vec<Checkpointed> {
    r.namespaces
        .into_iter()
        .map(|c| Checkpointed {
            namespace: c.namespace,
            outcome: CheckpointOutcome {
                seq: c.seq,
                written: c.written,
                removed_checkpoints: c.removed_checkpoints,
                removed_segments: c.removed_segments,
            },
        })
        .collect()
}

fn backup_report_to_pb(r: &BackupReport) -> pb::BackupReport {
    pb::BackupReport {
        path: r.path.display().to_string(),
        history: r.history.to_string(),
        namespaces: r
            .namespaces
            .iter()
            .map(|n| pb::NamespaceBackup {
                id: n.id,
                name: n.name.clone(),
                seq: n.seq,
                time_micros: n.time.map(|t| t.0),
                checkpoints: n.checkpoints.clone(),
                segments: n.segments.clone(),
            })
            .collect(),
        bytes: r.bytes,
    }
}

fn backup_report_from_pb(r: Option<pb::BackupReport>) -> Result<BackupReport, Error> {
    let r = r.ok_or_else(|| missing("the backup report"))?;
    Ok(BackupReport {
        path: r.path.into(),
        history: history_from_pb(&r.history)?,
        namespaces: r
            .namespaces
            .into_iter()
            .map(|n| NamespaceBackup {
                id: n.id,
                name: n.name,
                seq: n.seq,
                time: n.time_micros.map(time_from_pb),
                checkpoints: n.checkpoints,
                segments: n.segments,
            })
            .collect(),
        bytes: r.bytes,
    })
}

pub(crate) fn backup_to_pb(b: &BackupDone) -> pb::BackupResponse {
    pb::BackupResponse { backup: Some(backup_report_to_pb(&b.report)), verify: b.verify.as_ref().map(verify_to_pb) }
}

pub(crate) fn backup_from_pb(r: pb::BackupResponse) -> Result<BackupDone, Error> {
    Ok(BackupDone { report: backup_report_from_pb(r.backup)?, verify: r.verify.map(verify_from_pb).transpose()? })
}

fn finding_to_pb(f: &Finding) -> pb::Finding {
    pb::Finding { path: f.path.as_ref().map(|p| p.display().to_string()), message: f.message.clone() }
}

fn finding_from_pb(f: pb::Finding) -> Finding {
    Finding { path: f.path.map(Into::into), message: f.message }
}

pub(crate) fn verify_to_pb(r: &VerifyReport) -> pb::VerifyReport {
    let kind = match r.kind {
        VerifyKind::DataDir => pb::VerifyKind::DataDir,
        VerifyKind::Backup => pb::VerifyKind::Backup,
        VerifyKind::Archive => pb::VerifyKind::Archive,
    };
    pb::VerifyReport {
        path: r.path.display().to_string(),
        kind: kind as i32,
        version: r.version,
        history: r.history.map(|h| h.to_string()),
        problems: r.problems.iter().map(finding_to_pb).collect(),
        notes: r.notes.iter().map(finding_to_pb).collect(),
        checkpoints: count(r.checkpoints),
        checkpoints_checked: count(r.checkpoints_checked),
        segments: count(r.segments),
        records: r.records,
        first_seq: r.first_seq,
        last_seq: r.last_seq,
        seq: r.seq,
        time_micros: r.time.map(|t| t.0),
        namespaces: r
            .namespaces
            .iter()
            .map(|n| pb::NamespaceVerify {
                id: n.id,
                name: n.name.clone(),
                checkpoints: count(n.checkpoints),
                checkpoints_checked: count(n.checkpoints_checked),
                segments: count(n.segments),
                records: n.records,
                first_seq: n.first_seq,
                last_seq: n.last_seq,
                seq: n.seq,
                time_micros: n.time.map(|t| t.0),
            })
            .collect(),
    }
}

pub(crate) fn verify_from_pb(r: pb::VerifyReport) -> Result<VerifyReport, Error> {
    let kind = match pb::VerifyKind::try_from(r.kind) {
        Ok(pb::VerifyKind::DataDir) => VerifyKind::DataDir,
        Ok(pb::VerifyKind::Backup) => VerifyKind::Backup,
        Ok(pb::VerifyKind::Archive) => VerifyKind::Archive,
        _ => return Err(Error::invalid(format!("unknown verify kind {}", r.kind))),
    };
    Ok(VerifyReport {
        path: r.path.into(),
        kind,
        version: r.version,
        history: r.history.as_deref().map(history_from_pb).transpose()?,
        problems: r.problems.into_iter().map(finding_from_pb).collect(),
        notes: r.notes.into_iter().map(finding_from_pb).collect(),
        checkpoints: uncount(r.checkpoints),
        checkpoints_checked: uncount(r.checkpoints_checked),
        segments: uncount(r.segments),
        records: r.records,
        first_seq: r.first_seq,
        last_seq: r.last_seq,
        seq: r.seq,
        time: r.time_micros.map(time_from_pb),
        namespaces: r
            .namespaces
            .into_iter()
            .map(|n| NamespaceVerify {
                id: n.id,
                name: n.name,
                checkpoints: uncount(n.checkpoints),
                checkpoints_checked: uncount(n.checkpoints_checked),
                segments: uncount(n.segments),
                records: n.records,
                first_seq: n.first_seq,
                last_seq: n.last_seq,
                seq: n.seq,
                time: n.time_micros.map(time_from_pb),
            })
            .collect(),
    })
}

/// A verify request's target. Errors: `invalid_argument` for both a backup
/// and the archive.
pub(crate) fn verify_target_from_pb(r: pb::VerifyRequest) -> Result<VerifyTarget, Error> {
    match (r.backup, r.archive) {
        (Some(_), true) => Err(Error::invalid("verify a backup or the archive, not both")),
        (Some(name), false) => Ok(VerifyTarget::Backup(name)),
        (None, true) => Ok(VerifyTarget::Archive),
        (None, false) => Ok(VerifyTarget::Store),
    }
}

pub(crate) fn verify_target_to_pb(t: &VerifyTarget) -> pb::VerifyRequest {
    match t {
        VerifyTarget::Store => pb::VerifyRequest { backup: None, archive: false },
        VerifyTarget::Backup(name) => pb::VerifyRequest { backup: Some(name.clone()), archive: false },
        VerifyTarget::Archive => pb::VerifyRequest { backup: None, archive: true },
    }
}

pub(crate) fn prune_to_pb(r: &PruneReport) -> pb::PruneArchiveResponse {
    pb::PruneArchiveResponse {
        report: Some(pb::PruneReport {
            archive: r.archive.display().to_string(),
            backup: r.backup.display().to_string(),
            dry_run: r.dry_run,
            namespaces: r
                .namespaces
                .iter()
                .map(|n| pb::NamespacePrune {
                    id: n.id,
                    name: n.name.clone(),
                    backup_checkpoint: n.backup_checkpoint,
                    removed_segments: n.removed_segments.clone(),
                    removed_checkpoints: n.removed_checkpoints.clone(),
                    kept_segments: count(n.kept_segments),
                })
                .collect(),
            untouched: r.untouched.clone(),
            bytes: r.bytes,
        }),
    }
}

pub(crate) fn prune_from_pb(r: pb::PruneArchiveResponse) -> Result<PruneReport, Error> {
    let r = r.report.ok_or_else(|| missing("the prune report"))?;
    Ok(PruneReport {
        archive: r.archive.into(),
        backup: r.backup.into(),
        dry_run: r.dry_run,
        namespaces: r
            .namespaces
            .into_iter()
            .map(|n| NamespacePrune {
                id: n.id,
                name: n.name,
                backup_checkpoint: n.backup_checkpoint,
                removed_segments: n.removed_segments,
                removed_checkpoints: n.removed_checkpoints,
                kept_segments: uncount(n.kept_segments),
            })
            .collect(),
        untouched: r.untouched,
        bytes: r.bytes,
    })
}

// ---- managed jobs (step 16f, ADR 0056) ----

/// A `StartJob` request as the trait's arguments: the namespace, the job
/// and its options (a job's timeout is the request's own: no
/// `grpc-timeout` applies to a job that outlives its call).
pub(crate) fn start_job_from_pb(r: pb::StartJobRequest) -> Result<(String, AnalyticsRequest, QueryOptions), Error> {
    let options = options_from_pb(r.options, None)?;
    let request = analyze_from_pb(pb::AnalyzeRequest {
        namespace: String::new(),
        projection: r.projection,
        job: r.job,
        options: None,
    })?;
    Ok((r.namespace, request, options))
}

pub(crate) fn start_job_to_pb(
    namespace: &str,
    request: &AnalyticsRequest,
    options: &QueryOptions,
) -> Result<pb::StartJobRequest, Error> {
    let r = analyze_to_pb(namespace, request, options)?;
    Ok(pb::StartJobRequest { namespace: r.namespace, projection: r.projection, job: r.job, options: r.options })
}

fn job_state_to_pb(state: JobState) -> pb::JobState {
    match state {
        JobState::Queued => pb::JobState::Queued,
        JobState::Collecting => pb::JobState::Collecting,
        JobState::Running => pb::JobState::Running,
        JobState::Done => pb::JobState::Done,
        JobState::Failed => pb::JobState::Failed,
        JobState::Cancelled => pb::JobState::Cancelled,
        JobState::Expired => pb::JobState::Expired,
    }
}

fn job_state_from_pb(state: i32) -> Result<JobState, Error> {
    Ok(match pb::JobState::try_from(state) {
        Ok(pb::JobState::Queued) => JobState::Queued,
        Ok(pb::JobState::Collecting) => JobState::Collecting,
        Ok(pb::JobState::Running) => JobState::Running,
        Ok(pb::JobState::Done) => JobState::Done,
        Ok(pb::JobState::Failed) => JobState::Failed,
        Ok(pb::JobState::Cancelled) => JobState::Cancelled,
        Ok(pb::JobState::Expired) => JobState::Expired,
        _ => return Err(Error::invalid(format!("unknown job state {}", state))),
    })
}

pub(crate) fn job_to_pb(j: &JobInfo) -> pb::JobInfo {
    pb::JobInfo {
        id: j.id,
        namespace: j.namespace.clone(),
        user: j.user.clone(),
        client: j.client.map(|c| c.to_string()),
        kind: j.kind.clone(),
        state: job_state_to_pb(j.state).into(),
        created_micros: j.created.0,
        started_micros: j.started.map(|t| t.0),
        ended_micros: j.ended.map(|t| t.0),
        elapsed_micros: micros(j.elapsed),
        progress: j.progress.as_ref().map(|p| pb::JobProgress { phase: p.phase.clone(), done: p.done, total: p.total }),
        nodes: j.nodes,
        edges: j.edges,
        seq: j.seq,
        rows: j.rows,
        truncated: j.truncated,
        result_bytes: j.result_bytes,
        error: j
            .error
            .as_ref()
            .map(|e| pb::Error { code: e.code().as_str().to_owned(), message: e.message().to_owned() }),
        expires_micros: j.expires.map(|t| t.0),
    }
}

pub(crate) fn job_from_pb(j: Option<pb::JobInfo>) -> Result<JobInfo, Error> {
    let j = j.ok_or_else(|| missing("the job"))?;
    Ok(JobInfo {
        id: j.id,
        namespace: j.namespace,
        user: j.user,
        client: client_from_pb(j.client)?,
        kind: j.kind,
        state: job_state_from_pb(j.state)?,
        created: time_from_pb(j.created_micros),
        started: j.started_micros.map(time_from_pb),
        ended: j.ended_micros.map(time_from_pb),
        elapsed: Duration::from_micros(j.elapsed_micros),
        progress: j.progress.map(|p| JobProgress { phase: p.phase, done: p.done, total: p.total }),
        nodes: j.nodes,
        edges: j.edges,
        seq: j.seq,
        rows: j.rows,
        truncated: j.truncated,
        result_bytes: j.result_bytes,
        // A code a newer server sends reads as `internal`
        error: j.error.map(|e| Error::new(Code::parse(&e.code).unwrap_or(Code::Internal), e.message)),
        expires: j.expires_micros.map(time_from_pb),
    })
}

pub(crate) fn jobs_to_pb(list: &Listed<JobInfo>) -> pb::ListJobsResponse {
    pb::ListJobsResponse { jobs: list.items.iter().map(job_to_pb).collect(), truncated: list.truncated }
}

pub(crate) fn jobs_from_pb(r: pb::ListJobsResponse) -> Result<Listed<JobInfo>, Error> {
    let items = r.jobs.into_iter().map(|j| job_from_pb(Some(j))).collect::<Result<_, _>>()?;
    Ok(Listed { items, truncated: r.truncated })
}

pub(crate) fn job_page_to_pb(p: &JobPage) -> pb::GetJobResultResponse {
    let rows = job_result_to_pb(&p.rows);
    pb::GetJobResultResponse {
        job: Some(job_to_pb(&p.job)),
        kind: rows.kind.into(),
        scores: rows.scores,
        groups: rows.groups,
        counts: rows.counts,
        next_offset: p.next_offset,
    }
}

pub(crate) fn job_page_from_pb(r: pb::GetJobResultResponse) -> Result<JobPage, Error> {
    let kind = pb::JobResultKind::try_from(r.kind).unwrap_or(pb::JobResultKind::Unspecified);
    let rows = job_result_from_pb(JobRows { kind, scores: r.scores, groups: r.groups, counts: r.counts })?;
    Ok(JobPage { job: job_from_pb(r.job)?, rows, next_offset: r.next_offset })
}

#[cfg(test)]
mod tests {
    use iwdb_engine::CommitTime;
    use iwdb_engine::metrics::Histogram;
    use iwdb_query::metrics;

    use super::*;

    #[test]
    fn metrics_and_logs_round_trip() {
        let mut m = Metrics::new();
        let h = Histogram::new();
        h.observe(Duration::from_millis(7));
        m.add(metrics::REQUESTS, &["Find", "ok"], Value::Counter(2));
        m.add(metrics::LOCK_HOLD, &["read"], Value::Histogram(h.snapshot()));
        m.add(metrics::READY, &[], Value::Gauge(1.0));
        assert_eq!(metrics_from_pb(metrics_to_pb(&m)), Ok(m));
        let tail = LogTail {
            events: vec![LogEvent {
                seq: 3,
                time: CommitTime(5),
                level: Level::Warn,
                target: "iwdb::audit".into(),
                message: "m".into(),
                fields: vec![("k".into(), "v".into())],
            }],
            last_seq: 4,
            missed: true,
        };
        assert_eq!(log_from_pb(log_to_pb(&tail)), Ok(tail));
        let r = RequestInfo {
            id: 9,
            operation: Operation::Changes,
            namespace: Some("social".into()),
            user: "ann".into(),
            client: Some("10.0.0.1".parse().unwrap_or(IpAddr::from([0, 0, 0, 0]))),
            started: CommitTime(1),
            elapsed: Duration::from_micros(42),
            cancellable: true,
        };
        assert_eq!(request_from_pb(Some(request_to_pb(&r))), Ok(r));
    }

    #[test]
    fn admin_writes_round_trip() {
        let checkpoints = vec![Checkpointed {
            namespace: "social".into(),
            outcome: CheckpointOutcome {
                seq: 7,
                written: true,
                removed_checkpoints: vec![3],
                removed_segments: vec![1, 4],
            },
        }];
        assert_eq!(checkpoints_from_pb(checkpoints_to_pb(&checkpoints)), checkpoints);
        let finding = |path: Option<&str>| Finding { path: path.map(Into::into), message: "m".into() };
        let report = VerifyReport {
            path: "/b/x".into(),
            kind: VerifyKind::Backup,
            version: Some(5),
            history: Some(HistoryId([7; 16])),
            problems: vec![finding(Some("/b/x/IWDB"))],
            notes: vec![finding(None)],
            checkpoints: 2,
            checkpoints_checked: 1,
            segments: 3,
            records: 9,
            first_seq: Some(1),
            last_seq: Some(9),
            seq: Some(9),
            time: Some(CommitTime(11)),
            namespaces: vec![NamespaceVerify {
                id: 1,
                name: "default".into(),
                checkpoints: 2,
                checkpoints_checked: 1,
                segments: 3,
                records: 9,
                first_seq: Some(1),
                last_seq: Some(9),
                seq: Some(9),
                time: None,
            }],
        };
        let done = BackupDone {
            report: BackupReport {
                path: "/b/x".into(),
                history: HistoryId([7; 16]),
                namespaces: vec![NamespaceBackup {
                    id: 1,
                    name: "default".into(),
                    seq: 9,
                    time: Some(CommitTime(11)),
                    checkpoints: vec![5],
                    segments: vec![6, 8],
                }],
                bytes: 1234,
            },
            verify: Some(report.clone()),
        };
        assert_eq!(backup_from_pb(backup_to_pb(&done)), Ok(done));
        assert_eq!(verify_from_pb(verify_to_pb(&report)), Ok(report));
        let prune = PruneReport {
            archive: "/a".into(),
            backup: "/b/x".into(),
            dry_run: true,
            namespaces: vec![NamespacePrune {
                id: 1,
                name: "default".into(),
                backup_checkpoint: 5,
                removed_segments: vec![1, 3],
                removed_checkpoints: vec![1],
                kept_segments: 2,
            }],
            untouched: vec![4],
            bytes: 99,
        };
        assert_eq!(prune_from_pb(prune_to_pb(&prune)), Ok(prune));
        for target in [VerifyTarget::Store, VerifyTarget::Archive, VerifyTarget::Backup("b".into())] {
            assert_eq!(verify_target_from_pb(verify_target_to_pb(&target)), Ok(target));
        }
        let both = pb::VerifyRequest { backup: Some("b".into()), archive: true };
        assert!(verify_target_from_pb(both).is_err());
    }
}
