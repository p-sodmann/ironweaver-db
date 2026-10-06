//! The metrics (step 16c, ADR 0050): the one list of what is exported
//! ([`METRICS`], documented in `documentation/api/metrics.md`), a snapshot
//! of their values ([`Metrics`]), and Prometheus' text exposition format
//! ([`Metrics::to_prometheus`]). No exporter library: the values are
//! atomics the crates below keep, read when asked; nothing is pushed.
//!
//! **Labels are bounded**: `operation` (one of [`Operation::ALL`]), `code`
//! (`ok` or one of [`Code::ALL`]), `lock` (`read`, `write`), `namespace`
//! (a live namespace: its series go when it is dropped), `version`. Never
//! an id, a user, a client or a value of the data.

use std::fmt::Write as _;

use iwdb_engine::metrics::{BUCKETS_MICROS, HistogramSnapshot};

#[cfg(doc)]
use crate::{Code, auth::Operation};

/// What a metric is, in Prometheus' terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Counter,
    Gauge,
    Histogram,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        }
    }

    pub fn parse(name: &str) -> Option<Kind> {
        [Kind::Counter, Kind::Gauge, Kind::Histogram].into_iter().find(|k| k.as_str() == name)
    }
}

/// A metric the server exports: its name, kind, labels, unit and meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetricDef {
    pub name: &'static str,
    pub kind: Kind,
    pub labels: &'static [&'static str],
    /// `seconds`, `bytes`, `commits`, ... or empty for a count or a flag.
    pub unit: &'static str,
    pub help: &'static str,
}

macro_rules! metrics {
    ($($id:ident: $name:literal, $kind:ident, [$($label:literal),*], $unit:literal, $help:literal;)*) => {
        $(
            #[doc = $help]
            pub const $id: &str = $name;
        )*

        /// Every metric, in the order they are exported.
        pub const METRICS: &[MetricDef] = &[$(MetricDef {
            name: $name,
            kind: Kind::$kind,
            labels: &[$($label),*],
            unit: $unit,
            help: $help,
        },)*];
    };
}

metrics! {
    BUILD_INFO: "iwdb_build_info", Gauge, ["version"], "", "Always 1; the server's version is its label.";
    START_TIME: "iwdb_start_time_seconds", Gauge, [], "seconds", "When the database started serving, in seconds since 1970 (UTC).";
    READY: "iwdb_ready", Gauge, [], "", "1 while the server serves requests, 0 while it drains.";
    REQUESTS_ACTIVE: "iwdb_requests_active", Gauge, [], "", "Requests running now.";
    REQUESTS: "iwdb_requests_total", Counter, ["operation", "code"], "", "Requests that ended, by operation and outcome: `ok` or the error code (`timeout`, `unavailable` for rejected ones, `permission_denied`, ...).";
    REQUEST_DURATION: "iwdb_request_duration_seconds", Histogram, ["operation"], "seconds", "How long requests ran, by operation, queueing included.";
    COMMIT_DURATION: "iwdb_commit_duration_seconds", Histogram, [], "seconds", "Commits (data and catalog) in the commit pipeline, from the call to the answer: waiting for the writer, the WAL append and fsync, and the apply.";
    WAL_FSYNC_DURATION: "iwdb_wal_fsync_duration_seconds", Histogram, [], "seconds", "Fsyncs of the WALs.";
    CHECKPOINT_DURATION: "iwdb_checkpoint_duration_seconds", Histogram, [], "seconds", "Checkpoint runs that wrote a checkpoint.";
    LOCK_HOLD: "iwdb_lock_hold_seconds", Histogram, ["lock"], "seconds", "How long namespace locks were held: `write` by commits (apply and index flush), `read` by reads.";
    NAMESPACE_NODES: "iwdb_namespace_nodes", Gauge, ["namespace"], "", "Nodes in the namespace.";
    NAMESPACE_EDGES: "iwdb_namespace_edges", Gauge, ["namespace"], "", "Edges in the namespace.";
    NAMESPACE_MEMORY: "iwdb_namespace_memory_bytes", Gauge, ["namespace"], "bytes", "Approximate bytes the namespace's graph uses, indexes included (attribute payloads not counted; see `iwdb_memory_used_bytes`).";
    WAL_BYTES: "iwdb_wal_bytes", Gauge, ["namespace"], "bytes", "Bytes of the namespace's WAL segments on disk.";
    CHECKPOINT_BYTES: "iwdb_checkpoint_bytes", Gauge, ["namespace"], "bytes", "Bytes of the namespace's checkpoints on disk.";
    CHECKPOINT_LAG: "iwdb_checkpoint_lag_commits", Gauge, ["namespace"], "commits", "Commits since the namespace's newest checkpoint: what recovery would replay.";
    LAST_CHECKPOINT: "iwdb_last_checkpoint_timestamp_seconds", Gauge, ["namespace"], "seconds", "When the namespace's newest checkpoint was written, in seconds since 1970 (UTC); no sample without one.";
    UNSYNCED: "iwdb_unsynced_commits", Gauge, ["namespace"], "commits", "Commits applied but not known to be durable; no sample under the `off` fsync policy before a sync.";
    READ_ONLY: "iwdb_namespace_read_only", Gauge, ["namespace"], "", "1 if the namespace is read-only after a failure (until the server restarts).";
    CHECKPOINT_FAILED: "iwdb_checkpoint_failed", Gauge, ["namespace"], "", "1 if the namespace's last checkpoint failed.";
    DISK_FREE: "iwdb_disk_free_bytes", Gauge, [], "bytes", "Bytes free for the server on the data directory's file system; no sample where it can't be read.";
    MEMORY_USED: "iwdb_memory_used_bytes", Gauge, ["part"], "bytes", "Memory the server counts against its limit, by part: `graph` (the live graphs and indexes), `payload` (their attributes, estimated), `checkpoint` (the checkpointers' copies), `working` (analytics projections and index builds, estimated).";
    MEMORY_LIMIT: "iwdb_memory_limit_bytes", Gauge, [], "bytes", "The memory limit (`[memory] limit_bytes`, or the cgroup's); no sample without one.";
    MEMORY_WARN: "iwdb_memory_warn_bytes", Gauge, [], "bytes", "From here on the server warns (`warn_at` of the limit); no sample without a limit.";
    MEMORY_REFUSE_WRITES: "iwdb_memory_refuse_writes_bytes", Gauge, [], "bytes", "From here on the server refuses writes with `resource_exhausted` (`refuse_writes_at` of the limit); no sample without a limit.";
    MEMORY_STATE: "iwdb_memory_state", Gauge, [], "", "0 normal, 1 above the warning line, 2 refusing writes. Each state is left 5 % of the limit below its line.";
}

/// A metric's value in one sample.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Counter(u64),
    Gauge(f64),
    /// Durations in the buckets of [`BUCKETS_MICROS`].
    Histogram(HistogramSnapshot),
}

/// One series of a metric: its labels (in the order of
/// [`MetricDef::labels`]) and value.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    pub labels: Vec<(String, String)>,
    pub value: Value,
}

impl Sample {
    /// The value of label `name`, if the sample has it.
    pub fn label(&self, name: &str) -> Option<&str> {
        self.labels.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// A metric and its samples.
#[derive(Clone, Debug, PartialEq)]
pub struct Family {
    pub name: String,
    pub kind: Kind,
    pub help: String,
    pub samples: Vec<Sample>,
}

/// The metrics at one moment: every metric of [`METRICS`], in its order,
/// each with its samples (maybe none).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Metrics {
    pub families: Vec<Family>,
}

impl Metrics {
    /// Every metric of [`METRICS`], without samples.
    pub fn new() -> Self {
        let families = METRICS
            .iter()
            .map(|d| Family { name: d.name.to_owned(), kind: d.kind, help: d.help.to_owned(), samples: Vec::new() })
            .collect();
        Metrics { families }
    }

    /// Add a sample to metric `name` (one of [`METRICS`]; others are
    /// ignored). `labels` are the metric's labels' values, in order.
    pub fn add(&mut self, name: &str, labels: &[&str], value: Value) {
        let Some(def) = METRICS.iter().find(|d| d.name == name) else {
            debug_assert!(false, "{} isn't in METRICS", name);
            return;
        };
        debug_assert_eq!(def.labels.len(), labels.len(), "{}", name);
        if let Some(family) = self.families.iter_mut().find(|f| f.name == name) {
            let labels = def.labels.iter().zip(labels).map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect();
            family.samples.push(Sample { labels, value });
        }
    }

    /// The family of metric `name`.
    pub fn family(&self, name: &str) -> Option<&Family> {
        self.families.iter().find(|f| f.name == name)
    }

    /// Keep only the samples whose `namespace` label (if they have one)
    /// passes `keep`.
    pub fn retain_namespaces(&mut self, mut keep: impl FnMut(&str) -> bool) {
        for family in &mut self.families {
            family.samples.retain(|s| s.label("namespace").is_none_or(&mut keep));
        }
    }

    /// The Prometheus text exposition format (version 0.0.4): `# HELP` and
    /// `# TYPE` per metric, then its samples; histograms as cumulative
    /// `_bucket` series with `le`, `_sum` and `_count`.
    pub fn to_prometheus(&self) -> String {
        let mut out = String::new();
        for family in &self.families {
            let _ = writeln!(out, "# HELP {} {}", family.name, escape_help(&family.help));
            let _ = writeln!(out, "# TYPE {} {}", family.name, family.kind.as_str());
            for sample in &family.samples {
                match &sample.value {
                    Value::Counter(n) => line(&mut out, &family.name, &sample.labels, None, &n.to_string()),
                    Value::Gauge(x) => line(&mut out, &family.name, &sample.labels, None, &float(*x)),
                    Value::Histogram(h) => {
                        let bucket = format!("{}_bucket", family.name);
                        for (i, n) in h.cumulative().iter().enumerate() {
                            let le = BUCKETS_MICROS.get(i).map_or("+Inf".to_owned(), |&b| float(b as f64 / 1e6));
                            line(&mut out, &bucket, &sample.labels, Some(&le), &n.to_string());
                        }
                        let sum = float(h.sum_micros as f64 / 1e6);
                        line(&mut out, &format!("{}_sum", family.name), &sample.labels, None, &sum);
                        let count = h.count().to_string();
                        line(&mut out, &format!("{}_count", family.name), &sample.labels, None, &count);
                    }
                }
            }
        }
        out
    }
}

fn line(out: &mut String, name: &str, labels: &[(String, String)], le: Option<&str>, value: &str) {
    out.push_str(name);
    if !labels.is_empty() || le.is_some() {
        out.push('{');
        let le = le.map(|v| ("le".to_owned(), v.to_owned()));
        for (i, (n, v)) in labels.iter().chain(le.as_ref()).enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}=\"{}\"", n, escape_label(v));
        }
        out.push('}');
    }
    out.push(' ');
    out.push_str(value);
    out.push('\n');
}

fn float(x: f64) -> String {
    if x.is_nan() {
        "NaN".to_owned()
    } else if x.is_infinite() {
        if x > 0.0 { "+Inf".to_owned() } else { "-Inf".to_owned() }
    } else {
        format!("{}", x)
    }
}

fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn escape_help(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use iwdb_engine::metrics::Histogram;

    use super::*;

    #[test]
    fn names_are_unique_and_follow_prometheus_conventions() {
        let mut names: Vec<_> = METRICS.iter().map(|d| d.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), METRICS.len());
        for d in METRICS {
            assert!(d.name.starts_with("iwdb_"), "{}", d.name);
            assert!(d.name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'), "{}", d.name);
            assert_eq!(d.kind == Kind::Counter, d.name.ends_with("_total"), "{}", d.name);
            if !d.unit.is_empty() && d.unit != "commits" {
                assert!(d.name.ends_with(&format!("_{}", d.unit)), "{} in {}", d.name, d.unit);
            }
            for label in d.labels {
                assert!(["operation", "code", "lock", "namespace", "part", "version"].contains(label), "{}", label);
            }
        }
    }

    /// `documentation/api/metrics.md` lists exactly [`METRICS`], in order,
    /// with their kind, labels, unit and meaning.
    #[test]
    fn every_metric_is_documented_and_every_documented_one_exported() {
        let doc = include_str!("../../../documentation/api/metrics.md");
        let rows: Vec<Vec<String>> = doc
            .lines()
            .skip_while(|l| !l.starts_with("## The metrics"))
            .filter(|l| l.starts_with("| `iwdb_"))
            .map(|l| l.trim_matches('|').split(" | ").map(|c| c.trim().to_owned()).collect())
            .collect();
        let documented: Vec<&str> = rows.iter().map(|r| r[0].trim_matches('`')).collect();
        let exported: Vec<&str> = METRICS.iter().map(|d| d.name).collect();
        assert_eq!(documented, exported, "metrics.md's table and METRICS");
        for (row, d) in rows.iter().zip(METRICS) {
            let labels: Vec<String> = d.labels.iter().map(|l| format!("`{}`", l)).collect();
            let labels = if labels.is_empty() { "–".to_owned() } else { labels.join(", ") };
            let unit = if d.unit.is_empty() { "–" } else { d.unit };
            assert_eq!(
                row[1..],
                [d.kind.as_str().to_owned(), labels, unit.to_owned(), d.help.to_owned()],
                "{}",
                d.name
            );
        }
    }

    #[test]
    fn the_text_format() {
        let mut m = Metrics::new();
        m.add(REQUESTS, &["Find", "ok"], Value::Counter(3));
        m.add(NAMESPACE_NODES, &["so\"cial"], Value::Gauge(12.0));
        let h = Histogram::new();
        h.observe(Duration::from_millis(2));
        h.observe(Duration::from_secs(40));
        m.add(LOCK_HOLD, &["write"], Value::Histogram(h.snapshot()));
        let text = m.to_prometheus();
        assert!(
            text.contains(
                "# TYPE iwdb_requests_total counter\niwdb_requests_total{operation=\"Find\",code=\"ok\"} 3\n"
            )
        );
        assert!(text.contains("iwdb_namespace_nodes{namespace=\"so\\\"cial\"} 12\n"), "{}", text);
        assert!(text.contains("iwdb_lock_hold_seconds_bucket{lock=\"write\",le=\"0.001\"} 0\n"), "{}", text);
        assert!(text.contains("iwdb_lock_hold_seconds_bucket{lock=\"write\",le=\"0.0025\"} 1\n"), "{}", text);
        assert!(text.contains("iwdb_lock_hold_seconds_bucket{lock=\"write\",le=\"30\"} 1\n"), "{}", text);
        assert!(text.contains("iwdb_lock_hold_seconds_bucket{lock=\"write\",le=\"+Inf\"} 2\n"), "{}", text);
        assert!(text.contains("iwdb_lock_hold_seconds_sum{lock=\"write\"} 40.002\n"), "{}", text);
        assert!(text.contains("iwdb_lock_hold_seconds_count{lock=\"write\"} 2\n"), "{}", text);
        // Every metric has its HELP and TYPE, samples or not
        for d in METRICS {
            assert!(text.contains(&format!("# TYPE {} {}\n", d.name, d.kind.as_str())), "{}", d.name);
        }
        m.retain_namespaces(|n| n == "other");
        assert!(m.family(NAMESPACE_NODES).unwrap().samples.is_empty());
        assert_eq!(m.family(REQUESTS).unwrap().samples.len(), 1);
    }
}
