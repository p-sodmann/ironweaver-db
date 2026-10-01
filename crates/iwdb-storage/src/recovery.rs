//! Recovery: open a data directory and rebuild its namespace from the
//! newest usable checkpoint and the WAL. The procedure and its error cases
//! are described in `documentation/formats/data-dir.md`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use iwdb_engine::catalog::{IndexChanges, NamespaceName};
use iwdb_engine::Namespace;

use crate::checkpoint::{load_newest, SkippedCheckpoint};
use crate::format::Damage;
use crate::io::LogFs;
use crate::layout::{DataDir, NsPaths};
use crate::namespaces::{read_log, CutLog, NamespaceInfo, NamespaceLog, DEFAULT_NAME, NAMESPACES_NAME};
use crate::{Error, LoggedNamespace, Wal, WalOptions, WalReader};

/// What recovery found and did to one namespace. Recovery reports these
/// instead of logging them; the store logs the unusual ones.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
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

/// A namespace read from its checkpoints and WAL, before its WAL writer
/// starts (the layout 1 to 3 upgrade moves its files in between).
#[derive(Debug)]
pub struct ReadNamespace {
    namespace: Namespace,
    next_seq: u64,
    checkpoint: Option<u64>,
    skipped: Vec<SkippedCheckpoint>,
    index_changes: IndexChanges,
    replayed: u64,
    torn_tail: Option<CutTail>,
}

/// Recover the namespace `name` in `paths`: load the newest checkpoint
/// that loads (falling back to older ones, or to an empty namespace, on a
/// checksum or format error; the indexes are rebuilt from its catalog),
/// replay the WAL from the checkpoint's seq + 1 to its end, and cut a torn
/// tail off the last segment at its valid length and fsync it (remove the
/// segment if not even its header is valid).
///
/// Errors. Nothing is truncated or deleted in any of these cases:
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
pub fn read_namespace<F: LogFs>(fs: &F, paths: &NsPaths, name: &NamespaceName) -> Result<ReadNamespace, Error> {
    let base = load_newest(&paths.checkpoints, name)?;
    let mut namespace = base.namespace;
    let from = namespace.seq() + 1;
    let mut reader = WalReader::open(&paths.wal, from).map_err(|e| match e {
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
                fs.sync_dir(&paths.wal).map_err(|e| Error::io("sync directory", &paths.wal, e))?;
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
    Ok(ReadNamespace {
        namespace,
        next_seq: end.next_seq,
        checkpoint: base.checkpoint,
        skipped: base.skipped,
        index_changes: base.index_changes,
        replayed,
        torn_tail,
    })
}

/// Start the WAL writer of a namespace [`read_namespace`] has read, at
/// the log's next seq, in a new segment in `paths.wal`.
pub fn start_namespace<F: LogFs>(
    fs: F,
    paths: &NsPaths,
    read: ReadNamespace,
    wal_options: WalOptions,
) -> Result<(LoggedNamespace<F>, RecoveryReport), Error> {
    let ReadNamespace { namespace, next_seq, checkpoint, skipped, index_changes, replayed, torn_tail } = read;
    let seq = namespace.seq();
    let wal = Wal::create_with(fs, &paths.wal, wal_options, next_seq)?;
    let logged = LoggedNamespace::new(namespace, wal)?;
    let report = RecoveryReport { checkpoint, skipped_checkpoints: skipped, index_changes, replayed, torn_tail, seq };
    Ok((logged, report))
}

/// A namespace of a recovered store.
#[derive(Debug)]
pub struct RecoveredNamespace<F: LogFs> {
    pub info: NamespaceInfo,
    pub paths: NsPaths,
    pub namespace: LoggedNamespace<F>,
    pub report: RecoveryReport,
}

/// A recovered store: its directory (holding the lock), its namespace log,
/// its namespaces each with a new WAL writer, and what recovery did.
#[derive(Debug)]
pub struct Recovered<F: LogFs> {
    pub dir: DataDir,
    pub log: NamespaceLog<F>,
    pub namespaces: Vec<RecoveredNamespace<F>>,
    pub report: StoreRecovery,
}

/// What recovering a store did, besides what it did to each namespace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreRecovery {
    /// The data directory was created (or initialized) by this open.
    pub created: bool,
    /// The layout version the directory was upgraded from by this open
    /// (`Some(3)` for a step 8 directory), if it was.
    pub upgraded_from: Option<u32>,
    /// Stale temporary files that were removed.
    pub removed_temp_files: Vec<PathBuf>,
    /// Namespace directories that the namespace log doesn't list, which
    /// were removed (a crash between making a directory and logging its
    /// creation, or between logging a drop and removing it).
    pub removed_orphans: Vec<u64>,
    /// The torn tail that was cut off the namespace log, if any.
    pub cut_log: Option<CutLog>,
    /// What recovery did to each namespace, by name.
    pub namespaces: BTreeMap<String, RecoveryReport>,
}

impl StoreRecovery {
    /// The report of namespace `name`.
    pub fn namespace(&self, name: &str) -> Option<&RecoveryReport> {
        self.namespaces.get(name)
    }
}

/// Open the data directory `root` and recover its namespaces:
///
/// 1. open the directory and take its lock ([`DataDir::open`]); with
///    `create`, a new directory is initialized;
/// 2. read the namespace log (cutting a torn tail), remove stale temporary
///    files, and remove namespace directories it doesn't list;
/// 3. recover each namespace ([`read_namespace`]), and start its WAL
///    writer ([`start_namespace`]);
/// 4. in a layout 1 to 3 directory, between reading its one namespace and
///    starting its writer, upgrade it to layout 4 ([`DataDir::upgrade`]).
///
/// The result is the state after every commit in each namespace's log,
/// which includes every acknowledged commit that the fsync policy made
/// durable.
///
/// Errors. Nothing is truncated or deleted in any of these cases, apart
/// from temporary files and torn tails, and the lock is released:
/// - the directory: [`Error::NotADataDir`], [`Error::InvalidDataDir`],
///   [`Error::UnsupportedLayout`], [`Error::IsBackup`],
///   [`Error::InterruptedRestore`], [`Error::Locked`];
/// - the namespace log: [`Error::InvalidNamespaceLog`];
/// - a namespace the log lists whose directory is missing or incomplete:
///   [`Error::NamespaceDamaged`];
/// - those of [`read_namespace`].
pub fn recover<F: LogFs + Clone>(
    fs: F,
    root: &Path,
    create: bool,
    wal_options: WalOptions,
) -> Result<Recovered<F>, Error> {
    let (mut dir, created) = DataDir::open(&fs, root, create)?;
    let mut report = StoreRecovery { created, ..StoreRecovery::default() };
    let mut namespaces = Vec::new();
    let log;
    if dir.needs_upgrade().is_some() {
        let name = NamespaceName::new(DEFAULT_NAME).map_err(iwdb_engine::Error::from)?;
        let legacy = dir.legacy_paths();
        report.removed_temp_files = dir.remove_temp_files(&fs, std::slice::from_ref(&legacy))?;
        let read = read_namespace(&fs, &legacy, &name)?;
        report.upgraded_from = dir.upgrade(&fs)?;
        let (opened, cut) = NamespaceLog::open(fs.clone(), root)?;
        report.cut_log = cut;
        log = opened;
        let info = log.table().get(&name).cloned().ok_or_else(|| Error::InvalidNamespaceLog {
            path: log.path().to_path_buf(),
            reason: "no 'default'".into(),
        })?;
        let paths = dir.ns_paths(info.id);
        let (live, ns_report) = start_namespace(fs.clone(), &paths, read, wal_options)?;
        report.namespaces.insert(name.to_string(), ns_report.clone());
        namespaces.push(RecoveredNamespace { info, paths, namespace: live, report: ns_report });
    } else {
        // What the log says, before a torn tail is cut: a directory with data
        // that the log doesn't list is refused, with the log still as it was
        let (_, table) = read_log(&root.join(NAMESPACES_NAME))?;
        let orphans = dir.plan_orphans(&table)?;
        let (opened, cut) = NamespaceLog::open(fs.clone(), root)?;
        report.cut_log = cut;
        log = opened;
        let infos: Vec<NamespaceInfo> = log.table().live().cloned().collect();
        let paths: Vec<NsPaths> = infos.iter().map(|i| dir.ns_paths(i.id)).collect();
        report.removed_temp_files = dir.remove_temp_files(&fs, &paths)?;
        dir.remove_orphans(&fs, &orphans)?;
        report.removed_orphans = orphans;
        let mut reads = Vec::new();
        for (info, paths) in infos.iter().zip(&paths) {
            for sub in [&paths.checkpoints, &paths.wal] {
                if !sub.is_dir() {
                    return Err(Error::NamespaceDamaged {
                        id: info.id,
                        name: info.name.to_string(),
                        reason: format!("'{}' is missing", sub.display()),
                    });
                }
            }
            reads.push(read_namespace(&fs, paths, &info.name)?);
        }
        for ((info, paths), read) in infos.into_iter().zip(paths).zip(reads) {
            let (live, ns_report) = start_namespace(fs.clone(), &paths, read, wal_options.clone())?;
            report.namespaces.insert(info.name.to_string(), ns_report.clone());
            namespaces.push(RecoveredNamespace { info, paths, namespace: live, report: ns_report });
        }
    }
    Ok(Recovered { dir, log, namespaces, report })
}

/// Single-namespace convenience: field access to the `default`
/// namespace's [`RecoveryReport`] (an empty one if it has none).
impl std::ops::Deref for StoreRecovery {
    type Target = RecoveryReport;
    fn deref(&self) -> &RecoveryReport {
        static EMPTY: std::sync::OnceLock<RecoveryReport> = std::sync::OnceLock::new();
        self.namespaces.get(DEFAULT_NAME).unwrap_or_else(|| EMPTY.get_or_init(Default::default))
    }
}
