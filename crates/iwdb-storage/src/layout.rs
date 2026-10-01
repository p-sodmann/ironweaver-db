//! The data directory: its layout, marker and exclusive lock. The normative
//! description is `documentation/formats/data-dir.md`.
//!
//! ```text
//! <dir>/
//!   IWDB           marker: magic, layout version, history id, CRC32C (32 bytes; 16 in layout 1)
//!   LOCK           held with an exclusive lock while a store has the directory open
//!   NAMESPACES     the namespace log (see `namespaces`)
//!   ns/<id, 20 digits>/
//!     checkpoints/   <seq, 20 digits>.ckpt   (see `checkpoint`)
//!     wal/           <first seq, 20 digits>.wal   (see `documentation/formats/wal.md`)
//!   BACKUP         only in a backup: its manifest (`documentation/formats/backup.md`)
//!   RESTORING      only while a restore writes the directory
//! ```
//!
//! Layouts 1 to 3 had one namespace, with `checkpoints/` and `wal/` in the
//! directory itself; they are read as they are and upgraded when a store
//! opens them (ADR 0017).

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::history::HistoryId;
use crate::io::LogFs;
use crate::namespaces::{
    ns_dir_name, parse_ns_dir_name, read_log, write_whole, Event, EventKind, NamespaceTable, DEFAULT_ID, DEFAULT_NAME,
    NAMESPACES_NAME, NS_DIR,
};
use crate::Error;
use iwdb_engine::catalog::NamespaceName;
use iwdb_engine::CommitTime;

/// The layout version this version writes. It reads layouts 1 to 3 too,
/// and upgrades them when it opens a store (see [`DataDir::open`]).
/// Layout 3 (step 8) adds the idempotency key table (`iwdb.keys`) to the
/// checkpoints' graph meta; its marker is layout 2's with version 3.
/// Layout 4 (step 9) puts each namespace in `ns/<id>/` and adds the
/// namespace log `NAMESPACES`; its marker is layout 3's with version 4.
pub const LAYOUT_VERSION: u32 = 4;
/// The marker file's name.
pub const MARKER_NAME: &str = "IWDB";
/// The lock file's name.
pub const LOCK_NAME: &str = "LOCK";
/// The checkpoint directory's name.
pub const CHECKPOINT_DIR: &str = "checkpoints";
/// The WAL directory's name.
pub const WAL_DIR: &str = "wal";
/// A backup's manifest. A directory with it is a backup, which a store
/// refuses to open: it is restored instead.
pub const BACKUP_NAME: &str = "BACKUP";
/// Present while a restore writes a directory. A directory with it is an
/// interrupted restore, which a store refuses to open.
pub const RESTORING_NAME: &str = "RESTORING";
/// The first 8 bytes of the marker.
pub const MARKER_MAGIC: [u8; 8] = *b"IWDBDIR\n";
/// Length of the marker file (layouts 2 to 4).
pub const MARKER_LEN: usize = 32;
/// Length of a layout 1 marker.
pub const MARKER_LEN_V1: usize = 16;
/// Suffix of temporary files (a checkpoint, marker or segment being
/// written). Any file with it is stale when a store opens.
pub const TEMP_SUFFIX: &str = ".tmp";

/// A marker of layout `version`: magic, version (u32 LE), `body`, and the
/// CRC32C of everything before it. Every layout keeps this frame, so that a
/// reader can tell a newer marker from a damaged one.
pub fn encode_marker_with(version: u32, body: &[u8]) -> Vec<u8> {
    let mut marker = MARKER_MAGIC.to_vec();
    marker.extend_from_slice(&version.to_le_bytes());
    marker.extend_from_slice(body);
    let crc = crc32c::crc32c(&marker);
    marker.extend_from_slice(&crc.to_le_bytes());
    marker
}

/// The marker (current layout) of a directory of history `history`.
pub fn encode_marker(history: HistoryId) -> Vec<u8> {
    encode_marker_with(LAYOUT_VERSION, &history.0)
}

/// What a valid marker says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkerInfo {
    /// The layout version (1 to 4).
    pub version: u32,
    /// The history id (from layout 2; `None` in layout 1).
    pub history: Option<HistoryId>,
}

/// What a marker file says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    Valid(MarkerInfo),
    /// A valid marker of a layout this version doesn't know.
    Newer(u32),
    /// Not our magic: some other file.
    Foreign,
    /// Our magic, but the wrong length or checksum.
    Damaged,
}

fn decode_marker(bytes: &[u8]) -> Marker {
    if !bytes.starts_with(&MARKER_MAGIC) {
        return Marker::Foreign;
    }
    if bytes.len() < MARKER_LEN_V1 {
        return Marker::Damaged;
    }
    let (data, crc) = bytes.split_at(bytes.len() - 4);
    if crc32c::crc32c(data).to_le_bytes() != crc {
        return Marker::Damaged;
    }
    let version = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    match (version, bytes.len()) {
        (1, MARKER_LEN_V1) => Marker::Valid(MarkerInfo { version, history: None }),
        (2..=4, MARKER_LEN) => {
            let mut id = [0u8; 16];
            id.copy_from_slice(&bytes[12..28]);
            Marker::Valid(MarkerInfo { version, history: Some(HistoryId(id)) })
        }
        (version, _) if version > LAYOUT_VERSION => Marker::Newer(version),
        _ => Marker::Damaged,
    }
}

/// The paths of one namespace's files: its directory, `checkpoints/` and
/// `wal/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NsPaths {
    pub id: u64,
    pub dir: PathBuf,
    pub checkpoints: PathBuf,
    pub wal: PathBuf,
}

impl NsPaths {
    /// The paths of namespace `id` in the data directory (or backup) `root`:
    /// `ns/<id>/`.
    pub fn new(root: &Path, id: u64) -> Self {
        Self::in_dir(id, root.join(NS_DIR).join(ns_dir_name(id)))
    }

    /// The paths of a namespace whose directory is `dir`.
    pub fn in_dir(id: u64, dir: PathBuf) -> Self {
        NsPaths { id, checkpoints: dir.join(CHECKPOINT_DIR), wal: dir.join(WAL_DIR), dir }
    }

    /// The paths of the one namespace of a layout 1 to 3 directory (or a
    /// backup of one), whose files are in `root` itself.
    pub fn legacy(root: &Path) -> Self {
        Self::in_dir(DEFAULT_ID, root.to_path_buf())
    }
}

/// An open data directory: its paths, its history and the exclusive lock,
/// held until this value is dropped (or the process exits).
///
/// The lock is `flock` on `LOCK` (`LockFileEx` on Windows), taken without
/// waiting. It is per open file, so a second open in the same process fails
/// like one in another process. It is advisory: it keeps out other stores
/// and tools that take it, not arbitrary programs.
#[derive(Debug)]
pub struct DataDir {
    root: PathBuf,
    history: HistoryId,
    /// The layout version found, if the directory still has to be upgraded
    /// to the current layout ([`upgrade`](Self::upgrade)).
    upgrade_from: Option<u32>,
    /// Holds the lock; closing it releases the lock.
    _lock: File,
}

impl DataDir {
    /// Open the data directory `root` and take its lock. With `create`, a
    /// missing or empty directory is initialized in the current layout
    /// (so is one left by an interrupted initialization), with a new
    /// history id; otherwise it must exist with a marker. Returns the
    /// directory and whether it was created.
    ///
    /// A layout 1 to 3 directory opens too: a layout 1 directory gets a
    /// new history id, the others keep their own, and
    /// [`upgrade`](Self::upgrade) moves the one namespace into `ns/`,
    /// writes the namespace log and the layout 4 marker. Recovery does that
    /// once the namespace has been read successfully, so a failed open
    /// changes nothing. Until then the namespace's files are at
    /// [`legacy_paths`](Self::legacy_paths).
    ///
    /// Errors, all before anything is changed except as noted:
    /// [`Error::NotADataDir`] (no marker, and files that aren't ours, or
    /// `create` is false), [`Error::UnsupportedLayout`] (a newer layout),
    /// [`Error::InvalidDataDir`] (a damaged marker, or missing
    /// directories), [`Error::IsBackup`] (a backup: restore it instead),
    /// [`Error::InterruptedRestore`], [`Error::Locked`] (another store has
    /// it open; the `LOCK` file exists afterwards), [`Error::Io`].
    pub fn open<F: LogFs>(fs: &F, root: &Path, create: bool) -> Result<(DataDir, bool), Error> {
        let not_ours = |reason: &str| Error::NotADataDir { path: root.to_path_buf(), reason: reason.to_owned() };
        match fs::metadata(root) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(not_ours("it is not a directory")),
            Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                fs::create_dir_all(root).map_err(|e| Error::io("create directory", root, e))?;
                if let Some(parent) = root.parent().filter(|p| !p.as_os_str().is_empty()) {
                    fs.sync_dir(parent).map_err(|e| Error::io("sync directory", parent, e))?;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(not_ours("it doesn't exist")),
            Err(e) => return Err(Error::io("stat", root, e)),
        }

        check_openable(root)?;
        let initialized = read_marker(root)?.is_some();
        if !initialized {
            check_empty(root)?;
            if !create {
                return Err(not_ours("it has no marker file"));
            }
        }

        let lock_path = root.join(LOCK_NAME);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| Error::io("open", &lock_path, e))?;
        lock_file(&lock, &lock_path, true)?;

        // Checked again under the lock: another store may have initialized
        // it, or a restore may have finished it
        check_openable(root)?;
        let marker = read_marker(root)?;
        let created = marker.is_none();
        let (history, upgrade_from) = match marker {
            Some(MarkerInfo { version, history: Some(history) }) => {
                (history, (version < LAYOUT_VERSION).then_some(version))
            }
            Some(MarkerInfo { version, history: None }) => (HistoryId::random(), Some(version)),
            None => (HistoryId::random(), None),
        };
        let dir = DataDir { root: root.to_path_buf(), history, upgrade_from, _lock: lock };
        if created {
            check_empty(root)?;
            dir.initialize(fs)?;
        }
        let damaged = |reason: String| Error::InvalidDataDir { path: root.to_path_buf(), reason };
        if dir.upgrade_from.is_none() {
            for sub in [root.join(NS_DIR)] {
                if !sub.is_dir() {
                    return Err(damaged(format!("'{}' is missing", sub.display())));
                }
            }
            if !root.join(NAMESPACES_NAME).is_file() {
                return Err(damaged(format!("'{}' is missing", NAMESPACES_NAME)));
            }
        } else {
            let legacy = dir.legacy_paths();
            for sub in [&legacy.checkpoints, &legacy.wal] {
                if !sub.is_dir() {
                    return Err(damaged(format!("'{}' is missing", sub.display())));
                }
            }
        }
        Ok((dir, created))
    }

    /// Create `ns/`, the default namespace's directories and the namespace
    /// log, each made durable, then the marker. The marker comes last: a
    /// directory without one holds nothing but these.
    fn initialize<F: LogFs>(&self, fs: &F) -> Result<(), Error> {
        let ns_root = self.root.join(NS_DIR);
        match fs.create_dir(&ns_root) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Error::io("create directory", &ns_root, e)),
        }
        create_ns_dir(fs, &self.root, DEFAULT_ID)?;
        let name = NamespaceName::new(DEFAULT_NAME).map_err(iwdb_engine::Error::from)?;
        let event =
            Event { seq: 1, time: CommitTime::now(), kind: EventKind::Create, id: DEFAULT_ID, name, keyed: None };
        write_whole(fs, &self.root.join(NAMESPACES_NAME), &[event])?;
        fs.sync_dir(&self.root).map_err(|e| Error::io("sync directory", &self.root, e))?;
        self.write_marker(fs)
    }

    /// Write the current layout's marker with `write_atomic`, and sync the
    /// directory.
    fn write_marker<F: LogFs>(&self, fs: &F) -> Result<(), Error> {
        let marker = self.root.join(MARKER_NAME);
        let bytes = encode_marker(self.history);
        fs.write_atomic(&marker, &mut |out| out.write_all(&bytes)).map_err(|e| Error::io("write", &marker, e))?;
        fs.sync_dir(&self.root).map_err(|e| Error::io("sync directory", &self.root, e))
    }

    /// The paths of the one namespace of a layout 1 to 3 directory: its
    /// `checkpoints/` and `wal/`, each in the directory itself or, if an
    /// interrupted upgrade moved it already, in `ns/<id>/`. Meant for the
    /// recovery that runs before [`upgrade`](Self::upgrade).
    pub fn legacy_paths(&self) -> NsPaths {
        let moved = NsPaths::new(&self.root, DEFAULT_ID);
        let pick = |sub: &str, moved: &Path| {
            let at_root = self.root.join(sub);
            if at_root.exists() {
                at_root
            } else {
                moved.to_path_buf()
            }
        };
        NsPaths {
            id: DEFAULT_ID,
            dir: moved.dir.clone(),
            checkpoints: pick(CHECKPOINT_DIR, &moved.checkpoints),
            wal: pick(WAL_DIR, &moved.wal),
        }
    }

    /// Whether the directory is in an older layout and
    /// [`upgrade`](Self::upgrade) has work to do.
    pub fn needs_upgrade(&self) -> Option<u32> {
        self.upgrade_from
    }

    /// Upgrade a layout 1 to 3 directory to layout 4, in steps that can
    /// each be repeated after a crash:
    ///
    /// 1. create `ns/` and `ns/<id>/` for the one namespace (id 1, named
    ///    `default`), and sync;
    /// 2. rename `checkpoints/` and `wal/` into it (each rename is atomic),
    ///    and sync both directories;
    /// 3. write the namespace log (`write_atomic`), a single create event
    ///    at time 0, and sync;
    /// 4. replace the marker with a layout 4 one that holds the history id
    ///    (layout 2 and 3) or the one [`open`](Self::open) chose (layout 1),
    ///    then sync. This is the commit point.
    ///
    /// A crash before step 4 leaves the old marker, and the next open finds
    /// the directories where they are ([`legacy_paths`](Self::legacy_paths))
    /// and does the rest. Nothing in a checkpoint or segment changes (a
    /// checkpoint without `iwdb.keys` has an empty key table). Returns the
    /// layout version it upgraded from, `None` for a directory in the
    /// current layout. An older version can't open the directory
    /// afterwards.
    pub fn upgrade<F: LogFs>(&mut self, fs: &F) -> Result<Option<u32>, Error> {
        let Some(from) = self.upgrade_from else { return Ok(None) };
        let ns_root = self.root.join(NS_DIR);
        match fs.create_dir(&ns_root) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Error::io("create directory", &ns_root, e)),
        }
        let target = NsPaths::new(&self.root, DEFAULT_ID);
        match fs.create_dir(&target.dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Error::io("create directory", &target.dir, e)),
        }
        fs.sync_dir(&ns_root).map_err(|e| Error::io("sync directory", &ns_root, e))?;
        for (sub, to) in [(CHECKPOINT_DIR, &target.checkpoints), (WAL_DIR, &target.wal)] {
            let at_root = self.root.join(sub);
            if at_root.exists() {
                if to.exists() {
                    return Err(Error::InvalidDataDir {
                        path: self.root.clone(),
                        reason: format!("both '{}' and '{}' exist", at_root.display(), to.display()),
                    });
                }
                fs.rename(&at_root, to).map_err(|e| Error::io("rename", &at_root, e))?;
            } else if !to.is_dir() {
                return Err(Error::InvalidDataDir {
                    path: self.root.clone(),
                    reason: format!("'{}' is missing", at_root.display()),
                });
            }
        }
        fs.sync_dir(&target.dir).map_err(|e| Error::io("sync directory", &target.dir, e))?;
        fs.sync_dir(&self.root).map_err(|e| Error::io("sync directory", &self.root, e))?;
        let log = self.root.join(NAMESPACES_NAME);
        if log.exists() {
            // Left by an earlier attempt: it must be what this step writes
            let (_, table) = read_log(&log)?;
            let ok =
                table.events().len() == 1 && table.get_id(DEFAULT_ID).is_some_and(|n| n.name.as_str() == DEFAULT_NAME);
            if !ok {
                return Err(Error::InvalidNamespaceLog {
                    path: log,
                    reason: "an upgrade found a namespace log that isn't its own".into(),
                });
            }
        } else {
            let name = NamespaceName::new(DEFAULT_NAME).map_err(iwdb_engine::Error::from)?;
            let event =
                Event { seq: 1, time: CommitTime(0), kind: EventKind::Create, id: DEFAULT_ID, name, keyed: None };
            write_whole(fs, &log, &[event])?;
        }
        fs.sync_dir(&self.root).map_err(|e| Error::io("sync directory", &self.root, e))?;
        self.write_marker(fs)?;
        self.upgrade_from = None;
        Ok(Some(from))
    }

    /// The directory's history id (for a layout 1 directory, the one that
    /// [`upgrade`](Self::upgrade) writes).
    pub fn history(&self) -> HistoryId {
        self.history
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The paths of namespace `id` (layout 4).
    pub fn ns_paths(&self, id: u64) -> NsPaths {
        NsPaths::new(&self.root, id)
    }

    /// `ns/`.
    pub fn ns_root(&self) -> PathBuf {
        self.root.join(NS_DIR)
    }

    /// Remove stale temporary files (`*.tmp`) in the root, in `ns/` and in
    /// the `checkpoints/` and `wal/` of each namespace in `namespaces`: a
    /// checkpoint, marker, segment or namespace log whose creation was
    /// interrupted. None of them is ever read. Each directory where a file
    /// was removed is synced. Returns the removed files.
    pub fn remove_temp_files<F: LogFs>(&self, fs: &F, namespaces: &[NsPaths]) -> Result<Vec<PathBuf>, Error> {
        let mut dirs = vec![self.root.clone(), self.ns_root()];
        for ns in namespaces {
            dirs.push(ns.checkpoints.clone());
            dirs.push(ns.wal.clone());
        }
        let mut removed = Vec::new();
        for dir in dirs {
            if !dir.is_dir() {
                continue;
            }
            let mut any = false;
            for entry in fs::read_dir(&dir).map_err(|e| Error::io("list", &dir, e))? {
                let entry = entry.map_err(|e| Error::io("list", &dir, e))?;
                let is_temp = entry.file_name().to_str().is_some_and(|n| n.ends_with(TEMP_SUFFIX));
                if is_temp && entry.file_type().map_err(|e| Error::io("stat", entry.path(), e))?.is_file() {
                    let path = entry.path();
                    fs.remove_file(&path).map_err(|e| Error::io("remove", &path, e))?;
                    removed.push(path);
                    any = true;
                }
            }
            if any {
                fs.sync_dir(&dir).map_err(|e| Error::io("sync directory", &dir, e))?;
            }
        }
        removed.sort();
        Ok(removed)
    }

    /// The namespace directories under `ns/` that the namespace log
    /// `table` doesn't list as live: what a crash between making a
    /// directory and logging its create event, or between logging a drop
    /// and removing the directory, leaves. A directory of a namespace the
    /// log has dropped is removable; one the log never mentions is
    /// removable only if it holds no data (a crash before the create
    /// event leaves an empty one). One that holds data and isn't dropped
    /// means the log lost its create event (damage to the log's last
    /// event looks like a torn tail), and removing it would destroy a
    /// namespace: [`Error::NamespaceDamaged`] instead, before anything is
    /// changed. Returns the ids to remove, sorted.
    pub fn plan_orphans(&self, table: &NamespaceTable) -> Result<Vec<u64>, Error> {
        let ns_root = self.ns_root();
        let mut orphans = Vec::new();
        for entry in fs::read_dir(&ns_root).map_err(|e| Error::io("list", &ns_root, e))? {
            let entry = entry.map_err(|e| Error::io("list", &ns_root, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = parse_ns_dir_name(&name) else { continue };
            if table.get_id(id).is_some() || !entry.path().is_dir() {
                continue;
            }
            let dropped = table.events().iter().any(|e| e.id == id && e.kind == EventKind::Drop);
            if !dropped && has_data(&NsPaths::new(&self.root, id))? {
                return Err(Error::NamespaceDamaged {
                    id,
                    name: "?".into(),
                    reason: format!(
                        "'{}' holds data, but the namespace log doesn't list namespace {} (its create event may have been lost with a damaged last event of '{}')",
                        entry.path().display(),
                        id,
                        NAMESPACES_NAME
                    ),
                });
            }
            orphans.push(id);
        }
        orphans.sort_unstable();
        Ok(orphans)
    }

    /// Remove the namespace directories [`plan_orphans`](Self::plan_orphans)
    /// chose, each synced.
    pub fn remove_orphans<F: LogFs>(&self, fs: &F, orphans: &[u64]) -> Result<(), Error> {
        for id in orphans {
            remove_ns_dir(fs, &self.root, *id)?;
        }
        Ok(())
    }
}

/// Whether a namespace directory holds data: a checkpoint, or a WAL
/// segment with anything after its header.
pub(crate) fn has_data_pub(paths: &NsPaths) -> Result<bool, Error> {
    has_data(paths)
}

fn has_data(paths: &NsPaths) -> Result<bool, Error> {
    if paths.checkpoints.is_dir() && !crate::checkpoint::list_checkpoints(&paths.checkpoints)?.is_empty() {
        return Ok(true);
    }
    if paths.wal.is_dir() {
        for (_, path) in crate::reader::list_segments(&paths.wal)? {
            let len = fs::metadata(&path).map_err(|e| Error::io("stat", &path, e))?.len();
            if len > crate::format::SEGMENT_HEADER_LEN as u64 {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Create the directory of namespace `id` in `root`: `ns/<id>/`,
/// `checkpoints/` and `wal/`, each synced into its parent. A directory
/// that is there already is an orphan (a crashed create, a namespace
/// dropped earlier) and is removed first. The namespace log must not
/// list the id before this has returned: the directory comes first.
pub fn create_ns_dir<F: LogFs>(fs: &F, root: &Path, id: u64) -> Result<NsPaths, Error> {
    let paths = NsPaths::new(root, id);
    let ns_root = root.join(NS_DIR);
    if paths.dir.exists() {
        fs.remove_dir_all(&paths.dir).map_err(|e| Error::io("remove directory", &paths.dir, e))?;
    }
    for dir in [&paths.dir, &paths.checkpoints, &paths.wal] {
        match fs.create_dir(dir) {
            Ok(()) => {}
            Err(e) => return Err(Error::io("create directory", dir, e)),
        }
    }
    for dir in [&paths.checkpoints, &paths.wal, &paths.dir, &ns_root] {
        fs.sync_dir(dir).map_err(|e| Error::io("sync directory", dir, e))?;
    }
    Ok(paths)
}

/// Remove the directory of namespace `id` and sync `ns/`. The namespace
/// log must list the drop before this runs.
pub fn remove_ns_dir<F: LogFs>(fs: &F, root: &Path, id: u64) -> Result<(), Error> {
    let paths = NsPaths::new(root, id);
    match fs.remove_dir_all(&paths.dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io("remove directory", &paths.dir, e)),
    }
    let ns_root = root.join(NS_DIR);
    fs.sync_dir(&ns_root).map_err(|e| Error::io("sync directory", &ns_root, e))
}

/// Read the marker of the data directory `root`, without taking the lock
/// or changing anything: `None` if it has none. Errors:
/// [`Error::UnsupportedLayout`] (a newer layout), [`Error::NotADataDir`]
/// (a file named `IWDB` that isn't ours), [`Error::InvalidDataDir`] (a
/// damaged marker), [`Error::Io`].
pub fn read_marker(root: &Path) -> Result<Option<MarkerInfo>, Error> {
    let path = root.join(MARKER_NAME);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io("read", &path, e)),
    };
    match decode_marker(&bytes) {
        Marker::Valid(info) => Ok(Some(info)),
        Marker::Newer(version) => Err(Error::UnsupportedLayout { path: root.to_path_buf(), version }),
        Marker::Foreign => Err(Error::NotADataDir {
            path: root.to_path_buf(),
            reason: format!("'{}' is not an Ironweaver DB marker", MARKER_NAME),
        }),
        Marker::Damaged => {
            Err(Error::InvalidDataDir { path: root.to_path_buf(), reason: format!("'{}' is damaged", MARKER_NAME) })
        }
    }
}

/// A store opens neither an interrupted restore nor a backup.
fn check_openable(root: &Path) -> Result<(), Error> {
    if root.join(RESTORING_NAME).exists() {
        return Err(Error::InterruptedRestore { path: root.to_path_buf() });
    }
    if root.join(BACKUP_NAME).exists() && root.join(MARKER_NAME).exists() {
        return Err(Error::IsBackup { path: root.to_path_buf() });
    }
    Ok(())
}

/// A directory without a marker may only hold what an interrupted
/// initialization leaves: `LOCK`, a layout 1 to 3 initialization's empty
/// `checkpoints/` and `wal/`, a layout 4 one's `ns/` (holding nothing but
/// empty directories) and the namespace log of a new store (one create
/// event for `default`), and temporary files. An interrupted backup or
/// restore leaves checkpoints or segments, and is refused.
fn check_empty(root: &Path) -> Result<(), Error> {
    for entry in fs::read_dir(root).map_err(|e| Error::io("list", root, e))? {
        let entry = entry.map_err(|e| Error::io("list", root, e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let path = entry.path();
        let ok = match &*name {
            LOCK_NAME => path.is_file(),
            CHECKPOINT_DIR | WAL_DIR => {
                path.is_dir() && fs::read_dir(&path).map_err(|e| Error::io("list", &path, e))?.next().is_none()
            }
            NS_DIR => path.is_dir() && only_empty_dirs(&path)?,
            NAMESPACES_NAME => path.is_file() && is_initial_log(&path),
            other => other.ends_with(TEMP_SUFFIX) && path.is_file(),
        };
        if !ok {
            let what = if matches!(&*name, CHECKPOINT_DIR | WAL_DIR | NS_DIR) { "a non-empty " } else { "" };
            return Err(Error::NotADataDir {
                path: root.to_path_buf(),
                reason: format!(
                    "it has no marker file and holds {}'{}' (an interrupted backup or restore leaves such a directory)",
                    what, name
                ),
            });
        }
    }
    Ok(())
}

/// Whether `dir` holds nothing but directories, recursively.
fn only_empty_dirs(dir: &Path) -> Result<bool, Error> {
    for entry in fs::read_dir(dir).map_err(|e| Error::io("list", dir, e))? {
        let path = entry.map_err(|e| Error::io("list", dir, e))?.path();
        if !path.is_dir() || !only_empty_dirs(&path)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether the namespace log at `path` is the one initialization writes: a
/// single create event for `default`.
fn is_initial_log(path: &Path) -> bool {
    read_log(path).is_ok_and(|(_, table)| {
        table.events().len() == 1 && table.get_id(DEFAULT_ID).is_some_and(|n| n.name.as_str() == DEFAULT_NAME)
    })
}

/// Take a shared lock on `<root>/LOCK`, if that file exists, without
/// creating it: readers that must not run while a store has the directory
/// open (verify, restore reading a data directory) hold it while they
/// read. Shared locks don't exclude each other. Returns the locked file,
/// or `None` if there is no `LOCK` file (then no store has it open: a
/// store creates `LOCK` before anything else). Fails with
/// [`Error::Locked`] if a store has it open.
pub fn lock_shared(root: &Path) -> Result<Option<File>, Error> {
    let path = root.join(LOCK_NAME);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io("open", &path, e)),
    };
    lock_file(&file, &path, false)?;
    Ok(Some(file))
}

/// How long [`lock_file`] waits between attempts, in milliseconds: about
/// 80 ms in all.
const LOCK_RETRIES_MS: [u64; 6] = [1, 2, 5, 10, 20, 40];

/// Lock `file` (the lock file at `path`), exclusively or shared, without
/// blocking; [`Error::Locked`] if it is held.
///
/// A held lock is tried again a few times over about 80 ms before giving
/// up. A process that another thread of this process is spawning holds a
/// copy of every open file between its fork and its exec (close-on-exec
/// takes effect only at the exec), and a `flock` belongs to the open file,
/// so the lock of a store that was just closed can look held for that
/// moment. Without the retries, reopening a store while another thread
/// starts processes failed now and then (step 7 found it: 3.5% of reopens
/// under heavy spawning). A real holder makes the open fail after the
/// retries.
pub fn lock_file(file: &File, path: &Path, exclusive: bool) -> Result<(), Error> {
    let attempt = || {
        if exclusive {
            fs4::FileExt::try_lock(file)
        } else {
            fs4::FileExt::try_lock_shared(file)
        }
    };
    for wait in LOCK_RETRIES_MS.iter().map(|ms| Some(std::time::Duration::from_millis(*ms))).chain([None]) {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(fs4::TryLockError::WouldBlock) => match wait {
                Some(wait) => std::thread::sleep(wait),
                None => return Err(Error::Locked { path: path.to_path_buf() }),
            },
            Err(fs4::TryLockError::Error(e)) => return Err(Error::io("lock", path, e)),
        }
    }
    Err(Error::Locked { path: path.to_path_buf() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_checked() {
        let history = HistoryId([7; 16]);
        let marker = encode_marker(history);
        assert_eq!(marker.len(), MARKER_LEN);
        assert_eq!(decode_marker(&marker), Marker::Valid(MarkerInfo { version: 4, history: Some(history) }));
        let v3 = encode_marker_with(3, &history.0);
        assert_eq!(decode_marker(&v3), Marker::Valid(MarkerInfo { version: 3, history: Some(history) }));
        let v2 = encode_marker_with(2, &history.0);
        assert_eq!(decode_marker(&v2), Marker::Valid(MarkerInfo { version: 2, history: Some(history) }));
        let v1 = encode_marker_with(1, &[]);
        assert_eq!(v1.len(), MARKER_LEN_V1);
        assert_eq!(decode_marker(&v1), Marker::Valid(MarkerInfo { version: 1, history: None }));
        // A newer layout may have any body
        assert_eq!(decode_marker(&encode_marker_with(7, b"whatever")), Marker::Newer(7));
        assert_eq!(decode_marker(&encode_marker_with(5, b"whatever")), Marker::Newer(5));
        assert_eq!(decode_marker(&encode_marker_with(1, &[0; 16])), Marker::Damaged);
        assert_eq!(decode_marker(&encode_marker_with(0, &[])), Marker::Damaged);
        assert_eq!(decode_marker(b"hello"), Marker::Foreign);
        assert_eq!(decode_marker(&marker[..15]), Marker::Damaged);
        assert_eq!(decode_marker(&marker[..31]), Marker::Damaged);
        for bit in 64..MARKER_LEN * 8 {
            let mut bad = marker.clone();
            bad[bit / 8] ^= 1 << (bit % 8);
            assert_eq!(decode_marker(&bad), Marker::Damaged, "bit {}", bit);
        }
    }
}
