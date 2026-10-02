//! Printing results: human-readable text by default, one JSON object with
//! `--json`. Presentation only.

use std::path::Path;

use iwdb::{
    BackupReport, CheckpointOutcome, CommitTime, Error, Finding, FsyncPolicy, HistoryId, IndexState, Kind,
    NamespaceResult, NamespaceStatus, RecoveryReport, RestoreReport, Status, StoreRecovery, StoreStatus, VerifyReport,
};
use serde_json::{json, Value};

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

    pub fn checkpoint(&self, dir: &Path, name: &str, o: &CheckpointOutcome) {
        let value = json!({
            "path": path(dir),
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

    pub fn namespaces(&self, status: &StoreStatus) {
        let value = json!({"namespaces": status.namespaces.iter().map(ns_status_json).collect::<Vec<_>>()});
        let text = status.namespaces.iter().map(ns_status_text).collect::<Vec<_>>().join("\n");
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

fn ns_status_json(n: &NamespaceStatus) -> Value {
    json!({
        "id": n.id, "name": n.name, "created": time(Some(n.created)), "seq": n.seq, "synced_seq": n.synced_seq,
        "checkpoint": n.checkpoint, "read_only": n.read_only, "checkpoint_failure": n.checkpoint_failure,
        "nodes": n.nodes, "edges": n.edges, "memory_bytes": n.memory_bytes, "constraints": n.constraints,
        "indexes": n.indexes.iter().map(|i| json!({
            "path": i.path.to_string(),
            "state": match &i.state { IndexState::Ready => "ready", IndexState::Building { .. } => "building" },
            "declared": i.declared, "unique": i.unique,
        })).collect::<Vec<_>>(),
    })
}

fn ns_status_text(n: &NamespaceStatus) -> String {
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
    text
}
