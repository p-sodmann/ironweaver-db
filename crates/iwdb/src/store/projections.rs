//! Projections run by the store (ADR 0032): a thread per projection,
//! committing through the namespace like [`Ns`] does, stopped by its
//! handle or when the store stops.

use std::sync::Arc;
use std::time::Duration;

use iwdb_engine::{CommitResult, MarkName, MarkUpdate, Mutation};
use iwdb_storage::Error;
use iwdb_storage::io::LogFs;

use super::ns::write_in;
use super::{NsState, Shared, Store, background::spawn, lock};
use crate::projection::{Control, Projection, ProjectionHandle, ProjectionState, ProjectionStatus, Target};

/// A namespace for a projection's thread: like [`Ns`](super::Ns), but
/// holding the store's shared state instead of borrowing the store.
struct Owned<F: LogFs> {
    shared: Arc<Shared<F>>,
    state: Arc<NsState<F>>,
}

impl<F: LogFs + Clone + Send + Sync + 'static> Target for Owned<F>
where
    F::File: Send,
{
    fn mark(&self, name: &MarkName) -> Result<Option<u64>, Error> {
        Ok(self.state.live.read(|ns| ns.mark(name)))
    }

    fn commit(&self, mutations: &[Mutation], mark: &MarkUpdate) -> Result<CommitResult, Error> {
        write_in(&self.shared, &self.state, |live| live.commit_marked(mutations, None, Some(mark)))
    }
}

impl<F: LogFs + Clone + Send + Sync + 'static> Store<F>
where
    F::File: Send,
{
    /// Run `projection` into the namespace `namespace` on a thread of its
    /// own (ADR 0032): read events after its mark, map them, and commit
    /// them with the mark ([`Projection::step`]), until the handle's
    /// [`stop`](ProjectionHandle::stop), the store's close, or an error
    /// that stops it ([`ProjectionState::Failed`]). Source errors are
    /// retried with a backoff (1 s, doubling, at most
    /// [`max_backoff`](crate::projection::ProjectionOptions::max_backoff)).
    ///
    /// Every event is applied at most once, whatever crashes: the mark
    /// moves in the commit that applies the events up to it, and the
    /// projection resumes from it. Two projections with the same name in a
    /// namespace don't apply an event twice either (the mark is
    /// compare-and-set), but one of them does all the work.
    ///
    /// Errors: [`Error::NoSuchNamespace`]; [`Error::Io`] if the thread
    /// can't be started.
    pub fn project(&self, namespace: &str, projection: Projection) -> Result<ProjectionHandle, Error> {
        let state = self.shared.find(namespace).ok_or_else(|| Error::NoSuchNamespace { name: namespace.to_owned() })?;
        let mark = state.live.read(|ns| ns.mark(projection.name()));
        let control = Arc::new(Control::new(ProjectionStatus {
            name: projection.name().as_str().to_owned(),
            namespace: namespace.to_owned(),
            state: ProjectionState::Running,
            mark,
            applied: 0,
            skipped: 0,
            error: None,
        }));
        let target = Owned { shared: self.shared.clone(), state };
        let thread_control = control.clone();
        let name = format!("iwdb-projection-{}", projection.name().as_str());
        // Registered first, so that a store stopping meanwhile stops it
        lock(&self.shared.projections).push(control.clone());
        let thread = match spawn(&name, move || run(projection, target, &thread_control)) {
            Ok(thread) => thread,
            Err(e) => {
                control.update(|s| {
                    s.state = ProjectionState::Failed;
                    s.error = Some(e.to_string());
                });
                return Err(e);
            }
        };
        *lock(&control.thread) = Some(thread);
        Ok(ProjectionHandle { control })
    }

    /// The projections started with [`project`](Self::project), as their
    /// threads last saw them (stopped ones too).
    pub fn projections(&self) -> Vec<ProjectionStatus> {
        lock(&self.shared.projections).iter().map(|c| lock(&c.status).clone()).collect()
    }
}

/// Stop every projection of the store and wait for their threads.
pub(super) fn stop_all<F: LogFs>(shared: &Shared<F>) {
    let controls: Vec<Arc<Control>> = lock(&shared.projections).clone();
    for control in controls {
        control.stop();
    }
}

/// A projection's thread: steps until stopped or failed.
fn run<F: LogFs + Clone + Send + Sync + 'static>(mut projection: Projection, target: Owned<F>, control: &Control)
where
    F::File: Send,
{
    let options = projection.options().clone();
    let mut backoff = Duration::from_secs(1).min(options.max_backoff);
    loop {
        if control.stopped() {
            break;
        }
        match projection.step(&target) {
            Ok(progress) => {
                backoff = Duration::from_secs(1).min(options.max_backoff);
                let caught_up = progress.read == 0;
                control.update(|s| {
                    s.mark = progress.mark;
                    s.applied += progress.applied as u64;
                    s.skipped += progress.skipped as u64;
                    s.state = if caught_up { ProjectionState::CaughtUp } else { ProjectionState::Running };
                    if progress.read > 0 || caught_up {
                        s.error = None;
                    }
                });
                if caught_up && control.sleep(options.poll) {
                    break;
                }
            }
            Err(error) if error.is_retryable() => {
                log::warn!("projection {}: {}; retrying in {:?}", projection.name(), error, backoff);
                control.update(|s| {
                    s.state = ProjectionState::Retrying;
                    s.error = Some(error.to_string());
                });
                if control.sleep(backoff) {
                    break;
                }
                backoff = (backoff * 2).min(options.max_backoff);
            }
            Err(error) => {
                log::error!("projection {} stopped: {}", projection.name(), error);
                let mark = target.mark(projection.name()).ok().flatten();
                control.update(|s| {
                    s.state = ProjectionState::Failed;
                    s.error = Some(error.to_string());
                    s.mark = mark;
                });
                return;
            }
        }
    }
    control.update(|s| s.state = ProjectionState::Stopped);
}
