//! Writing the log: [`Wal`], its [`FsyncPolicy`] and [`WalOptions`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iwdb_engine::CommitRecord;
use iwdb_engine::metrics::Histogram;

use crate::format::{self, FORMAT_VERSION, FRAME_HEADER_LEN, FrameHeader, SEGMENT_HEADER_LEN};
use crate::io::{LogFile, LogFs, StdFs};
use crate::{Error, reader};
use iwdb_engine::CommitTime;

/// Smallest segment size (1 KiB).
pub const MIN_SEGMENT_SIZE: u64 = 1 << 10;
/// Largest segment size (1 GiB).
pub const MAX_SEGMENT_SIZE: u64 = 1 << 30;
/// Default segment size (64 MiB).
pub const DEFAULT_SEGMENT_SIZE: u64 = 64 << 20;

/// When the log is fsynced. The guarantees are stated in
/// `documentation/guarantees.md` (ADR 0005).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsyncPolicy {
    /// Every append is fsynced before it returns, so before the commit is
    /// applied and acknowledged. An acknowledged commit survives any crash.
    Always,
    /// Group commit, like Redis `appendfsync everysec`. An append fsyncs
    /// the log (itself and every record before it) before it returns when,
    /// counting the record just written, `max_batch` records are unsynced,
    /// or the oldest unsynced record was written `max_delay` or longer ago.
    /// Otherwise it returns without an fsync. [`Wal::sync_due`], called on
    /// a timer by the owner, syncs records older than `max_delay` when no
    /// append comes. After a crash, acknowledged commits can be lost: fewer
    /// than `max_batch` of them, all written within `max_delay` of each
    /// other (with a timer calling `sync_due` every `P`, within
    /// `max_delay + P` of the crash).
    Group { max_delay: Duration, max_batch: u32 },
    /// Never fsync on its own (tests only). A process crash loses nothing
    /// (the data is in the OS page cache), an OS crash or power loss can
    /// lose or damage anything written since the files were created. Only
    /// an explicit [`Wal::sync`] (or [`Wal::close`]... no: `close` doesn't
    /// sync with `Off`) fsyncs: then every segment that may hold unsynced
    /// records, and the directory.
    Off,
}

/// Options of a [`Wal`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalOptions {
    pub fsync: FsyncPolicy,
    /// A segment is closed and a new one started before a record that would
    /// make it larger than this (unless the segment has no records yet).
    /// Between [`MIN_SEGMENT_SIZE`] and [`MAX_SEGMENT_SIZE`].
    pub segment_size: u64,
}

impl Default for WalOptions {
    /// `always`, 64 MiB segments.
    fn default() -> Self {
        WalOptions { fsync: FsyncPolicy::Always, segment_size: DEFAULT_SEGMENT_SIZE }
    }
}

impl WalOptions {
    fn check(&self) -> Result<(), Error> {
        if !(MIN_SEGMENT_SIZE..=MAX_SEGMENT_SIZE).contains(&self.segment_size) {
            return Err(Error::InvalidOptions(format!(
                "segment size {} is not between {} and {}",
                self.segment_size, MIN_SEGMENT_SIZE, MAX_SEGMENT_SIZE
            )));
        }
        if let FsyncPolicy::Group { max_batch: 0, .. } = self.fsync {
            return Err(Error::InvalidOptions("a group commit batch needs at least one record".into()));
        }
        Ok(())
    }
}

/// Whether group commit must fsync now: `unsynced` records (at least one)
/// are unsynced and the oldest was written at `oldest`.
fn group_due(max_delay: Duration, max_batch: u32, unsynced: u64, oldest: Instant, now: Instant) -> bool {
    unsynced >= u64::from(max_batch) || now.saturating_duration_since(oldest) >= max_delay
}

/// The writer of a log directory: appends [`CommitRecord`]s to segment
/// files, fsyncing them according to its [`FsyncPolicy`].
///
/// - It is the only appender (the commit pipeline's single writer), and
///   stores and returns records without knowing what their ops mean.
/// - A new writer always starts a new segment. Segments are created
///   complete: the header is written to a temporary file, synced, renamed
///   into place and the directory synced, so a segment file either has a
///   valid header or doesn't exist. Before rotating, the old segment is
///   synced (except with [`FsyncPolicy::Off`]), so only the last segment
///   can have a torn tail.
/// - If a write, fsync, rename or directory sync fails, the writer is
///   **failed**: it rejects every further append with [`Error::ReadOnly`]
///   until the log is reopened (recovered). A failed fsync is never
///   retried: after one, the OS may have dropped the unsynced pages and
///   marked them clean, so a second fsync can succeed without writing them.
///
/// Appends are O(record size), plus an fsync per the policy.
pub struct Wal<F: LogFs = StdFs> {
    fs: F,
    dir: PathBuf,
    options: WalOptions,
    file: F::File,
    segment_path: PathBuf,
    segment_len: u64,
    segment_records: u64,
    next_seq: u64,
    /// The highest seq whose fsync completed (as far as this writer knows).
    synced_seq: u64,
    /// When the oldest unsynced record was written.
    oldest_unsynced: Option<Instant>,
    failed: Option<String>,
    frame: Vec<u8>,
    /// Bytes of record frames this writer has appended.
    appended: u64,
    /// The commit time of the last record in the log, as far as this
    /// writer knows: the floor for the next one.
    last_time: CommitTime,
    /// How long the segment fsyncs took (the metrics, ADR 0050).
    fsyncs: Arc<Histogram>,
}

impl<F: LogFs> std::fmt::Debug for Wal<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wal")
            .field("dir", &self.dir)
            .field("options", &self.options)
            .field("segment_path", &self.segment_path)
            .field("next_seq", &self.next_seq)
            .field("synced_seq", &self.synced_seq)
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}

impl Wal<StdFs> {
    /// Start writing the log in the existing directory `dir` at `next_seq`,
    /// in a new segment. See [`create_with`](Self::create_with).
    pub fn create(dir: &Path, options: WalOptions, next_seq: u64) -> Result<Self, Error> {
        Self::create_with(StdFs, dir, options, next_seq)
    }
}

impl<F: LogFs> Wal<F> {
    /// Start writing the log in the existing directory `dir` at `next_seq`
    /// (at least 1, below `u64::MAX`), in a new segment, through `fs`.
    ///
    /// The log in `dir` must be empty or end right before `next_seq`, with
    /// no torn tail: in a new directory there is nothing; after a restart,
    /// recovery has read the log to its end and truncated a torn
    /// tail. The last segment is read to check this (O(its size)) and, except
    /// with [`FsyncPolicy::Off`], fsynced: the writer's first record says that
    /// everything before it is synced.
    ///
    /// Errors: [`Error::LogAhead`] if the log has records at or after
    /// `next_seq`; [`Error::LogEndsBefore`] if it ends before `next_seq - 1`;
    /// [`Error::TornTail`] if its last segment has a torn tail; any error
    /// of the reader. A segment at `next_seq` without records (left by a
    /// crash right after a rotation) is replaced.
    pub fn create_with(fs: F, dir: &Path, options: WalOptions, next_seq: u64) -> Result<Self, Error> {
        options.check()?;
        if next_seq == 0 || next_seq == u64::MAX {
            return Err(Error::InvalidOptions(format!("the log can't start at seq {}", next_seq)));
        }
        let mut last_time = CommitTime(i64::MIN);
        if let Some((first_seq, path)) = reader::list_segments(dir)?.pop() {
            if first_seq > next_seq {
                return Err(Error::LogAhead { next_seq, first_seq, path });
            }
            let end = reader::read_segment_file(&path, first_seq, true)?;
            if end.torn.is_some() {
                return Err(Error::TornTail { path, valid_len: end.valid_len });
            }
            if end.next_seq > next_seq {
                return Err(Error::LogAhead { next_seq, first_seq, path });
            }
            if end.next_seq < next_seq {
                return Err(Error::LogEndsBefore { from: next_seq, next_seq: end.next_seq });
            }
            last_time = end.last_time.unwrap_or(last_time);
            if options.fsync != FsyncPolicy::Off {
                let mut file = fs.open_append(&path).map_err(|e| Error::io("open", &path, e))?;
                file.sync().map_err(|e| Error::io("fsync", &path, e))?;
            }
        }
        let (file, segment_path) = new_segment(&fs, dir, next_seq, options.fsync)?;
        // With `Off` the log before was never fsynced (as far as we know)
        let synced_seq = if options.fsync == FsyncPolicy::Off { 0 } else { next_seq - 1 };
        Ok(Wal {
            fs,
            dir: dir.to_path_buf(),
            options,
            file,
            segment_path,
            segment_len: SEGMENT_HEADER_LEN as u64,
            segment_records: 0,
            next_seq,
            synced_seq,
            oldest_unsynced: None,
            failed: None,
            frame: Vec::new(),
            appended: 0,
            last_time,
            fsyncs: Arc::default(),
        })
    }

    /// Append `record` (whose seq must be [`next_seq`](Self::next_seq)) and
    /// fsync per the policy. When this returns `Ok`, the record is written
    /// and, with [`FsyncPolicy::Always`], durable: the commit may be
    /// applied and acknowledged.
    ///
    /// Errors:
    /// - [`Error::RecordTooLarge`], [`Error::Encode`], [`Error::OutOfOrder`]:
    ///   nothing was written, and the log stays usable;
    /// - [`Error::Io`]: a write, fsync or rotation failed. The log is now
    ///   failed. The record may or may not be in the log (it may be found
    ///   after reopening), so the commit's outcome is unknown; it must not
    ///   be applied;
    /// - [`Error::ReadOnly`]: the log failed earlier.
    ///
    /// The record's commit time is the system clock's, but never earlier
    /// than the previous record's ([`CommitTime`]); it is returned.
    pub fn append(&mut self, record: &CommitRecord) -> Result<CommitTime, Error> {
        let time = CommitTime::now().max(self.last_time);
        self.append_at(record, time)?;
        Ok(time)
    }

    /// [`append`](Self::append) with an explicit commit time, written as
    /// given (it need not be later than the previous record's; readers
    /// don't require commit times to be ordered). For fixtures and tests,
    /// and for copying records with their original time.
    pub fn append_at(&mut self, record: &CommitRecord, time: CommitTime) -> Result<(), Error> {
        self.check_usable()?;
        if record.seq != self.next_seq {
            return Err(Error::OutOfOrder { expected: self.next_seq, found: record.seq });
        }
        if record.seq == u64::MAX {
            return Err(iwdb_engine::Error::SeqExhausted.into());
        }
        let (kind, payload) = format::encode_payload(record)?;
        let frame_len = (FRAME_HEADER_LEN + payload.len()) as u64;
        let span =
            crate::trace_span!("iwdb.wal.append", iwdb.wal.bytes = frame_len, iwdb.wal.synced = tracing::field::Empty);
        let _entered = span.enter();
        if self.segment_records > 0 && self.segment_len + frame_len > self.options.segment_size {
            self.rotate()?;
        }
        self.frame.clear();
        let header = FrameHeader { seq: record.seq, synced_seq: self.synced_seq, time: time.0, kind };
        format::encode_frame(&mut self.frame, FORMAT_VERSION, header, &payload);
        if let Err(e) = self.file.write_all(&self.frame) {
            return Err(self.fail("append", e));
        }
        self.segment_len += frame_len;
        self.appended += frame_len;
        self.segment_records += 1;
        self.next_seq += 1;
        self.last_time = self.last_time.max(time);
        let now = Instant::now();
        let oldest = *self.oldest_unsynced.get_or_insert(now);
        let due = match self.options.fsync {
            FsyncPolicy::Always => true,
            FsyncPolicy::Group { max_delay, max_batch } => {
                group_due(max_delay, max_batch, self.unsynced(), oldest, now)
            }
            FsyncPolicy::Off => false,
        };
        if due {
            self.traced_sync()?;
        }
        // Recorded once: an exporter may keep both values of a field set twice
        span.record("iwdb.wal.synced", due);
        Ok(())
    }

    /// Fsync every record written so far (whatever the policy), unless
    /// they are synced already. On error the log is failed.
    pub fn sync(&mut self) -> Result<(), Error> {
        self.check_usable()?;
        if self.unsynced() > 0 {
            self.traced_sync()?;
        }
        Ok(())
    }

    /// With [`FsyncPolicy::Group`]: fsync if the oldest unsynced record was
    /// written `max_delay` or longer ago. The owner calls this on a timer,
    /// so that the last commits before an idle period become durable
    /// without waiting for the next append. Returns whether it synced.
    pub fn sync_due(&mut self) -> Result<bool, Error> {
        self.check_usable()?;
        let (FsyncPolicy::Group { max_delay, .. }, Some(oldest)) = (self.options.fsync, self.oldest_unsynced) else {
            return Ok(false);
        };
        if Instant::now().saturating_duration_since(oldest) < max_delay {
            return Ok(false);
        }
        self.sync_now()?;
        Ok(true)
    }

    /// Sync (except with [`FsyncPolicy::Off`]) and close. Dropping a `Wal`
    /// without closing it doesn't sync.
    pub fn close(mut self) -> Result<(), Error> {
        if self.options.fsync != FsyncPolicy::Off {
            self.sync()?;
        }
        Ok(())
    }

    /// The seq the next record must have.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// The highest seq known to be durable: every record up to it is
    /// synced. Records after it are written but may be lost in a crash.
    pub fn synced_seq(&self) -> u64 {
        self.synced_seq
    }

    /// How long this writer's fsyncs of its segment took: shared, so it
    /// can be read without the writer's lock.
    pub fn fsyncs(&self) -> Arc<Histogram> {
        self.fsyncs.clone()
    }

    /// Bytes of record frames appended by this writer since it was
    /// created (the log's growth; the checkpointer's size trigger).
    pub fn appended_bytes(&self) -> u64 {
        self.appended
    }

    /// Why the log failed, if it did (it accepts no more appends then).
    pub fn failure(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn options(&self) -> &WalOptions {
        &self.options
    }

    /// The segment being written.
    pub fn segment_path(&self) -> &Path {
        &self.segment_path
    }

    fn unsynced(&self) -> u64 {
        self.next_seq - 1 - self.synced_seq
    }

    fn check_usable(&self) -> Result<(), Error> {
        match &self.failed {
            Some(cause) => Err(Error::ReadOnly { cause: cause.clone() }),
            None => Ok(()),
        }
    }

    /// [`sync_now`](Self::sync_now) in a trace span (ADR 0057): a commit's
    /// own fsync, or an explicit [`sync`](Self::sync). The timer's
    /// [`sync_due`](Self::sync_due) isn't traced (a trace per interval).
    fn traced_sync(&mut self) -> Result<(), Error> {
        let policy = match self.options.fsync {
            FsyncPolicy::Always => "always",
            FsyncPolicy::Group { .. } => "group",
            FsyncPolicy::Off => "off",
        };
        let span = crate::trace_span!("iwdb.wal.fsync", iwdb.wal.fsync_policy = policy, iwdb.wal.batch = self.unsynced());
        let _entered = span.enter();
        self.sync_now()
    }

    /// Fsync the current segment. With `Off`, first every other segment
    /// that may hold records after `synced_seq` (rotations and earlier
    /// writers didn't sync them), and afterwards the directory (segments
    /// were created without a directory sync). Never retried on failure
    /// (see the type docs).
    fn sync_now(&mut self) -> Result<(), Error> {
        if self.options.fsync == FsyncPolicy::Off
            && let Err(e) = self.sync_older_segments()
        {
            self.failed = Some(e.to_string());
            return Err(e);
        }
        let start = Instant::now();
        let synced = self.file.sync();
        self.fsyncs.observe(start.elapsed());
        if let Err(e) = synced {
            return Err(self.fail("fsync", e));
        }
        if self.options.fsync == FsyncPolicy::Off
            && let Err(e) = self.fs.sync_dir(&self.dir)
        {
            let error = Error::io("sync directory", &self.dir, e);
            self.failed = Some(error.to_string());
            return Err(error);
        }
        self.synced_seq = self.next_seq - 1;
        self.oldest_unsynced = None;
        Ok(())
    }

    /// With `Off`: fsync the segments before the current one that hold
    /// records after `synced_seq`. A segment's records end where the next
    /// segment begins. One that is gone was removed by the checkpointer
    /// after a checkpoint that covers it.
    fn sync_older_segments(&mut self) -> Result<(), Error> {
        let segments = reader::list_segments(&self.dir)?;
        for [(_, path), (next_first, _)] in segments.array_windows() {
            if *next_first > self.synced_seq + 1 && *path != self.segment_path {
                let mut file = match self.fs.open_append(path) {
                    Ok(file) => file,
                    // The checkpointer removed it meanwhile: a checkpoint,
                    // fsynced by `write_atomic`, holds its records
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(Error::io("open", path, e)),
                };
                file.sync().map_err(|e| Error::io("fsync", path, e))?;
            }
        }
        Ok(())
    }

    /// Close the current segment (synced, except with `Off`) and start one
    /// at `next_seq`.
    fn rotate(&mut self) -> Result<(), Error> {
        if self.options.fsync != FsyncPolicy::Off && self.unsynced() > 0 {
            self.sync_now()?;
        }
        match new_segment(&self.fs, &self.dir, self.next_seq, self.options.fsync) {
            Ok((file, path)) => {
                self.file = file;
                self.segment_path = path;
                self.segment_len = SEGMENT_HEADER_LEN as u64;
                self.segment_records = 0;
                Ok(())
            }
            Err(e) => {
                self.failed = Some(e.to_string());
                Err(e)
            }
        }
    }

    /// Mark the log failed after an I/O error on the current segment.
    fn fail(&mut self, op: &'static str, source: std::io::Error) -> Error {
        let error = Error::io(op, &self.segment_path, source);
        self.failed = Some(error.to_string());
        error
    }
}

/// Create the segment starting at `first_seq`: header into a temporary
/// file, fsync, rename into place, directory fsync (no fsyncs with
/// [`FsyncPolicy::Off`]); then open it for appending.
fn new_segment<F: LogFs>(fs: &F, dir: &Path, first_seq: u64, policy: FsyncPolicy) -> Result<(F::File, PathBuf), Error> {
    let name = format::segment_name(first_seq);
    let path = dir.join(&name);
    let tmp = dir.join(format!("{}.tmp", name));
    let sync = policy != FsyncPolicy::Off;
    let mut file = fs.create(&tmp).map_err(|e| Error::io("create", &tmp, e))?;
    file.write_all(&format::encode_segment_header(first_seq)).map_err(|e| Error::io("append", &tmp, e))?;
    if sync {
        file.sync().map_err(|e| Error::io("fsync", &tmp, e))?;
    }
    drop(file);
    fs.rename(&tmp, &path).map_err(|e| Error::io("rename", &tmp, e))?;
    if sync {
        fs.sync_dir(dir).map_err(|e| Error::io("sync directory", dir, e))?;
    }
    let file = fs.open_append(&path).map_err(|e| Error::io("open", &path, e))?;
    Ok((file, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_commit_is_due_by_count_or_age() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        assert!(!group_due(ms(10), 3, 1, t0, t0));
        assert!(!group_due(ms(10), 3, 2, t0, t0 + ms(9)));
        assert!(group_due(ms(10), 3, 3, t0, t0));
        assert!(group_due(ms(10), 3, 1, t0, t0 + ms(10)));
        assert!(group_due(Duration::ZERO, 100, 1, t0, t0));
        assert!(group_due(ms(10), 1, 1, t0, t0));
        // A clock that goes backwards counts as no time passed
        assert!(!group_due(ms(10), 3, 1, t0 + ms(5), t0));
    }

    #[test]
    fn options_are_checked() {
        let options = |fsync, segment_size| WalOptions { fsync, segment_size }.check();
        assert!(options(FsyncPolicy::Always, DEFAULT_SEGMENT_SIZE).is_ok());
        assert!(options(FsyncPolicy::Always, MIN_SEGMENT_SIZE - 1).is_err());
        assert!(options(FsyncPolicy::Always, MAX_SEGMENT_SIZE + 1).is_err());
        assert!(options(FsyncPolicy::Group { max_delay: Duration::ZERO, max_batch: 0 }, MIN_SEGMENT_SIZE).is_err());
        assert!(options(FsyncPolicy::Group { max_delay: Duration::ZERO, max_batch: 1 }, MIN_SEGMENT_SIZE).is_ok());
    }
}
