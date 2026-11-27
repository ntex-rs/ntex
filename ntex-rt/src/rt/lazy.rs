//! A deadline that is checked at turn starts and never wakes an idle runtime.
use std::cell::Cell;
use std::task::{Context, Poll, Waker};
use std::{fmt, sync::Arc, time::Instant};

use crate::watchdog::Watchdog;

/// A deadline checked when a runtime turn starts.
///
/// Unlike a timer, the deadline does not wake an idle runtime and is not part
/// of the [`Pending`](crate::PollResult::Pending) timeout: it expires at the
/// start of the first turn after it. The latency sampler uses it.
#[derive(Default)]
pub(crate) struct LazyTimer {
    deadline: Cell<Option<Instant>>,
    /// The runtime was idle since the last turn start.
    slept: Cell<bool>,
    /// Effective deadline of the expired timer, not yet taken.
    expired: Cell<Option<Instant>>,
    waker: Cell<Option<Waker>>,
    /// Notified when the deadline expires.
    watchdog: Cell<Option<Arc<Watchdog>>>,
}

impl LazyTimer {
    /// Arms the timer, a pending expiration is discarded.
    pub(crate) fn reset(&self, deadline: Instant) {
        self.deadline.set(Some(deadline));
        self.slept.set(false);
        self.expired.set(None);
    }

    /// Returns the effective deadline once the timer expired.
    ///
    /// The effective deadline is the deadline, or the start of the turn that
    /// expired the timer if the runtime was idle at the deadline.
    pub(crate) fn poll_expired(&self, cx: &mut Context<'_>) -> Poll<Instant> {
        if let Some(at) = self.expired.take() {
            Poll::Ready(at)
        } else {
            let waker = match self.waker.take() {
                Some(w) if w.will_wake(cx.waker()) => w,
                _ => cx.waker().clone(),
            };
            self.waker.set(Some(waker));
            Poll::Pending
        }
    }

    /// Sets the watchdog notified when the timer expires.
    pub(crate) fn set_watchdog(&self, watchdog: Arc<Watchdog>) {
        self.watchdog.set(Some(watchdog));
    }

    /// Expires a due timer, called at the start of each turn.
    #[inline]
    pub(super) fn turn(&self, start: Instant) {
        if let Some(deadline) = self.deadline.get() {
            if deadline <= start {
                self.expire(deadline, start);
            } else {
                self.slept.set(false);
            }
        }
    }

    #[cold]
    fn expire(&self, deadline: Instant, start: Instant) {
        // the runtime was busy at the deadline unless it was idle before this
        // turn, a turn that started before the deadline would have expired it
        let at = if self.slept.replace(false) { start } else { deadline };
        self.deadline.set(None);
        self.expired.set(Some(at));
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
        if let Some(watchdog) = self.watchdog.take() {
            watchdog.notify();
            self.watchdog.set(Some(watchdog));
        }
    }

    /// Returns `true` if the timer is armed and due at `now`.
    #[inline]
    pub(super) fn is_due(&self, now: Instant) -> bool {
        self.deadline.get().is_some_and(|d| d <= now)
    }

    /// Marks the runtime idle.
    #[inline]
    pub(super) fn sleep(&self) {
        if self.deadline.get().is_some() {
            self.slept.set(true);
        }
    }

    /// Drops the stored waker and watchdog.
    pub(super) fn clear(&self) {
        self.deadline.set(None);
        drop(self.waker.take());
        drop(self.watchdog.take());
    }
}

impl fmt::Debug for LazyTimer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LazyTimer")
            .field("deadline", &self.deadline.get())
            .field("expired", &self.expired.get())
            .finish_non_exhaustive()
    }
}
