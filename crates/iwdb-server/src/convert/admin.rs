//! `admin.proto` (step 16c): the server's status, requests, readers,
//! metrics and log, both ways.

use std::net::IpAddr;
use std::time::Duration;

use iwdb_engine::metrics::{BUCKETS, BUCKETS_MICROS, HistogramSnapshot};
use iwdb_query::auth::Operation;
use iwdb_query::log::{Level, LogEvent, LogTail};
use iwdb_query::metrics::{Family, Kind, Metrics, Sample, Value};
use iwdb_query::requests::{ConsumerInfo, RequestInfo};
use iwdb_query::{DiskStatus, Error, LimitSource, Listed, MemoryState, MemoryStatus, RequestCounts, ServerStatus};

use super::{missing, status_from_pb, status_to_pb, time_from_pb};
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
        payload_bytes: m.payload_bytes,
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
        payload_bytes: m.payload_bytes,
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
}
