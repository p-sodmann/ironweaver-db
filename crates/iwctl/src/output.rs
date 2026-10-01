//! Printing results: human-readable text by default, one JSON object with
//! `--json`. Presentation only.

use std::path::Path;

use iwdb::{
    BackupReport, CheckpointOutcome, CommitTime, Error, Finding, FsyncPolicy, HistoryId, Kind, RecoveryReport,
    RestoreReport, Status, VerifyReport,
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

fn recovery_json(r: &RecoveryReport) -> Value {
    json!({
        "created": r.created,
        "upgraded_from": r.upgraded_from,
        "checkpoint": r.checkpoint,
        "skipped_checkpoints": r.skipped_checkpoints.iter().map(|s| json!({"seq": s.seq, "reason": s.reason})).collect::<Vec<_>>(),
        "replayed": r.replayed,
        "torn_tail": r.torn_tail.as_ref().map(|t| json!({
            "path": path(&t.path), "valid_len": t.valid_len, "file_len": t.file_len, "discarded_frames": t.discarded_frames,
        })),
        "removed_temp_files": r.removed_temp_files.len(),
        "seq": r.seq,
    })
}

/// What recovery did, in one line.
fn recovery_text(r: &RecoveryReport) -> String {
    let mut parts = Vec::new();
    match r.checkpoint {
        Some(seq) => parts.push(format!("replayed {} records onto checkpoint {}", r.replayed, seq)),
        None => parts.push(format!("replayed {} records", r.replayed)),
    }
    if r.created {
        parts.push("created the directory".into());
    }
    if let Some(from) = r.upgraded_from {
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
    if !r.removed_temp_files.is_empty() {
        parts.push(format!("removed {} temporary files", r.removed_temp_files.len()));
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
                "seq": m.seq, "time": time(m.time), "created": m.created.to_string(), "source": m.source, "files": m.files.len(),
            })),
            "store": store.map(|s| json!({
                "seq": s.seq,
                "synced_seq": s.synced_seq,
                "checkpoint": s.checkpoint,
                "read_only": s.read_only,
                "fsync": fsync(s.fsync),
                "recovery": recovery_json(&s.recovery),
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
        }
        if let Some(m) = &f.manifest {
            text += &format!(
                "\n  backup of '{}' at seq {} ({}), taken {}, {} files",
                m.source,
                m.seq,
                m.time.map_or("no commit time".into(), |t| t.to_string()),
                m.created,
                m.files.len()
            );
        }
        if f.kind != Kind::Archive {
            text += &format!("\n  checkpoints: {}", seqs(&f.checkpoints));
        }
        text += &format!(
            "\n  wal: {} segments{}, last record {}{}",
            f.segments,
            f.first_segment.map_or(String::new(), |s| format!(" from seq {}", s)),
            f.last_seq.map_or("none".into(), |s| s.to_string()),
            f.last_time.map_or(String::new(), |t| format!(" at {}", t))
        );
        if f.torn_tail {
            text += "\n  the last segment has a torn tail (recovery cuts it)";
        }
        if f.temp_files > 0 {
            text += &format!("\n  {} temporary files (an interrupted write; the next open removes them)", f.temp_files);
        }
        self.print(value, &text);
    }

    pub fn checkpoint(&self, dir: &Path, o: &CheckpointOutcome) {
        let value = json!({
            "path": path(dir),
            "seq": o.seq,
            "written": o.written,
            "removed_checkpoints": o.removed_checkpoints,
            "removed_segments": o.removed_segments,
        });
        let text = format!(
            "checkpoint at seq {} ({}); removed checkpoints: {}; removed WAL segments: {}",
            o.seq,
            if o.written { "written" } else { "already current, nothing written" },
            seqs(&o.removed_checkpoints),
            seqs(&o.removed_segments)
        );
        self.print(value, &text);
    }

    pub fn backup(&self, r: &BackupReport) {
        let value = json!({
            "backup": {
                "path": path(&r.path), "seq": r.seq, "time": time(r.time), "history": r.history.to_string(),
                "checkpoints": r.checkpoints, "segments": r.segments, "bytes": r.bytes,
            }
        });
        let text = format!(
            "backed up to seq {}{} into {}: checkpoints {}, {} WAL segments, {} bytes",
            r.seq,
            r.time.map_or(String::new(), |t| format!(" ({})", t)),
            r.path.display(),
            seqs(&r.checkpoints),
            r.segments.len(),
            r.bytes
        );
        self.print(value, &text);
    }

    pub fn restore(&self, r: &RestoreReport) {
        let value = json!({
            "restore": {
                "path": path(&r.path), "seq": r.seq, "time": time(r.time), "history": r.history.to_string(),
                "source_history": history(r.source_history), "checkpoint": r.checkpoint, "replayed": r.replayed,
                "skipped_checkpoints": r.skipped_checkpoints.iter().map(|s| s.seq).collect::<Vec<_>>(),
                "backup_segments": r.backup_segments, "archive_segments": r.archive_segments,
            }
        });
        let text = format!(
            "restored {} to seq {}{} (new history {}): {} records replayed onto {}, {} segments from the backup, {} from the archive",
            r.path.display(),
            r.seq,
            r.time.map_or(String::new(), |t| format!(" ({})", t)),
            r.history,
            r.replayed,
            r.checkpoint.map_or("an empty store".into(), |c| format!("checkpoint {}", c)),
            r.backup_segments,
            r.archive_segments
        );
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
        for p in &r.problems {
            text += &format!("\n  problem: {}", p);
        }
        for n in &r.notes {
            text += &format!("\n  note: {}", n);
        }
        self.print(value, &text);
    }
}
