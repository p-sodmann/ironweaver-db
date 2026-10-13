//! Namespace lookup, creation and removal (ADR 0017).
//!
//! Lifecycle changes take the catalog mutex first. Drops then hold the
//! namespace checkpointer through the durable drop event and removal from
//! the namespace map. The event append remains the durable commit point.

use std::sync::{Arc, MutexGuard, PoisonError};

use iwdb_engine::IdempotencyKey;
use iwdb_engine::catalog::NamespaceName;
use iwdb_storage::io::LogFs;
use iwdb_storage::layout::{NsPaths, create_ns_dir, remove_ns_dir};
use iwdb_storage::namespaces::{EventKind, NamespaceInfo, NamespaceResult, Plan};
use iwdb_storage::{Error, read_namespace, start_namespace};

use super::background::or_abort;
use super::{CatalogState, NAMESPACE, Ns, Store, lock, new_state, public_name, unreachable_default};

impl<F: LogFs + Clone + Send + Sync + 'static> Store<F>
where
    F::File: Send,
{
    /// Every namespace (but the reserved system namespace), by name, without
    /// waiting for the namespace log (which creating, dropping and backups
    /// hold): for the status views and the metrics.
    pub fn open_namespaces(&self) -> Vec<Ns<'_, F>> {
        let mut list: Vec<Ns<'_, F>> = self
            .shared
            .states()
            .into_iter()
            .filter(|s| !s.info.name.is_reserved())
            .map(|state| Ns { store: self, state })
            .collect();
        list.sort_by(|a, b| a.name().cmp(b.name()));
        list
    }

    pub fn namespace(&self, name: &str) -> Result<Ns<'_, F>, Error> {
        match self.shared.find(name) {
            Some(state) if !state.info.name.is_reserved() => Ok(Ns { store: self, state }),
            _ => Err(Error::NoSuchNamespace { name: name.to_owned() }),
        }
    }

    /// The reserved system namespace of users and grants (ADR 0043), made
    /// with `create` if the store has none yet.
    /// Serialize a change to users, grants or tokens (`crate::auth`).
    pub(crate) fn auth_lock(&self) -> MutexGuard<'_, ()> {
        lock(&self.shared.auth)
    }

    pub(crate) fn system_namespace(&self, create: bool) -> Result<Option<Ns<'_, F>>, Error> {
        if let Some(state) = self.shared.find(NamespaceName::SYSTEM) {
            return Ok(Some(Ns { store: self, state }));
        }
        if !create {
            return Ok(None);
        }
        let mut catalog = lock(&self.shared.catalog);
        if self.shared.find(NamespaceName::SYSTEM).is_none() {
            or_abort("creating the system namespace", || {
                self.create_locked(&mut catalog, &NamespaceName::system(), None)
            })?;
            self.sync_archive_log(&catalog);
        }
        drop(catalog);
        Ok(self.shared.find(NamespaceName::SYSTEM).map(|state| Ns { store: self, state }))
    }

    /// The `default` namespace.
    pub fn default_namespace(&self) -> Ns<'_, F> {
        // Invariant: `default` exists (it can't be dropped, and open
        // creates it if a restored store lacks it)
        match self.shared.find(NAMESPACE) {
            Some(state) => Ns { store: self, state },
            None => unreachable_default(),
        }
    }

    /// The live namespaces, by name (without the reserved system
    /// namespace). Doesn't wait for any commit.
    pub fn namespaces(&self) -> Vec<NamespaceInfo> {
        let mut list: Vec<NamespaceInfo> =
            lock(&self.shared.catalog).log.table().live().filter(|i| !i.name.is_reserved()).cloned().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    /// Create the namespace `name`: an empty graph with its own WAL, seq
    /// space and catalog.
    ///
    /// **Crash safety** (ADR 0017): its directory is made and synced first,
    /// then the create event is appended to the namespace log and fsynced;
    /// that is the commit point. A crash before leaves a directory the log
    /// doesn't list, which the next open removes; after, the namespace
    /// exists (empty). With an idempotency key, a retry after an unknown
    /// outcome returns the original event
    /// ([`NamespaceResult::deduplicated`]); the key is remembered for good
    /// (the log is never truncated).
    ///
    /// Errors: [`Error::NamespaceExists`]; an invalid name or key
    /// ([`Error::Engine`]); [`IdempotencyKeyReused`](iwdb_engine::Error::IdempotencyKeyReused) (as
    /// [`Error::Engine`]); [`Error::ReadOnly`] (the namespace log failed);
    /// [`Error::Io`] (nothing is created, or the outcome is unknown: the
    /// namespace log is failed until reopening).
    pub fn create_namespace(&self, name: &str, key: Option<&IdempotencyKey>) -> Result<NamespaceResult, Error> {
        let name = public_name(name)?;
        let mut catalog = lock(&self.shared.catalog);
        let result = or_abort("creating a namespace", || self.create_locked(&mut catalog, &name, key))?;
        if !result.deduplicated {
            self.sync_archive_log(&catalog);
        }
        Ok(result)
    }

    // The caller holds the catalog mutex through the durable log append
    // and publication in the namespace map.
    pub(super) fn create_locked(
        &self,
        catalog: &mut CatalogState<F>,
        name: &NamespaceName,
        key: Option<&IdempotencyKey>,
    ) -> Result<NamespaceResult, Error> {
        if let Some(cause) = catalog.log.failure() {
            return Err(Error::ReadOnly { cause: cause.to_owned() });
        }
        let id = match catalog.log.table().plan(EventKind::Create, name, key)? {
            Plan::Duplicate(event) => return Ok(NamespaceResult { event, deduplicated: true }),
            Plan::New { id } => id,
        };
        // The memory limit (ADR 0054), before the namespace is logged.
        // `default` is only created when a store opens without one, the
        // system namespace with the first user, which must work at any time
        if name.as_str() != NAMESPACE && !name.is_reserved() {
            self.shared.memory.check_write()?;
        }
        let paths = create_ns_dir(&self.shared.fs, &self.shared.root, id)?;
        let event = catalog.log.append(EventKind::Create, id, name, key)?;
        self.open_created(catalog, name, paths, event)
    }

    /// Open the namespace `name` whose create event was just logged (its
    /// directory in `paths`), and add it to the open namespaces.
    pub(super) fn open_created(
        &self,
        catalog: &mut CatalogState<F>,
        name: &NamespaceName,
        paths: NsPaths,
        event: iwdb_storage::namespaces::Event,
    ) -> Result<NamespaceResult, Error> {
        let shared = &self.shared;
        // The namespace exists now. If opening it fails, the namespace log
        // is failed: the next open sorts it out
        let opened = (|| {
            let read = read_namespace(&shared.fs, &paths, name)?;
            start_namespace(shared.fs.clone(), &paths, read, shared.options.wal.clone())
        })();
        let (live, report) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                catalog.log.fail(format!("namespace '{}' was created but couldn't be opened: {}", name, e));
                return Err(e);
            }
        };
        let info =
            catalog.log.table().get(name).cloned().ok_or_else(|| Error::NoSuchNamespace { name: name.to_string() })?;
        let size_trigger = shared.options.checkpoint.wal_size.unwrap_or(u64::MAX);
        let state = Arc::new(new_state(
            &shared.fs,
            &shared.options,
            &shared.memory,
            &catalog.archive,
            info,
            paths,
            live,
            report,
            size_trigger,
        ));
        shared.namespaces.write().unwrap_or_else(PoisonError::into_inner).insert(name.to_string(), state);
        Ok(NamespaceResult { event, deduplicated: false })
    }

    /// Drop the namespace `name` and everything in it. `default` can't be
    /// dropped.
    ///
    /// **Crash safety** (ADR 0017): the namespace stops accepting commits,
    /// its WAL is fsynced, and with an archive its remaining segments are
    /// archived (so a restore to a time before the drop still has them);
    /// then the drop event is appended to the namespace log and fsynced,
    /// the commit point; then its directory is removed. A crash before the
    /// event leaves the namespace; after it, it is gone, and the next open
    /// removes what is left of the directory. With an idempotency key, a
    /// retry returns the original event.
    ///
    /// **Who waits**: commits, and waits for a `min_seq`, on the namespace
    /// fail with [`Error::NamespaceDropped`]; reads and analytics that
    /// started finish on the state they read. Handles ([`Ns`]) of it fail
    /// the same way.
    ///
    /// Errors: [`Error::NoSuchNamespace`]; [`Error::InvalidOptions`] for
    /// `default`; the archive's errors (the namespace is left as it was);
    /// [`Error::Io`] from the namespace log (outcome unknown: the namespace
    /// is unavailable and the log failed until reopening).
    pub fn drop_namespace(&self, name: &str, key: Option<&IdempotencyKey>) -> Result<NamespaceResult, Error> {
        let name = public_name(name)?;
        let mut catalog = lock(&self.shared.catalog);
        let result = or_abort("dropping a namespace", || self.drop_locked(&mut catalog, &name, key))?;
        if !result.deduplicated {
            self.sync_archive_log(&catalog);
        }
        Ok(result)
    }

    fn drop_locked(
        &self,
        catalog: &mut CatalogState<F>,
        name: &NamespaceName,
        key: Option<&IdempotencyKey>,
    ) -> Result<NamespaceResult, Error> {
        if let Some(cause) = catalog.log.failure() {
            return Err(Error::ReadOnly { cause: cause.to_owned() });
        }
        let id = match catalog.log.table().plan(EventKind::Drop, name, key)? {
            Plan::Duplicate(event) => return Ok(NamespaceResult { event, deduplicated: true }),
            Plan::New { id } => id,
        };
        if name.as_str() == NAMESPACE {
            return Err(Error::InvalidOptions("the 'default' namespace can't be dropped".into()));
        }
        let shared = &self.shared;
        let state = shared.find(name.as_str()).ok_or_else(|| Error::NoSuchNamespace { name: name.to_string() })?;
        let checkpointer = lock(&state.checkpointer);
        state.live.mark_dropped();
        // Waits for a commit in progress; none starts after the flag
        if state.live.read_only().is_none() {
            state.live.sync()?;
        }
        if let Some(archive) = &catalog.archive {
            let segments = iwdb_storage::list_segments(&state.paths.wal)?;
            if !segments.is_empty() {
                archive.copy(id, &segments)?;
                archive.sync(id)?;
            }
        }
        let event = catalog.log.append(EventKind::Drop, id, name, key)?;
        shared.namespaces.write().unwrap_or_else(PoisonError::into_inner).remove(name.as_str());
        drop(checkpointer);
        // The drop is durable. The directory goes now, or at the next open
        if let Err(e) = remove_ns_dir(&shared.fs, &shared.root, id) {
            log::warn!(
                "{}: namespace '{}' is dropped, but its directory wasn't removed: {}",
                shared.root.display(),
                name,
                e
            );
        }
        Ok(NamespaceResult { event, deduplicated: false })
    }

    /// Bring the archive's copy of the namespace log up to date. A failure
    /// is logged, not returned: the next namespace operation or open
    /// retries, and restores only reach as far as the copy.
    pub(super) fn sync_archive_log(&self, catalog: &CatalogState<F>) {
        if let Some(archive) = &catalog.archive
            && let Err(e) = archive.write_log(catalog.log.table().events())
        {
            log::warn!("{}: the archive's namespace log is stale: {}", self.shared.root.display(), e);
        }
    }
}
