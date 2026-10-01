//! Recovery: open a data directory and rebuild its namespace from the
//! newest usable checkpoint and the WAL. The procedure and its error cases
//! are described in `documentation/formats/data-dir.md`.

use std::path::{Path, PathBuf};

use iwdb_engine::catalog::{IndexChanges, NamespaceName};

use crate::checkpoint::{load_newest, SkippedCheckpoint};
use crate::format::Damage;
use crate::io::LogFs;
use crate::layout::DataDir;
use crate::{Error, LoggedNamespace, Wal, WalOptions, WalReader};

/// What recovery found and did. Recovery reports these instead of logging
/// them; the store logs the unusual ones.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// The data directory was created (or initialized) by this open.
    pub created: bool,
    /// The layout version the directory was upgraded from by this open
    /// (`Some(1)` for a step 5 directory), if it was.
    pub upgraded_from: Option<u32>,
    /// Stale temporary files that were removed.
    pub removed_temp_files: Vec<PathBuf>,
    /// The seq of the checkpoint recovery started from, `None` if it
    /// started from an empty namespace.
    pub checkpoint: Option<u64>,
    /// Newer checkpoints that failed to load, newest first.
    pub skipped_checkpoints: Vec<SkippedCheckpoint>,
    /// How the loaded checkpoint's saved indexes differed from its catalog
    /// (always empty for checkpoints the database wrote).
    pub index_changes: IndexChanges,
    /// WAL records replayed on top of the checkpoint.
    pub replayed: u64,
    /// The torn tail that was cut off the log, if any.
    pub torn_tail: Option<CutTail>,
    /// The seq the namespace was recovered to.
    pub seq: u64,
}

/// A torn tail cut off the last WAL segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CutTail {
    pub path: PathBuf,
    /// The segment's length before, and after (0: the file was removed).
    pub file_len: u64,
    pub valid_len: u64,
    pub damage: Damage,
    /// Complete frames that were discarded with the tail: records written
    /// after the damaged one and before it was synced (possible only with
    /// the `group` or `off` fsync policy, after an OS crash). They were
    /// never acknowledged as durable, and the damaged record precedes them.
    pub discarded_frames: u64,
    /// The segment had no valid header and was removed.
    pub removed: bool,
}

/// A recovered store: its directory (holding the lock), its namespace with
/// a new WAL writer, and what recovery did.
#[derive(Debug)]
pub struct Recovered<F: LogFs> {
    pub dir: DataDir,
    pub namespace: LoggedNamespace<F>,
    pub report: RecoveryReport,
}

/// Open the data directory `root` and recover its namespace `name`:
///
/// 1. open the directory and take its lock ([`DataDir::open`]); with
///    `create`, a new directory is initialized;
/// 2. remove stale temporary files;
/// 3. load the newest checkpoint that loads, falling back to older ones
///    (or to an empty namespace) on a checksum or format error; the
///    indexes are rebuilt from its catalog;
/// 4. replay the WAL from the checkpoint's seq + 1 to its end;
/// 5. cut a torn tail off the last segment at its valid length and fsync
///    it (remove the segment if not even its header is valid);
/// 6. upgrade a layout 1 or 2 directory to the current layout (a new marker
///    with a history id, [`DataDir::upgrade`]);
/// 7. start a WAL writer at the log's next seq, in a new segment.
///
/// The result is the state after every commit in the log, which includes
/// every acknowledged commit that the fsync policy made durable.
///
/// Errors. Nothing is truncated or deleted in any of these cases, apart
/// from temporary files, and the lock is released:
/// - the directory: [`Error::NotADataDir`], [`Error::InvalidDataDir`],
///   [`Error::UnsupportedLayout`], [`Error::IsBackup`],
///   [`Error::InterruptedRestore`], [`Error::Locked`];
/// - [`Error::NoUsableCheckpoint`]: the WAL doesn't reach back to the
///   newest checkpoint that loads (the newer ones are damaged and their
///   records were cut from the WAL);
/// - [`Error::LogEndsBefore`]: the WAL ends before the checkpoint's seq
///   (possible only after an OS crash with the `off` policy, or if WAL
///   files were removed);
/// - WAL corruption: [`Error::Corrupt`], [`Error::InvalidRecord`],
///   [`Error::SeqMismatch`], [`Error::HeaderMismatch`],
///   [`Error::UnsupportedVersion`], [`Error::SegmentTooLarge`];
/// - [`Error::ReplayFailed`]: a logged record fails to apply
///   (`ApplyFailed`, including `GraphError::Internal`), a bug; report it;
/// - [`Error::Io`], including a failed truncation.
pub fn recover<F: LogFs>(
    fs: F,
    root: &Path,
    create: bool,
    name: &NamespaceName,
    wal_options: WalOptions,
) -> Result<Recovered<F>, Error> {
    let (mut dir, created) = DataDir::open(&fs, root, create)?;
    let removed_temp_files = dir.remove_temp_files(&fs)?;

    let base = load_newest(dir.checkpoint_dir(), name)?;
    let mut namespace = base.namespace;
    let from = namespace.seq() + 1;
    let mut reader = WalReader::open(dir.wal_dir(), from).map_err(|e| match e {
        Error::MissingRecords { from, first_seq } => {
            Error::NoUsableCheckpoint { from, first_seq, skipped: base.skipped.clone() }
        }
        other => other,
    })?;
    let mut replayed = 0;
    while let Some(record) = reader.next_timed() {
        let (record, time) = record?;
        let seq = record.seq;
        namespace.replay(record, time).map_err(|source| Error::ReplayFailed { seq, source })?;
        replayed += 1;
    }
    let end = reader.end().cloned().ok_or(Error::LogEndsBefore { from, next_seq: namespace.seq() + 1 })?;

    let mut torn_tail = None;
    if let Some(segment) = &end.last_segment {
        if let Some(torn) = &segment.torn {
            let removed = segment.valid_len == 0;
            if removed {
                fs.remove_file(&segment.path).map_err(|e| Error::io("remove", &segment.path, e))?;
                fs.sync_dir(dir.wal_dir()).map_err(|e| Error::io("sync directory", dir.wal_dir(), e))?;
            } else {
                fs.truncate(&segment.path, segment.valid_len).map_err(|e| Error::io("truncate", &segment.path, e))?;
            }
            torn_tail = Some(CutTail {
                path: segment.path.clone(),
                file_len: segment.file_len,
                valid_len: segment.valid_len,
                damage: torn.damage,
                discarded_frames: torn.discarded_frames,
                removed,
            });
        }
    }

    let upgraded_from = dir.upgrade(&fs)?;
    let seq = namespace.seq();
    let wal = Wal::create_with(fs, dir.wal_dir(), wal_options, end.next_seq)?;
    let namespace = LoggedNamespace::new(namespace, wal)?;
    let report = RecoveryReport {
        created,
        upgraded_from,
        removed_temp_files,
        checkpoint: base.checkpoint,
        skipped_checkpoints: base.skipped,
        index_changes: base.index_changes,
        replayed,
        torn_tail,
        seq,
    };
    Ok(Recovered { dir, namespace, report })
}
