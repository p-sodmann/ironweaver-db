//! Online backups and verification (ADR 0055).
//!
//! Backups acquire the catalog mutex before checkpointer locks in namespace
//! id order and keep those guards until copying finishes. Verification holds
//! one checkpointer at a time and releases it before checking live state.

use std::path::Path;
use std::sync::MutexGuard;
use std::sync::atomic::Ordering;

use iwdb_engine::CommitTime;
use iwdb_storage::backup::{self, NamespaceSource};
use iwdb_storage::io::{LogFs, Throttle};
use iwdb_storage::{BackupReport, Checkpointer, Error, Kind, VerifyReport};

use super::background::or_abort;
use super::{BackupStats, Store, lock};

impl<F: LogFs + Clone + Send + Sync + 'static> Store<F>
where
    F::File: Send,
{
    /// Back the store up into `dest`, a missing or empty directory, while
    /// it runs: a consistent copy of its data directory up to the last
    /// commit of every namespace, which `verify` checks and
    /// [`restore`](crate::restore) turns into a store again
    /// (`documentation/formats/backup.md`, ADR 0009, ADR 0017).
    ///
    /// - **What it reaches**: every namespace's WAL is fsynced first, and
    ///   the backup holds each up to its synced seq then, which is its last
    ///   commit (a read-only namespace: its synced seq). Commits after
    ///   that are not in it. Namespaces are copied one after another, each
    ///   consistent; there are no commits across namespaces to keep apart.
    /// - **What it holds**: per namespace, every checkpoint at or below its
    ///   seq and the WAL from the oldest of them up to the seq, so it
    ///   restores to any seq in between; and the namespace log.
    /// - **Waiting**: commits wait only for the fsync. Namespaces can't be
    ///   created or dropped, and checkpoints (background, explicit and on
    ///   close) wait, until the copy is done: the backup holds the catalog
    ///   mutex and every checkpointer's lock, so that no file it copies is
    ///   removed meanwhile, and the WALs grow until then.
    /// - **Writing**: every file is fsynced, then the manifest, then the
    ///   marker, last. A backup that fails or is interrupted leaves a
    ///   directory without a marker, which a store, `verify` and `restore`
    ///   refuse; remove it and try again.
    ///
    /// Errors: [`Error::DestinationNotEmpty`]; [`Error::InvalidOptions`] if
    /// `dest` is inside the data directory; [`Error::Io`]; a WAL read error
    /// if a segment it copies is damaged; a failed fsync of a WAL (then
    /// that namespace is read-only, as after any failed fsync).
    ///
    /// The copy runs at most at [`BackupOptions::max_bytes_per_second`](crate::BackupOptions)
    /// ([`backup_with`](Self::backup_with) sets another rate).
    pub fn backup(&self, dest: &Path) -> Result<BackupReport, Error> {
        self.backup_with(dest, self.shared.options.backup.max_bytes_per_second)
    }

    /// [`backup`](Self::backup), copying at most `max_bytes_per_second`
    /// (`None` or 0: unthrottled). Checkpoints wait for the whole copy, so
    /// a slower one holds them back longer (ADR 0055).
    pub fn backup_with(&self, dest: &Path, max_bytes_per_second: Option<u64>) -> Result<BackupReport, Error> {
        let _span = iwdb_storage::trace_span!("iwdb.backup").entered();
        let stats = &self.shared.backups;
        stats.running.fetch_add(1, Ordering::AcqRel);
        let result = self.backup_now(dest, max_bytes_per_second);
        stats.running.fetch_sub(1, Ordering::AcqRel);
        match &result {
            Ok(_) => {
                stats.ok.fetch_add(1, Ordering::Relaxed);
                *lock(&stats.last) = Some(CommitTime::now());
            }
            Err(_) => {
                stats.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
        result
    }

    /// Backups since the store opened: running, bytes copied, outcomes.
    pub fn backup_stats(&self) -> BackupStats {
        let b = &self.shared.backups;
        BackupStats {
            running: b.running.load(Ordering::Acquire),
            bytes: b.bytes.load(Ordering::Relaxed),
            ok: b.ok.load(Ordering::Relaxed),
            failed: b.failed.load(Ordering::Relaxed),
            last: *lock(&b.last),
        }
    }

    /// Verify the open store without stopping it (ADR 0055): per namespace,
    /// [`verify`](crate::verify)'s checks of its checkpoints and WAL, with
    /// the WAL read up to the namespace's synced seq (it is fsynced first,
    /// as for a backup), then the live state's invariants, and the live
    /// state against the replayed one if nothing was committed meanwhile.
    /// Damage is reported in [`VerifyReport::problems`], not as an error.
    /// The namespace log isn't read (the store holds it).
    ///
    /// - **Waiting**: each namespace's checkpoints wait while it is checked
    ///   (its checkpointer's lock is held, so that no file vanishes), and
    ///   its commits wait for the fsync and for the read lock at the end.
    /// - **Memory**: a replayed copy of the namespace being checked plus
    ///   one checkpoint, which the memory limit doesn't count.
    ///
    /// Errors: a failed fsync of a WAL (the namespace is then read-only, as
    /// after any failed fsync); [`Error::Io`] for a directory that can't be
    /// listed.
    pub fn verify(&self) -> Result<VerifyReport, Error> {
        let _span = iwdb_storage::trace_span!("iwdb.verify").entered();
        let root = self.dir.root();
        let mut report = VerifyReport::new(root, Kind::DataDir);
        if let Ok(Some(marker)) = iwdb_storage::layout::read_marker(root) {
            report.version = Some(marker.version);
            report.history = marker.history;
        }
        let mut states = self.shared.states();
        states.sort_by_key(|s| s.info.id);
        for state in states {
            let _namespace =
                iwdb_storage::trace_span!("iwdb.verify.namespace", db.namespace = %state.info.name).entered();
            let guard = lock(&state.checkpointer);
            let live = &state.live;
            // A read-only namespace appends nothing more: read it to its end
            let until = if live.read_only().is_none() {
                or_abort("an fsync of the WAL", || live.sync())?;
                live.wal().synced_seq().min(live.seq())
            } else {
                u64::MAX
            };
            let (mut sub, replayed) =
                iwdb_storage::verify::verify_open_namespace(root, &state.info.name, &state.paths, until)?;
            drop(guard);
            live.read(|ns| {
                for violation in iwdb_engine::invariants::check(ns) {
                    sub.problem(None, format!("the live state at seq {}: {}", ns.seq(), violation));
                }
                if let Some(replayed) = replayed.as_ref().filter(|r| r.seq() == ns.seq())
                    && let Err(difference) = iwdb_engine::invariants::compare(replayed, ns)
                {
                    sub.problem(
                        None,
                        format!("the live state differs from the WAL replayed to seq {}: {}", ns.seq(), difference),
                    );
                }
            });
            report.merge(state.info.id, state.info.name.as_str(), true, sub);
        }
        report.summarize();
        log::info!(
            "{}: verified the open store: {}",
            root.display(),
            if report.is_ok() { "ok".to_owned() } else { format!("{} problems", report.problems.len()) }
        );
        Ok(report)
    }

    fn backup_now(&self, dest: &Path, max_bytes_per_second: Option<u64>) -> Result<BackupReport, Error> {
        let catalog = lock(&self.shared.catalog);
        let mut states = self.shared.states();
        states.sort_by_key(|s| s.info.id);
        let guards: Vec<MutexGuard<'_, Checkpointer<F>>> = states.iter().map(|s| lock(&s.checkpointer)).collect();
        let mut sources = Vec::new();
        for (state, guard) in states.iter().zip(&guards) {
            let live = &state.live;
            if live.read_only().is_none() {
                or_abort("an fsync of the WAL", || live.sync())?;
            }
            let seq = live.wal().synced_seq().min(live.seq());
            sources.push((state.clone(), seq, guard.damaged().clone()));
        }
        let namespaces: Vec<NamespaceSource<'_>> = sources
            .iter()
            .map(|(state, seq, damaged)| NamespaceSource {
                name: state.info.name.clone(),
                paths: state.paths.clone(),
                seq: *seq,
                damaged,
            })
            .collect();
        let log = std::fs::read(catalog.log.path()).map_err(|e| Error::Io {
            op: "read",
            path: catalog.log.path().to_path_buf(),
            source: e,
        })?;
        let mut throttle = Throttle::new(max_bytes_per_second, Some(&self.shared.backups.bytes));
        let report = backup::write_backup(
            &self.shared.fs,
            self.dir.root(),
            self.history(),
            &namespaces,
            &log,
            dest,
            &mut throttle,
        )?;
        log::info!(
            "{}: backed up {} namespaces into '{}'",
            self.dir.root().display(),
            report.namespaces.len(),
            dest.display()
        );
        Ok(report)
    }
}
