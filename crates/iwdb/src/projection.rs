//! Projection mode (ADR 0032): Ironweaver DB as a durable projection of an
//! external, ordered event log.
//!
//! A [`Projection`] reads events from a [`Source`] after its mark, maps
//! each to mutations with a [`Mapping`], and commits them **with the mark
//! in the same commit** ([`Ns::commit_marked`]): the high-water mark moves
//! atomically with the effect of the events up to it. A projection always
//! resumes from the committed mark, so every event is applied at most
//! once, whatever crashes; and exactly once, unless it is skipped
//! ([`OnError::Skip`]).
//!
//! - [`Projection::step`] runs one round in the caller's thread;
//! - [`Store::project`](crate::Store::project) runs a projection on a
//!   thread of its own until it is stopped, the store closes or an error
//!   stops it, and returns a [`ProjectionHandle`].
//!
//! Sources: anything that implements [`Source`]; with the `postgres`
//! feature, `postgres::PostgresSource` reads a table. Mappings: anything
//! that implements [`Mapping`] (closures do), or the declarative
//! [`Rules`].

use std::fmt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ironweaver_core::Attrs;
use iwdb_engine::{CommitResult, MarkName, MarkUpdate, Mutation};
use iwdb_query::CommitOptions;
use iwdb_storage::Error;
use iwdb_storage::io::LogFs;

use crate::store::Ns;

#[cfg(feature = "postgres")]
pub mod postgres;
mod rules;

pub use rules::{Piece, Rule, RuleMutation, Rules, Template};

/// One event of a source: its position in the source's log and its
/// fields.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceEvent {
    /// Above 0, and strictly increasing in a source's log.
    pub position: u64,
    pub fields: Attrs,
}

/// A source failed to read. The runner retries with a backoff.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct SourceError {
    pub message: String,
}

impl SourceError {
    pub fn new(message: impl Into<String>) -> Self {
        SourceError { message: message.into() }
    }
}

/// An external, ordered event log.
pub trait Source: Send {
    /// Up to `limit` events with positions above `after`, by position
    /// (strictly increasing). Fewer, or none, when the log has no more
    /// yet. `after` is 0 before the first event.
    fn read(&mut self, after: u64, limit: usize) -> Result<Vec<SourceEvent>, SourceError>;
}

/// An event can't be mapped (a missing field, a value of the wrong kind).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct MappingError {
    pub message: String,
}

impl MappingError {
    pub fn new(message: impl Into<String>) -> Self {
        MappingError { message: message.into() }
    }
}

/// Maps an event to the mutations that apply it: a transaction, all or
/// nothing. No mutations: the event changes nothing (its mark is still
/// committed). Must be deterministic for the exactly-once guarantee to
/// mean anything.
pub trait Mapping: Send {
    fn map(&self, event: &SourceEvent) -> Result<Vec<Mutation>, MappingError>;
}

impl<M> Mapping for M
where
    M: Fn(&SourceEvent) -> Result<Vec<Mutation>, MappingError> + Send,
{
    fn map(&self, event: &SourceEvent) -> Result<Vec<Mutation>, MappingError> {
        self(event)
    }
}

/// What to do with an event that can't be applied: its mapping fails, or
/// its mutations fail to commit (a constraint, a missing node).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnError {
    /// Stop the projection with the error; its mark stays before the
    /// event, so after fixing the cause it resumes there.
    #[default]
    Stop,
    /// Commit the event's mark alone and go on: the event is skipped (and
    /// counted).
    Skip,
}

/// How a projection runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionOptions {
    /// Events read, mapped and committed together (at least 1). A batch is
    /// one commit, unless one of its events fails.
    pub batch: usize,
    /// How long to wait before reading again once the source has no more
    /// events.
    pub poll: Duration,
    pub on_error: OnError,
    /// The longest wait between retries after a source error (the first
    /// is 1 s, then doubling).
    pub max_backoff: Duration,
}

impl Default for ProjectionOptions {
    fn default() -> Self {
        ProjectionOptions {
            batch: 100,
            poll: Duration::from_millis(500),
            on_error: OnError::Stop,
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// Where a projection commits: a namespace. [`Ns`] is one; the store's
/// projection threads use their own.
pub trait Target {
    /// The position of the mark `name`, if it is set.
    fn mark(&self, name: &MarkName) -> Result<Option<u64>, Error>;
    /// Commit `mutations` and move the mark, compare-and-set
    /// ([`Ns::commit_marked`]).
    fn commit(&self, mutations: &[Mutation], mark: &MarkUpdate) -> Result<CommitResult, Error>;
}

impl<F: LogFs + Clone + Send + Sync + 'static> Target for Ns<'_, F>
where
    F::File: Send,
{
    fn mark(&self, name: &MarkName) -> Result<Option<u64>, Error> {
        Ok(Ns::mark(self, name))
    }

    fn commit(&self, mutations: &[Mutation], mark: &MarkUpdate) -> Result<CommitResult, Error> {
        self.commit_marked(mutations, mark, &CommitOptions::default())
    }
}

/// Why a projection stopped (or, for a source error, waits).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProjectionError {
    /// The source failed to read; the runner retries.
    #[error("the source failed: {0}")]
    Source(#[from] SourceError),
    /// The source returned an event at or below the position read after,
    /// or out of order: a broken source.
    #[error("the source returned position {position} after {after}: positions must increase")]
    Order { after: u64, position: u64 },
    /// An event couldn't be mapped ([`OnError::Stop`]).
    #[error("event {position} can't be mapped: {error}")]
    Mapping { position: u64, error: MappingError },
    /// An event's mutations failed to commit ([`OnError::Stop`]).
    #[error("event {position} fails to commit: {error}")]
    Event { position: u64, error: Error },
    /// The namespace can't be written (read-only, dropped, an I/O error).
    #[error(transparent)]
    Store(Error),
}

impl ProjectionError {
    /// Whether the runner retries after it (a source error) rather than
    /// stopping.
    pub fn is_retryable(&self) -> bool {
        matches!(self, ProjectionError::Source(_))
    }
}

/// What one [`Projection::step`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// Events read from the source.
    pub read: usize,
    /// Events committed with their mutations (or as their mark alone,
    /// when their mapping gave none).
    pub applied: usize,
    /// Events committed as their mark alone because they failed
    /// ([`OnError::Skip`]).
    pub skipped: usize,
    /// Another writer moved the mark meanwhile: nothing was committed, and
    /// the next step reads after the new mark.
    pub conflict: bool,
    /// The mark after the step.
    pub mark: Option<u64>,
}

/// A projection: a source, a mapping and the name of its mark.
pub struct Projection {
    name: MarkName,
    source: Box<dyn Source>,
    mapping: Box<dyn Mapping>,
    options: ProjectionOptions,
}

impl fmt::Debug for Projection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Projection").field("name", &self.name).field("options", &self.options).finish_non_exhaustive()
    }
}

impl Projection {
    /// A projection whose mark is called `name` (a namespace can hold
    /// several, each with its own mark).
    pub fn new(
        name: MarkName,
        source: impl Source + 'static,
        mapping: impl Mapping + 'static,
        options: ProjectionOptions,
    ) -> Self {
        Projection { name, source: Box::new(source), mapping: Box::new(mapping), options }
    }

    pub fn name(&self) -> &MarkName {
        &self.name
    }

    pub fn options(&self) -> &ProjectionOptions {
        &self.options
    }

    /// One round: read up to `batch` events after the mark, map them and
    /// commit them with the mark moved to the last one. If the batch fails
    /// to commit, its events are committed one at a time, and one that
    /// fails alone is handled by [`OnError`]. Returns with
    /// [`Progress::read`] 0 when the source has nothing after the mark.
    ///
    /// Exactly once: each commit moves the mark from the position it read,
    /// so a step that runs against a stale mark (another projector with
    /// the same name) commits nothing ([`Progress::conflict`]).
    pub fn step(&mut self, target: &impl Target) -> Result<Progress, ProjectionError> {
        let mark = target.mark(&self.name).map_err(ProjectionError::Store)?;
        let after = mark.unwrap_or(0);
        let events = self.source.read(after, self.options.batch.max(1))?;
        let mut progress = Progress { read: events.len(), mark, ..Progress::default() };
        let mut previous = after;
        for event in &events {
            if event.position <= previous {
                return Err(ProjectionError::Order { after: previous, position: event.position });
            }
            previous = event.position;
        }
        let Some(last) = events.last() else { return Ok(progress) };

        // The whole batch in one commit, if every event maps
        let mapped: Result<Vec<Vec<Mutation>>, MappingError> = events.iter().map(|e| self.mapping.map(e)).collect();
        if let Ok(mapped) = mapped {
            let mutations: Vec<Mutation> = mapped.into_iter().flatten().collect();
            let update = MarkUpdate { name: self.name.clone(), expected: mark, position: last.position };
            match target.commit(&mutations, &update) {
                Ok(_) => {
                    progress.applied = events.len();
                    progress.mark = Some(last.position);
                    return Ok(progress);
                }
                Err(e) if is_conflict(&e) => {
                    progress.conflict = true;
                    return Ok(progress);
                }
                Err(e) if !is_event_error(&e) => return Err(ProjectionError::Store(e)),
                // One of the events fails: find it below
                Err(_) => {}
            }
        }

        // One event at a time
        for event in &events {
            let update = MarkUpdate { name: self.name.clone(), expected: progress.mark, position: event.position };
            let failure = match self.mapping.map(event) {
                Ok(mutations) => match target.commit(&mutations, &update) {
                    Ok(_) => {
                        progress.applied += 1;
                        progress.mark = Some(event.position);
                        continue;
                    }
                    Err(e) if is_conflict(&e) => {
                        progress.conflict = true;
                        return Ok(progress);
                    }
                    Err(e) if !is_event_error(&e) => return Err(ProjectionError::Store(e)),
                    Err(error) => ProjectionError::Event { position: event.position, error },
                },
                Err(error) => ProjectionError::Mapping { position: event.position, error },
            };
            if self.options.on_error == OnError::Stop {
                return Err(failure);
            }
            log::warn!("projection {}: skipping event {}: {}", self.name, event.position, failure);
            match target.commit(&[], &update) {
                Ok(_) => {
                    progress.skipped += 1;
                    progress.mark = Some(event.position);
                }
                Err(e) if is_conflict(&e) => {
                    progress.conflict = true;
                    return Ok(progress);
                }
                Err(e) => return Err(ProjectionError::Store(e)),
            }
        }
        Ok(progress)
    }
}

fn is_conflict(error: &Error) -> bool {
    matches!(error, Error::Engine(iwdb_engine::Error::MarkConflict { .. }))
}

/// Whether a failed commit is the fault of its mutations (so of an event),
/// rather than of the store: then the namespace is still writable, and the
/// event can be skipped.
fn is_event_error(error: &Error) -> bool {
    use iwdb_engine::Error as E;
    match error {
        Error::RecordTooLarge { .. } => true,
        Error::Engine(e) => !matches!(
            e,
            E::Poisoned
                | E::ApplyFailed { .. }
                | E::SeqExhausted
                | E::OutOfOrder { .. }
                | E::MarkConflict { .. }
                | E::TooManyMarks { .. }
                | E::InvalidMark { .. }
        ),
        _ => false,
    }
}

/// Where a projection run by the store is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectionState {
    /// Reading and committing events.
    Running,
    /// The source has no events after the mark; reading again every
    /// [`ProjectionOptions::poll`].
    CaughtUp,
    /// Waiting to retry after a source error ([`ProjectionStatus::error`]).
    Retrying,
    /// Stopped by [`ProjectionHandle::stop`] or the store's close.
    Stopped,
    /// Stopped by an error ([`ProjectionStatus::error`]).
    Failed,
}

/// A projection run by the store, as last seen by its thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionStatus {
    pub name: String,
    pub namespace: String,
    pub state: ProjectionState,
    /// The mark: the position of the last event committed.
    pub mark: Option<u64>,
    /// Events committed since the projection started (with mutations, or
    /// as their mark alone when their mapping gave none).
    pub applied: u64,
    /// Events skipped since it started ([`OnError::Skip`]).
    pub skipped: u64,
    /// The last error: why it failed or retries. Cleared by progress.
    pub error: Option<String>,
}

/// What the store and a projection's handle share with its thread.
#[derive(Debug)]
pub(crate) struct Control {
    pub(crate) status: Mutex<ProjectionStatus>,
    pub(crate) stop: Mutex<bool>,
    pub(crate) wake: Condvar,
    pub(crate) thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Control {
    pub(crate) fn new(status: ProjectionStatus) -> Self {
        Control { status: Mutex::new(status), stop: Mutex::new(false), wake: Condvar::new(), thread: Mutex::new(None) }
    }

    /// Ask the thread to stop and wait for it. Idempotent.
    pub(crate) fn stop(&self) {
        *lock(&self.stop) = true;
        self.wake.notify_all();
        let thread = lock(&self.thread).take();
        if let Some(thread) = thread {
            // A panicked thread has nothing left to clean up
            let _ = thread.join();
        }
    }

    /// Wait up to `timeout`, or until stopped. Returns whether it was
    /// stopped.
    pub(crate) fn sleep(&self, timeout: Duration) -> bool {
        let stop = lock(&self.stop);
        if *stop {
            return true;
        }
        let (stop, _) = self.wake.wait_timeout(stop, timeout).unwrap_or_else(std::sync::PoisonError::into_inner);
        *stop
    }

    pub(crate) fn stopped(&self) -> bool {
        *lock(&self.stop)
    }

    pub(crate) fn update(&self, f: impl FnOnce(&mut ProjectionStatus)) {
        f(&mut lock(&self.status));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A projection run by the store ([`Store::project`](crate::Store::project)).
/// Dropping the handle doesn't stop it: the store's close does, or
/// [`stop`](Self::stop).
#[derive(Clone, Debug)]
pub struct ProjectionHandle {
    pub(crate) control: Arc<Control>,
}

impl ProjectionHandle {
    /// Where the projection is.
    pub fn status(&self) -> ProjectionStatus {
        lock(&self.control.status).clone()
    }

    /// Stop the projection and wait for its thread: a commit in progress
    /// finishes first. Its mark stays where it is.
    pub fn stop(&self) {
        self.control.stop();
    }

    /// Wait until `f` holds for the status, or `timeout` passes. Returns
    /// the status it ended with (for tests and tools).
    pub fn wait_until(&self, timeout: Duration, f: impl Fn(&ProjectionStatus) -> bool) -> ProjectionStatus {
        let end = std::time::Instant::now() + timeout;
        loop {
            let status = self.status();
            if f(&status) || std::time::Instant::now() >= end {
                return status;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
