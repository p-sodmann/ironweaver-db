//! Printing results: human-readable text by default, one JSON object with
//! `--json`. Presentation only.

use std::path::Path;

use iwdb::import::{ExportReport, ImportReport, MergeReport};
use iwdb::{
    BackupReport, CheckpointOutcome, CommitTime, Error, Finding, FsyncPolicy, HistoryId, IndexState, Kind,
    NamespaceResult, NamespaceStatus, PruneReport, RecoveryReport, RestoreReport, Status, StoreRecovery, VerifyReport,
};
use iwdb_query::requests::RequestInfo;
use iwdb_query::{JobInfo, JobPage, JobResult, Listed, MemoryState, ServerStatus};
use serde_json::{Value, json};

pub struct Out {
    pub json: bool,
}

fn kind(kind: Kind) -> &'static str {
    match kind {
        Kind::DataDir => "data directory",
        Kind::Backup => "backup",
        Kind::Archive => "archive",
    }
}

fn fsync(policy: FsyncPolicy) -> String {
    match policy {
        FsyncPolicy::Always => "always".into(),
        FsyncPolicy::Group { max_delay, max_batch } => format!("group ({:?}, {} records)", max_delay, max_batch),
        FsyncPolicy::Off => "off".into(),
    }
}

fn time(t: Option<CommitTime>) -> Value {
    t.map_or(Value::Null, |t| Value::String(t.to_string()))
}

fn history(h: Option<HistoryId>) -> Value {
    h.map_or(Value::Null, |h| Value::String(h.to_string()))
}

fn path(p: &Path) -> Value {
    Value::String(p.display().to_string())
}

fn findings(list: &[Finding]) -> Value {
    Value::Array(list.iter().map(|f| json!({"path": f.path.as_deref().map(path), "message": f.message})).collect())
}

fn seqs(list: &[u64]) -> String {
    if list.is_empty() {
        return "none".into();
    }
    list.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")
}

fn recovery_json(s: &StoreRecovery) -> Value {
    let mut value = ns_recovery_json(s);
    value["created"] = json!(s.created);
    value["upgraded_from"] = json!(s.upgraded_from);
    value["removed_temp_files"] = json!(s.removed_temp_files.len());
    value["removed_orphans"] = json!(s.removed_orphans);
    value["namespaces"] = Value::Object(s.namespaces.iter().map(|(n, r)| (n.clone(), ns_recovery_json(r))).collect());
    value
}

fn ns_recovery_json(r: &RecoveryReport) -> Value {
    json!({
        "checkpoint": r.checkpoint,
        "skipped_checkpoints": r.skipped_checkpoints.iter().map(|s| json!({"seq": s.seq, "reason": s.reason})).collect::<Vec<_>>(),
        "replayed": r.replayed,
        "torn_tail": r.torn_tail.as_ref().map(|t| json!({
            "path": path(&t.path), "valid_len": t.valid_len, "file_len": t.file_len, "discarded_frames": t.discarded_frames,
        })),
        "seq": r.seq,
        "finished_import": r.finished_import,
    })
}

/// What recovery did, in one line.
fn recovery_text(s: &StoreRecovery) -> String {
    let mut parts = Vec::new();
    let r = s.namespaces.get("default").cloned().unwrap_or_default();
    match r.checkpoint {
        Some(seq) => parts.push(format!("replayed {} records onto checkpoint {}", r.replayed, seq)),
        None => parts.push(format!("replayed {} records", r.replayed)),
    }
    if s.created {
        parts.push("created the directory".into());
    }
    if s.namespaces.len() > 1 {
        parts.push(format!("{} namespaces recovered", s.namespaces.len()));
    }
    if !s.removed_orphans.is_empty() {
        parts.push(format!("removed the directories of {} namespaces the log doesn't list", s.removed_orphans.len()));
    }
    if let Some(from) = s.upgraded_from {
        parts.push(format!("upgraded it from layout {}", from));
    }
    if let Some(t) = &r.torn_tail {
        parts.push(format!("cut a torn tail off {} ({} -> {} bytes)", t.path.display(), t.file_len, t.valid_len));
    }
    if !r.skipped_checkpoints.is_empty() {
        parts.push(format!(
            "skipped damaged checkpoints {}",
            seqs(&r.skipped_checkpoints.iter().map(|s| s.seq).collect::<Vec<_>>())
        ));
    }
    if !s.removed_temp_files.is_empty() {
        parts.push(format!("removed {} temporary files", s.removed_temp_files.len()));
    }
    parts.join("; ")
}

impl Out {
    fn print(&self, value: Value, text: &str) {
        if self.json {
            println!("{}", value);
        } else {
            println!("{}", text);
        }
    }

    pub fn error(&self, error: &Error) {
        if self.json {
            println!("{}", json!({"error": error.to_string()}));
        }
        eprintln!("iwctl: {}", error);
    }

    pub fn status(&self, status: &Status) {
        let f = &status.files;
        let store = status.store.as_ref();
        let value = json!({
            "path": path(&f.path),
            "kind": kind(f.kind),
            "version": f.version,
            "history": history(f.history),
            "in_use": f.in_use,
            "checkpoints": f.checkpoints,
            "segments": f.segments,
            "first_segment": f.first_segment,
            "last_seq": f.last_seq,
            "last_time": time(f.last_time),
            "torn_tail": f.torn_tail,
            "temp_files": f.temp_files,
            "backup": f.manifest.as_ref().map(|m| json!({
                "seq": m.namespaces.first().map(|n| n.seq), "time": time(m.namespaces.first().and_then(|n| n.time)),
                "created": m.created.to_string(), "source": m.source, "files": m.files.len(),
                "namespaces": m.namespaces.iter().map(|n| json!({"id": n.id, "name": n.name.as_str(), "seq": n.seq, "time": time(n.time)})).collect::<Vec<_>>(),
            })),
            "namespaces": f.namespaces.iter().map(|n| json!({
                "id": n.id, "name": n.name, "checkpoints": n.checkpoints, "segments": n.segments,
                "first_segment": n.first_segment, "last_seq": n.last_seq, "last_time": time(n.last_time), "torn_tail": n.torn_tail,
            })).collect::<Vec<_>>(),
            "store": store.map(|s| json!({
                "seq": s.seq,
                "synced_seq": s.synced_seq,
                "checkpoint": s.checkpoint,
                "read_only": s.read_only,
                "fsync": fsync(s.fsync),
                "catalog_failure": s.catalog_failure,
                "recovery": recovery_json(&s.recovery),
                "namespaces": s.namespaces.iter().map(ns_status_json).collect::<Vec<_>>(),
            })),
        });
        let mut text = format!("{}  {}", kind(f.kind), f.path.display());
        let version = f.version.map_or("?".into(), |v| v.to_string());
        let what = if f.kind == Kind::Archive { "format" } else { "layout" };
        text += &format!(
            "\n  {} {}, history {}",
            what,
            version,
            f.history.map_or("none (layout 1)".into(), |h| h.to_string())
        );
        if f.in_use {
            text += "\n  in use: a store has it open (what follows is read from files that may be changing)";
        }
        if let Some(s) = store {
            let synced =
                s.synced_seq.map_or("none (fsync off: nothing is known to be durable)".into(), |v| v.to_string());
            text += &format!("\n  store: seq {}, synced seq {}, fsync {}", s.seq, synced, fsync(s.fsync));
            if let Some(cause) = &s.read_only {
                text += &format!("\n  read-only: {}", cause);
            }
            text += &format!("\n  recovery: {}", recovery_text(&s.recovery));
            if let Some(cause) = &s.catalog_failure {
                text += &format!("\n  namespace log failed: {}", cause);
            }
            for n in &s.namespaces {
                text += &format!("\n  {}", ns_status_text(n));
            }
        }
        if let Some(m) = &f.manifest {
            text += &format!("\n  backup of '{}', taken {}, {} files", m.source, m.created, m.files.len());
            for n in &m.namespaces {
                text += &format!(
                    "\n    namespace {} at seq {} ({})",
                    n.name,
                    n.seq,
                    n.time.map_or("no commit time".into(), |t| t.to_string())
                );
            }
        }
        if let Some(events) = f.namespace_events {
            text += &format!("\n  namespace log: {} events", events);
        }
        let several = f.namespaces.len() > 1;
        for n in &f.namespaces {
            let indent = if several { "    " } else { "  " };
            if several || n.name.as_deref().is_some_and(|name| name != "default") {
                text += &format!("\n  namespace {} (id {}):", n.name.as_deref().unwrap_or("?"), n.id);
            }
            if f.kind != Kind::Archive {
                text += &format!("\n{}checkpoints: {}", indent, seqs(&n.checkpoints));
            }
            text += &format!(
                "\n{}wal: {} segments{}, last record {}{}",
                indent,
                n.segments,
                n.first_segment.map_or(String::new(), |s| format!(" from seq {}", s)),
                n.last_seq.map_or("none".into(), |s| s.to_string()),
                n.last_time.map_or(String::new(), |t| format!(" at {}", t))
            );
            if n.torn_tail {
                text += &format!("\n{}the last segment has a torn tail (recovery cuts it)", indent);
            }
        }
        if f.temp_files > 0 {
            text += &format!("\n  {} temporary files (an interrupted write; the next open removes them)", f.temp_files);
        }
        self.print(value, &text);
    }

    /// A namespace's checkpoint in `location` (a data directory, or a
    /// server's endpoint).
    pub fn checkpoint(&self, location: &str, name: &str, o: &CheckpointOutcome) {
        let value = json!({
            "path": location,
            "namespace": name,
            "seq": o.seq,
            "written": o.written,
            "removed_checkpoints": o.removed_checkpoints,
            "removed_segments": o.removed_segments,
        });
        let text = format!(
            "checkpoint at seq {} ({}); removed checkpoints: {}; removed WAL segments: {}{}",
            o.seq,
            if o.written { "written" } else { "already current, nothing written" },
            seqs(&o.removed_checkpoints),
            seqs(&o.removed_segments),
            if name == "default" { String::new() } else { format!(" (namespace {})", name) }
        );
        self.print(value, &text);
    }

    pub fn backup(&self, r: &BackupReport) {
        let value = json!({
            "backup": {
                "path": path(&r.path), "seq": r.seq, "time": time(r.time), "history": r.history.to_string(),
                "checkpoints": r.checkpoints, "segments": r.segments, "bytes": r.bytes,
                "namespaces": r.namespaces.iter().map(|n| json!({
                    "id": n.id, "name": n.name, "seq": n.seq, "time": time(n.time),
                    "checkpoints": n.checkpoints, "segments": n.segments,
                })).collect::<Vec<_>>(),
            }
        });
        let mut text =
            format!("backed up {} namespaces into {}, {} bytes", r.namespaces.len(), r.path.display(), r.bytes);
        for n in &r.namespaces {
            text += &format!(
                "\n  {} at seq {}{}: checkpoints {}, {} WAL segments",
                n.name,
                n.seq,
                n.time.map_or(String::new(), |t| format!(" ({})", t)),
                seqs(&n.checkpoints),
                n.segments.len()
            );
        }
        self.print(value, &text);
    }

    pub fn restore(&self, r: &RestoreReport) {
        let value = json!({
            "restore": {
                "path": path(&r.path), "seq": r.seq, "time": time(r.time), "history": r.history.to_string(),
                "source_history": history(r.source_history), "checkpoint": r.checkpoint, "replayed": r.replayed,
                "skipped_checkpoints": r.skipped_checkpoints.iter().map(|s| s.seq).collect::<Vec<_>>(),
                "backup_segments": r.backup_segments, "archive_segments": r.archive_segments,
                "namespaces": r.namespaces.iter().map(|n| json!({
                    "id": n.id, "name": n.name, "seq": n.seq, "time": time(n.time), "checkpoint": n.checkpoint,
                    "replayed": n.replayed, "skipped_checkpoints": n.skipped_checkpoints.iter().map(|s| s.seq).collect::<Vec<_>>(),
                    "backup_segments": n.backup_segments, "archive_segments": n.archive_segments,
                })).collect::<Vec<_>>(),
            }
        });
        let mut text =
            format!("restored {} (new history {}): {} namespaces", r.path.display(), r.history, r.namespaces.len());
        for n in &r.namespaces {
            text += &format!(
                "\n  {} to seq {}{}: {} records replayed onto {}, {} segments from the backup, {} from the archive",
                n.name,
                n.seq,
                n.time.map_or(String::new(), |t| format!(" ({})", t)),
                n.replayed,
                n.checkpoint.map_or("an empty namespace".into(), |c| format!("checkpoint {}", c)),
                n.backup_segments,
                n.archive_segments
            );
        }
        self.print(value, &text);
    }

    pub fn namespaces(&self, namespaces: &[NamespaceStatus]) {
        let value = json!({"namespaces": namespaces.iter().map(ns_status_json).collect::<Vec<_>>()});
        let text = namespaces.iter().map(ns_status_text).collect::<Vec<_>>().join("\n");
        self.print(value, &text);
    }

    /// A server's error: its code and message (stderr; with `--json` also
    /// `{"error": {"code", "message"}}` on stdout).
    pub fn remote_error(&self, e: &iwdb_query::Error) {
        if self.json {
            println!("{}", json!({"error": {"code": e.code().as_str(), "message": e.message()}}));
        }
        eprintln!("iwctl: {}: {}", e.code(), e.message());
    }

    /// A server's status (`iwctl --server ... status`).
    pub fn server_status(&self, endpoint: &str, s: &ServerStatus) {
        let m = &s.memory;
        let r = &s.requests;
        let value = json!({
            "server": endpoint,
            "version": s.version,
            "started": s.started.to_string(),
            "ready": s.ready,
            "fsync": s.fsync,
            "memory": {
                "graph_bytes": m.graph_bytes, "payload_bytes": m.payload_bytes, "checkpoint_bytes": m.checkpoint_bytes,
                "working_bytes": m.working_bytes, "used_bytes": m.used_bytes, "limit_bytes": m.limit_bytes,
                "warn_bytes": m.warn_bytes, "refuse_writes_bytes": m.refuse_writes_bytes,
                "state": memory_state(m.state),
            },
            "disk": {"wal_bytes": s.disk.wal_bytes, "checkpoint_bytes": s.disk.checkpoint_bytes, "free_bytes": s.disk.free_bytes},
            "requests": {
                "active": r.active, "total": r.total, "timed_out": r.timed_out, "cancelled": r.cancelled,
                "rejected": r.rejected, "denied": r.denied,
            },
            "jobs": {
                "queued": s.jobs.queued, "running": s.jobs.running, "finished": s.jobs.finished,
                "result_bytes": s.jobs.result_bytes, "done_total": s.jobs.done_total,
                "failed_total": s.jobs.failed_total, "cancelled_total": s.jobs.cancelled_total,
            },
            "namespaces": s.namespaces.iter().map(ns_status_json).collect::<Vec<_>>(),
        });
        let mib = |b: u64| format!("{:.1} MiB", b as f64 / (1 << 20) as f64);
        let mut text = format!(
            "server  {}\n  version {}, started {}, {}, fsync {}",
            endpoint,
            s.version,
            s.started,
            if s.ready { "ready" } else { "draining" },
            s.fsync
        );
        text += &format!(
            "\n  memory: {} used ({}){}",
            mib(m.used_bytes),
            memory_state(m.state),
            m.limit_bytes.map_or(String::new(), |l| format!(" of a {} limit", mib(l)))
        );
        text += &format!(
            "\n  disk: WAL {}, checkpoints {}{}",
            mib(s.disk.wal_bytes),
            mib(s.disk.checkpoint_bytes),
            s.disk.free_bytes.map_or(String::new(), |f| format!(", {} free", mib(f)))
        );
        text += &format!(
            "\n  requests: {} running, {} ended ({} timed out, {} cancelled, {} rejected, {} denied)",
            r.active, r.total, r.timed_out, r.cancelled, r.rejected, r.denied
        );
        let j = &s.jobs;
        text += &format!(
            "\n  jobs: {} queued, {} running, {} kept ({} of results)",
            j.queued,
            j.running,
            j.finished,
            mib(j.result_bytes)
        );
        for n in &s.namespaces {
            text += &format!("\n  {}", ns_status_text(n));
        }
        self.print(value, &text);
    }

    /// The running requests (`iwctl --server ... requests`).
    pub fn requests(&self, list: &Listed<RequestInfo>) {
        let value =
            json!({"requests": list.items.iter().map(request_json).collect::<Vec<_>>(), "truncated": list.truncated});
        let mut text = if list.items.is_empty() { "no running requests".to_owned() } else { String::new() };
        let lines: Vec<String> = list.items.iter().map(request_text).collect();
        text += &lines.join("\n");
        if list.truncated {
            text += "\n(more not shown)";
        }
        self.print(value, &text);
    }

    /// A cancelled request (`iwctl --server ... cancel`).
    pub fn cancelled(&self, r: &RequestInfo) {
        let value = json!({"cancelled": request_json(r)});
        self.print(value, &format!("cancelled {}", request_text(r)));
    }

    /// The managed jobs (`iwctl --server ... jobs list`).
    pub fn jobs(&self, list: &Listed<JobInfo>) {
        let value = json!({"jobs": list.items.iter().map(job_json).collect::<Vec<_>>(), "truncated": list.truncated});
        let mut text = if list.items.is_empty() { "no jobs".to_owned() } else { String::new() };
        let lines: Vec<String> = list.items.iter().map(job_text).collect();
        text += &lines.join("\n");
        if list.truncated {
            text += "\n(more not shown)";
        }
        self.print(value, &text);
    }

    /// One job (`jobs show`), or one just cancelled (`jobs cancel`): `what`
    /// names which.
    pub fn job(&self, what: &str, j: &JobInfo) {
        let mut text = format!("{} {}", what, job_text(j));
        if let Some(e) = &j.error {
            text += &format!("\n  {}: {}", e.code(), e.message());
        }
        if let Some(t) = j.expires {
            text += &format!("\n  kept until {}", t);
        }
        self.print(json!({ what: job_json(j) }), &text);
    }

    /// A page of a job's result (`jobs result`).
    pub fn job_page(&self, p: &JobPage) {
        let (kind, rows): (&str, Vec<Value>) = match &p.rows {
            JobResult::Scores(r) => ("scores", r.iter().map(|(id, s)| json!({"id": id, "score": s})).collect()),
            JobResult::Counts(r) => ("counts", r.iter().map(|(id, c)| json!({"id": id, "count": c})).collect()),
            JobResult::Groups(r) => ("groups", r.iter().map(|ids| json!(ids)).collect()),
        };
        let value = json!({"job": job_json(&p.job), "kind": kind, "rows": rows, "next_offset": p.next_offset});
        let mut lines: Vec<String> = match &p.rows {
            JobResult::Scores(r) => r.iter().map(|(id, s)| format!("{}\t{}", id, s)).collect(),
            JobResult::Counts(r) => r.iter().map(|(id, c)| format!("{}\t{}", id, c)).collect(),
            JobResult::Groups(r) => r.iter().map(|ids| ids.join(" ")).collect(),
        };
        if let Some(next) = p.next_offset {
            lines.push(format!("(more: jobs result {} {})", p.job.id, next));
        }
        self.print(value, &lines.join("\n"));
    }

    /// What pruning an archive removed, or would remove.
    pub fn prune(&self, r: &PruneReport) {
        let value = json!({
            "prune": {
                "archive": path(&r.archive), "backup": path(&r.backup), "dry_run": r.dry_run, "bytes": r.bytes,
                "untouched": r.untouched,
                "namespaces": r.namespaces.iter().map(|n| json!({
                    "id": n.id, "name": n.name, "backup_checkpoint": n.backup_checkpoint,
                    "removed_segments": n.removed_segments, "removed_checkpoints": n.removed_checkpoints,
                    "kept_segments": n.kept_segments,
                })).collect::<Vec<_>>(),
            }
        });
        let segments: usize = r.namespaces.iter().map(|n| n.removed_segments.len()).sum();
        let mut text = format!(
            "{} {} archived segments ({} bytes) of {} before the backup {}",
            if r.dry_run { "would remove" } else { "removed" },
            segments,
            r.bytes,
            r.archive.display(),
            r.backup.display()
        );
        for n in &r.namespaces {
            text += &format!(
                "\n  {}: {} segments removed, {} kept (the backup's oldest checkpoint: {})",
                n.name,
                n.removed_segments.len(),
                n.kept_segments,
                if n.backup_checkpoint == 0 {
                    "none, so nothing goes".to_owned()
                } else {
                    n.backup_checkpoint.to_string()
                }
            );
            if !n.removed_checkpoints.is_empty() {
                text += &format!(", archived checkpoints {} removed", seqs(&n.removed_checkpoints));
            }
        }
        if !r.untouched.is_empty() {
            text += &format!(
                "\n  left alone (not in the backup): namespaces {}",
                r.untouched.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")
            );
        }
        self.print(value, &text);
    }

    pub fn namespace_result(&self, what: &str, r: &NamespaceResult) {
        let e = &r.event;
        let value = json!({
            what: {"id": e.id, "name": e.name.as_str(), "event": e.seq, "time": e.time.to_string(), "deduplicated": r.deduplicated}
        });
        let text = format!(
            "{} namespace '{}' (id {}, event {}{})",
            what,
            e.name,
            e.id,
            e.seq,
            if r.deduplicated { ", a retry: nothing changed" } else { "" }
        );
        self.print(value, &text);
    }

    pub fn import(&self, r: &ImportReport) {
        let e = &r.event;
        let indexes: Vec<String> = r.indexes.iter().map(|p| p.to_string()).collect();
        let value = json!({
            "imported": {
                "id": e.id, "name": e.name.as_str(), "event": e.seq, "time": e.time.to_string(),
                "format": r.format.name(), "seq": r.seq, "nodes": r.nodes, "edges": r.edges, "indexes": indexes,
                "dropped": r.dropped, "bytes_read": r.bytes_read, "checkpoint_bytes": r.checkpoint_bytes,
            }
        });
        let mut text = format!(
            "imported namespace '{}' (id {}) from a {} file: {} nodes, {} edges",
            e.name, e.id, r.format, r.nodes, r.edges
        );
        if !indexes.is_empty() {
            text += &format!(", indexes {}", indexes.join(", "));
        }
        if !r.dropped.is_empty() {
            text += &format!("\nleft out (a namespace has no place for them): {}", r.dropped.join(", "));
        }
        self.print(value, &text);
    }

    pub fn merge(&self, name: &str, r: &MergeReport) {
        let created: Vec<String> = r.created_indexes.iter().map(|p| p.to_string()).collect();
        let value = json!({
            "merged": {
                "namespace": name, "format": r.format.name(), "nodes": r.nodes, "edges": r.edges,
                "created_indexes": created, "dropped": r.dropped, "bytes_read": r.bytes_read,
                "commits": r.commits, "first_seq": r.first_seq, "last_seq": r.last_seq,
            }
        });
        let seqs = match (r.first_seq, r.last_seq) {
            (Some(a), Some(b)) => format!(", seqs {} to {}", a, b),
            _ => String::new(),
        };
        let mut text = format!(
            "merged a {} file into namespace '{}': {} nodes, {} edges in {} commits{}",
            r.format, name, r.nodes, r.edges, r.commits, seqs
        );
        if !created.is_empty() {
            text += &format!(", created indexes {}", created.join(", "));
        }
        if !r.dropped.is_empty() {
            text += &format!("\nleft out (a namespace has no place for them): {}", r.dropped.join(", "));
        }
        self.print(value, &text);
    }

    pub fn export(&self, file: &Path, r: &ExportReport) {
        let value = json!({
            "exported": {"path": path(file), "format": r.format.name(), "seq": r.seq, "nodes": r.nodes, "edges": r.edges, "bytes": r.bytes}
        });
        let text = format!(
            "exported seq {} ({} nodes, {} edges) to {} ({}, {} bytes)",
            r.seq,
            r.nodes,
            r.edges,
            file.display(),
            r.format,
            r.bytes
        );
        self.print(value, &text);
    }

    pub fn commit(&self, what: &str, seq: u64, deduplicated: bool) {
        let value = json!({"catalog_change": what, "seq": seq, "deduplicated": deduplicated});
        let text = format!("{} at seq {}{}", what, seq, if deduplicated { " (a retry: nothing changed)" } else { "" });
        self.print(value, &text);
    }

    pub fn indexes(&self, ns: &NamespaceStatus, catalog: &iwdb::NamespaceCatalog) {
        let indexes: Vec<Value> = ns
            .indexes
            .iter()
            .map(|i| {
                let (state, progress) = match &i.state {
                    IndexState::Ready => ("ready", Value::Null),
                    IndexState::Building { scanned, total } => ("building", json!([scanned, total])),
                };
                let size = i.size.map_or(Value::Null, |s| {
                    json!({"entries": s.entries, "distinct_keys": s.distinct_keys, "memory_bytes": s.memory_bytes})
                });
                json!({"path": i.path.to_string(), "state": state, "progress": progress, "declared": i.declared, "unique": i.unique, "size": size})
            })
            .collect();
        let constraints: Vec<Value> = catalog.constraints().map(|c| Value::String(c.to_string())).collect();
        let value = json!({"namespace": ns.name, "indexes": indexes, "constraints": constraints});
        let mut text = format!("namespace {}:", ns.name);
        for i in &ns.indexes {
            let state = match &i.state {
                IndexState::Ready => "ready".to_owned(),
                IndexState::Building { scanned, total } => format!("building ({}/{})", scanned, total),
            };
            let by = match (i.declared, i.unique) {
                (true, true) => "declared, unique constraint",
                (true, false) => "declared",
                (false, _) => "unique constraint",
            };
            text += &format!("\n  index {} {} ({})", i.path, state, by);
            if let Some(s) = i.size {
                text +=
                    &format!(": {} entries, {} distinct, {} KiB", s.entries, s.distinct_keys, s.memory_bytes / 1024);
            }
        }
        for c in catalog.constraints() {
            text += &format!("\n  {}", c);
        }
        self.print(value, &text);
    }

    pub fn verify(&self, r: &VerifyReport) {
        let value = json!({
            "verify": {
                "path": path(&r.path),
                "kind": kind(r.kind),
                "ok": r.is_ok(),
                "version": r.version,
                "history": history(r.history),
                "problems": findings(&r.problems),
                "notes": findings(&r.notes),
                "checkpoints": r.checkpoints,
                "checkpoints_checked": r.checkpoints_checked,
                "segments": r.segments,
                "records": r.records,
                "first_seq": r.first_seq,
                "last_seq": r.last_seq,
                "seq": r.seq,
                "time": time(r.time),
                "namespaces": r.namespaces.iter().map(|n| json!({
                    "id": n.id, "name": n.name, "checkpoints": n.checkpoints, "checkpoints_checked": n.checkpoints_checked,
                    "segments": n.segments, "records": n.records, "first_seq": n.first_seq, "last_seq": n.last_seq,
                    "seq": n.seq, "time": time(n.time),
                })).collect::<Vec<_>>(),
            }
        });
        let verdict = if r.is_ok() { "ok" } else { "DAMAGED" };
        let mut text = format!("verify {} ({}): {}", r.path.display(), kind(r.kind), verdict);
        text += &format!(
            "\n  {} checkpoints ({} loaded and checked), {} WAL segments, {} records{}",
            r.checkpoints,
            r.checkpoints_checked,
            r.segments,
            r.records,
            match (r.first_seq, r.last_seq) {
                (Some(a), Some(b)) => format!(" (seq {} to {})", a, b),
                _ => String::new(),
            }
        );
        if let Some(seq) = r.seq {
            text += &format!("\n  recovers to seq {}", seq);
        }
        if r.namespaces.len() > 1 || r.namespaces.iter().any(|n| n.name != "default") {
            for n in &r.namespaces {
                text += &format!(
                    "\n  namespace {}: {} checkpoints, {} segments, {} records, seq {}",
                    n.name,
                    n.checkpoints,
                    n.segments,
                    n.records,
                    n.seq.map_or("?".into(), |s| s.to_string())
                );
            }
        }
        for p in &r.problems {
            text += &format!("\n  problem: {}", p);
        }
        for n in &r.notes {
            text += &format!("\n  note: {}", n);
        }
        self.print(value, &text);
    }
}

pub fn ns_status_json(n: &NamespaceStatus) -> Value {
    json!({
        "id": n.id, "name": n.name, "created": time(Some(n.created)), "seq": n.seq, "synced_seq": n.synced_seq,
        "checkpoint": n.checkpoint, "read_only": n.read_only, "checkpoint_failure": n.checkpoint_failure,
        "nodes": n.nodes, "edges": n.edges, "memory_bytes": n.memory_bytes, "constraints": n.constraints,
        "indexes": n.indexes.iter().map(|i| json!({
            "path": i.path.to_string(),
            "state": match &i.state { IndexState::Ready => "ready", IndexState::Building { .. } => "building" },
            "declared": i.declared, "unique": i.unique,
        })).collect::<Vec<_>>(),
        "marks": n.marks.iter().map(|m| json!({"name": m.name, "position": m.position, "seq": m.seq})).collect::<Vec<_>>(),
    })
}

pub fn ns_status_text(n: &NamespaceStatus) -> String {
    let mut text = format!(
        "namespace {} (id {}): seq {}, {} nodes, {} edges, {} indexes, {} constraints, ~{} KiB",
        n.name,
        n.id,
        n.seq,
        n.nodes,
        n.edges,
        n.indexes.len(),
        n.constraints,
        n.memory_bytes / 1024
    );
    if let Some(cause) = &n.read_only {
        text += &format!(" (read-only: {})", cause);
    }
    for m in &n.marks {
        text += &format!("\n  mark {:?} at {} (seq {})", m.name, m.position, m.seq);
    }
    text
}

fn memory_state(state: MemoryState) -> &'static str {
    match state {
        MemoryState::Normal => "normal",
        MemoryState::Warn => "warn",
        MemoryState::RefusingWrites => "refusing writes",
    }
}

fn request_json(r: &RequestInfo) -> Value {
    json!({
        "id": r.id, "operation": r.operation.name(), "namespace": r.namespace, "user": r.user,
        "client": r.client.map(|c| c.to_string()), "started": r.started.to_string(),
        "elapsed_micros": u64::try_from(r.elapsed.as_micros()).unwrap_or(u64::MAX), "cancellable": r.cancellable,
    })
}

fn request_text(r: &RequestInfo) -> String {
    format!(
        "request {}: {}{} by {}{}, running {:.1} s{}",
        r.id,
        r.operation.name(),
        r.namespace.as_deref().map_or(String::new(), |n| format!(" on {}", n)),
        r.user,
        r.client.map_or(String::new(), |c| format!(" from {}", c)),
        r.elapsed.as_secs_f64(),
        if r.cancellable { "" } else { " (can't be cancelled)" }
    )
}

fn job_json(j: &JobInfo) -> Value {
    json!({
        "id": j.id, "namespace": j.namespace, "user": j.user, "client": j.client.map(|c| c.to_string()),
        "kind": j.kind, "state": j.state.as_str(), "created": j.created.to_string(),
        "started": time(j.started), "ended": time(j.ended),
        "elapsed_micros": u64::try_from(j.elapsed.as_micros()).unwrap_or(u64::MAX),
        "nodes": j.nodes, "edges": j.edges, "seq": j.seq, "rows": j.rows, "truncated": j.truncated,
        "result_bytes": j.result_bytes,
        "error": j.error.as_ref().map(|e| json!({"code": e.code().as_str(), "message": e.message()})),
        "expires": time(j.expires),
    })
}

fn job_text(j: &JobInfo) -> String {
    let size = match (j.nodes, j.edges) {
        (Some(n), Some(e)) => format!(", {} nodes and {} edges", n, e),
        _ => String::new(),
    };
    let rows = j.rows.map_or(String::new(), |r| format!(", {} rows{}", r, if j.truncated { " (cut)" } else { "" }));
    format!(
        "job {}: {} on {} by {}, {} for {:.1} s{}{}",
        j.id,
        j.kind,
        j.namespace,
        j.user,
        j.state,
        j.elapsed.as_secs_f64(),
        size,
        rows
    )
}
