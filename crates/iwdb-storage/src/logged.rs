//! [`LoggedNamespace`]: a namespace whose commits go through its log.

use iwdb_engine::{CatalogChange, CommitResult, Mutation, Namespace, Prepared};

use crate::io::{LogFs, StdFs};
use crate::{Error, Wal};

/// A [`Namespace`] and the [`Wal`] its commits are logged to: the commit
/// order of the durable write path.
///
/// A commit is resolved and validated ([`Namespace::prepare`]), appended to
/// the log and fsynced per the log's policy ([`Wal::append`]), applied in
/// memory ([`Namespace::apply`]), and only then acknowledged (returned).
/// So an acknowledged commit is in the log, durable with
/// [`FsyncPolicy::Always`](crate::FsyncPolicy::Always), and a commit whose
/// append fails is never applied.
///
/// **Read-only state.** If the log fails (a write, fsync or rotation
/// error), or applying a logged record fails and poisons the namespace
/// ([`iwdb_engine::Error::ApplyFailed`]), every further commit fails with
/// [`Error::ReadOnly`] until the namespace is reopened from its checkpoint
/// and log (step 5). Reads still work, and see every applied commit.
///
/// This is the composition step 4 needs for its tests; the embedded
/// `Store` (step 5) builds on it. Not thread-safe by itself (`&mut self`);
/// one namespace only (step 9 decides how namespaces share logs).
#[derive(Debug)]
pub struct LoggedNamespace<F: LogFs = StdFs> {
    namespace: Namespace,
    wal: Wal<F>,
}

impl<F: LogFs> LoggedNamespace<F> {
    /// Pair a namespace with the log its next commit goes to: the log's
    /// next seq must follow the namespace's.
    pub fn new(namespace: Namespace, wal: Wal<F>) -> Result<Self, Error> {
        let expected = namespace.seq().checked_add(1).ok_or(iwdb_engine::Error::SeqExhausted)?;
        if wal.next_seq() != expected {
            return Err(Error::OutOfOrder { expected, found: wal.next_seq() });
        }
        Ok(LoggedNamespace { namespace, wal })
    }

    /// Commit a data transaction: all mutations or none. Returns once the
    /// commit is logged (per the fsync policy) and applied.
    ///
    /// Errors: an [`Error::Engine`] from validation (nothing changes, the
    /// namespace stays writable); [`Error::RecordTooLarge`] (likewise);
    /// [`Error::Io`] (not applied, outcome unknown, now read-only);
    /// [`Error::ReadOnly`].
    pub fn commit(&mut self, mutations: &[Mutation]) -> Result<CommitResult, Error> {
        self.check_writable()?;
        let prepared = self.namespace.prepare(mutations)?;
        self.log_and_apply(prepared)
    }

    /// Commit a catalog change, like [`commit`](Self::commit).
    pub fn commit_catalog(&mut self, change: CatalogChange) -> Result<CommitResult, Error> {
        self.check_writable()?;
        let prepared = self.namespace.prepare_catalog(change)?;
        self.log_and_apply(prepared)
    }

    /// Fsync every logged commit (see [`Wal::sync`]).
    pub fn sync(&mut self) -> Result<(), Error> {
        self.wal.sync()
    }

    /// Fsync group-committed records that are due (see [`Wal::sync_due`]).
    pub fn sync_due(&mut self) -> Result<bool, Error> {
        self.wal.sync_due()
    }

    /// The namespace, for reading.
    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }

    pub fn wal(&self) -> &Wal<F> {
        &self.wal
    }

    /// Why the namespace is read-only, if it is.
    pub fn read_only(&self) -> Option<String> {
        if let Some(cause) = self.wal.failure() {
            return Some(cause.to_owned());
        }
        self.namespace.is_poisoned().then(|| iwdb_engine::Error::Poisoned.to_string())
    }

    /// Sync (per the policy) and close the log, returning the namespace.
    pub fn close(self) -> Result<Namespace, Error> {
        self.wal.close()?;
        Ok(self.namespace)
    }

    /// Split into the namespace and its log.
    pub fn into_parts(self) -> (Namespace, Wal<F>) {
        (self.namespace, self.wal)
    }

    fn check_writable(&self) -> Result<(), Error> {
        match self.read_only() {
            Some(cause) => Err(Error::ReadOnly { cause }),
            None => Ok(()),
        }
    }

    fn log_and_apply(&mut self, prepared: Prepared) -> Result<CommitResult, Error> {
        self.wal.append(prepared.record())?;
        // An error here is ApplyFailed (the namespace is now poisoned, so
        // read-only) or a bug; either way the commit is not acknowledged.
        Ok(self.namespace.apply(prepared)?)
    }
}
