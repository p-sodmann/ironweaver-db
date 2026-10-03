//! [`StreamableWait`]: waiting for a namespace's streamable seq without a
//! thread, for the change stream's long poll (ADR 0031).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use iwdb_storage::Wait;
use iwdb_storage::io::LogFs;

use super::NsState;
use crate::request::{ScheduledWake, TimerHandle};

/// Resolves when the streamable seq of a namespace reaches a seq
/// ([`Wait::Reached`]), at the deadline ([`Wait::TimedOut`]) or when the
/// namespace is dropped ([`Wait::Dropped`]). It holds no thread while it
/// waits: the namespace wakes it when the seq advances, the store's timer
/// at the deadline. Dropping it unregisters both.
pub(crate) struct StreamableWait<F: LogFs> {
    state: Arc<NsState<F>>,
    seq: u64,
    deadline: Option<Instant>,
    timer: TimerHandle,
    /// The namespace's registration of the waker.
    id: Option<u64>,
    /// The timer's wake-up, and the waker it wakes.
    scheduled: Option<(ScheduledWake, Waker)>,
}

impl<F: LogFs> StreamableWait<F> {
    pub(super) fn new(state: Arc<NsState<F>>, seq: u64, deadline: Option<Instant>, timer: TimerHandle) -> Self {
        StreamableWait { state, seq, deadline, timer, id: None, scheduled: None }
    }

    fn finish(&mut self, wait: Wait) -> Poll<Wait> {
        if let Some(id) = self.id.take() {
            self.state.live.forget_waker(id);
        }
        self.scheduled = None;
        Poll::Ready(wait)
    }
}

impl<F: LogFs> Future for StreamableWait<F> {
    type Output = Wait;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Wait> {
        let this = &mut *self;
        let live = &this.state.live;
        // Register first, then look: a seq published in between wakes us
        this.id = Some(live.wake_when_streamable(this.id, this.seq, cx.waker()));
        if live.is_dropped() {
            return this.finish(Wait::Dropped);
        }
        let now = live.streamable_seq();
        if now >= this.seq {
            return this.finish(Wait::Reached(now));
        }
        if let Some(deadline) = this.deadline {
            if Instant::now() >= deadline {
                return this.finish(Wait::TimedOut(now));
            }
            if !this.scheduled.as_ref().is_some_and(|(_, w)| w.will_wake(cx.waker())) {
                let scheduled = this.timer.schedule_wake(deadline, cx.waker().clone());
                this.scheduled = Some((scheduled, cx.waker().clone()));
            }
        }
        Poll::Pending
    }
}

impl<F: LogFs> Drop for StreamableWait<F> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.state.live.forget_waker(id);
        }
    }
}
