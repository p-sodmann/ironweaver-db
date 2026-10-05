//! Where the server's audit entries go (step 15c, ADR 0049).
//!
//! [`LogAudit`] writes each [`AuditEntry`] as a `tracing` event of target
//! [`TARGET`] (`iwdb::audit`), at `info`: one line of the normal log
//! (JSON or text, ADR 0042), which [`crate::logging`] lets through
//! whatever `[log] level` says unless the level names the target itself.
//!
//! With `[audit] dir`, [`AuditFiles`] also writes them as JSON lines to a
//! file per UTC day, `audit-YYYY-MM-DD.jsonl` (mode 0600), and deletes the
//! files older than `[audit] retention_days` at start and at each new day.
//!
//! The entries are best-effort: written after the outcome, without fsync.
//! A crash can lose the last ones; the changes they record are in the WAL.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use iwdb_query::audit::{AuditEntry, AuditSink};
use tracing_subscriber::fmt::MakeWriter;

/// The `tracing` target of audit entries.
pub const TARGET: &str = "iwdb::audit";

/// Audit entries as `tracing` events of target [`TARGET`]: the server's
/// default sink. Fields: `operation`, `outcome` (`success` or `failure`),
/// `code`, `user`, `auth` (`session`, `api_token`, `certificate`, `off`),
/// `client`, `namespace`, `subject`, `token_name`, `role`, `admin`, `seq`,
/// `namespace_event`; the absent ones are left out.
pub struct LogAudit;

impl AuditSink for LogAudit {
    fn record(&self, e: &AuditEntry) {
        tracing::info!(
            target: "iwdb::audit",
            operation = e.operation.map(|o| o.name()),
            outcome = e.outcome(),
            code = e.code.map(|c| c.as_str()),
            user = e.user.as_deref(),
            auth = e.via.map(|v| v.as_str()),
            client = e.client.map(tracing::field::display),
            namespace = e.namespace.as_deref(),
            subject = e.subject.as_deref(),
            token_name = e.token_name.as_deref(),
            role = e.role.map(|r| r.as_str()),
            admin = e.admin,
            seq = e.seq,
            namespace_event = e.namespace_event,
            "audit"
        );
    }
}

/// The day of `time`, in days since the Unix epoch (UTC).
fn day_of(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() / 86_400) as i64,
        Err(e) => -((e.duration().as_secs() / 86_400) as i64) - 1,
    }
}

/// `(year, month, day)` of a day since the Unix epoch (Howard Hinnant's
/// `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The inverse of [`civil`].
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The file name of `day`'s entries.
fn file_name(day: i64) -> String {
    let (y, m, d) = civil(day);
    format!("audit-{:04}-{:02}-{:02}.jsonl", y, m, d)
}

/// The day of an audit file's name; `None` for any other name.
fn day_of_name(name: &str) -> Option<i64> {
    let date = name.strip_prefix("audit-")?.strip_suffix(".jsonl")?;
    let mut parts = date.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let (y, m, d) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    let day = days_from_civil(y, m, d);
    (civil(day) == (y, m, d)).then_some(day)
}

struct State {
    dir: PathBuf,
    /// Days to keep (today included); 0: every file.
    retention_days: u32,
    /// The day `file` is of.
    day: i64,
    file: Option<File>,
}

impl State {
    /// The file of `today`, opened (and old files deleted) on a new day.
    fn file(&mut self, today: i64) -> io::Result<&mut File> {
        if self.day != today || self.file.is_none() {
            let path = self.dir.join(file_name(today));
            let mut options = OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            self.file = Some(options.open(&path)?);
            self.day = today;
            // A failure to delete an old file doesn't stop the audit log
            let _ = prune(&self.dir, self.retention_days, today);
        }
        self.file.as_mut().ok_or_else(|| io::Error::other("the audit file isn't open"))
    }
}

/// Delete the audit files of days before the last `retention_days`.
/// Returns how many it deleted.
fn prune(dir: &Path, retention_days: u32, today: i64) -> io::Result<usize> {
    if retention_days == 0 {
        return Ok(0);
    }
    let oldest = today - i64::from(retention_days) + 1;
    let mut deleted = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Some(day) = entry.file_name().to_str().and_then(day_of_name) else { continue };
        if day < oldest && entry.file_type()?.is_file() {
            fs::remove_file(entry.path())?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// The audit log's files (`[audit] dir`): a `MakeWriter` for the JSON
/// layer that writes the `iwdb::audit` events.
#[derive(Clone)]
pub struct AuditFiles {
    state: Arc<Mutex<State>>,
}

impl AuditFiles {
    /// Open today's file in `dir` (made if missing, mode 0700) and delete
    /// the files older than `retention_days` (0: keep them all). Errors
    /// name the directory.
    pub fn open(dir: &Path, retention_days: u32) -> Result<Self, String> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir).map_err(|e| format!("[audit] dir {}: {}", dir.display(), e))?;
        let mut state = State { dir: dir.to_owned(), retention_days, day: 0, file: None };
        state
            .file(day_of(SystemTime::now()))
            .map_err(|e| format!("[audit] dir {}: opening today's file: {}", dir.display(), e))?;
        Ok(AuditFiles { state: Arc::new(Mutex::new(state)) })
    }
}

/// One event's writer: holds the files' lock until the line is written.
pub struct AuditWriter<'a>(MutexGuard<'a, State>);

impl Write for AuditWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.file(day_of(SystemTime::now()))?.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.0.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

impl<'a> MakeWriter<'a> for AuditFiles {
    type Writer = AuditWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        // A writer that panicked while holding the lock left the state
        // intact (a file handle and a day)
        AuditWriter(self.state.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn days_and_names_round_trip() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(file_name(20_365), "audit-2025-10-04.jsonl");
        for day in [0, 1, 59, 60, 11_016, 20_365, 2_932_896] {
            assert_eq!(day_of_name(&file_name(day)), Some(day), "{}", day);
        }
        for bad in
            ["audit-2025-02-30.jsonl", "audit-2025-1-04.jsonl", "audit-2025-10-04.log", "x.jsonl", "audit-.jsonl"]
        {
            assert_eq!(day_of_name(bad), None, "{}", bad);
        }
        assert_eq!(day_of(UNIX_EPOCH), 0);
    }

    #[test]
    fn old_files_are_deleted_and_others_kept() {
        let dir = tempfile::tempdir().unwrap();
        let today = day_of(SystemTime::now());
        for age in [0, 1, 29, 30, 31, 400] {
            fs::write(dir.path().join(file_name(today - age)), "{}\n").unwrap();
        }
        fs::write(dir.path().join("notes.txt"), "kept").unwrap();
        let files = AuditFiles::open(dir.path(), 30).unwrap();
        let mut names: Vec<String> =
            fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        let mut expected: Vec<String> = [29, 1, 0].iter().map(|age| file_name(today - age)).collect();
        expected.push("notes.txt".into());
        expected.sort();
        assert_eq!(names, expected);
        // Writing appends to today's file
        files.make_writer().write_all(b"{\"a\":1}\n").unwrap();
        let text = fs::read_to_string(dir.path().join(file_name(today))).unwrap();
        assert_eq!(text, "{}\n{\"a\":1}\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(file_name(today))).unwrap().permissions().mode();
            // A file made before (here by the test) keeps its mode; a new one is 0600
            let fresh = tempfile::tempdir().unwrap();
            AuditFiles::open(fresh.path(), 0).unwrap();
            let new = fs::metadata(fresh.path().join(file_name(today))).unwrap().permissions().mode();
            assert_eq!(new & 0o777, 0o600, "{:o} {:o}", new, mode);
        }
    }

    #[test]
    fn retention_zero_keeps_every_file() {
        let dir = tempfile::tempdir().unwrap();
        let today = day_of(SystemTime::now());
        fs::write(dir.path().join(file_name(today - 5000)), "").unwrap();
        AuditFiles::open(dir.path(), 0).unwrap();
        assert!(dir.path().join(file_name(today - 5000)).exists());
    }
}
