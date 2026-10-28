use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};
use std::{fmt, future::Future, marker::PhantomData, pin::Pin, rc::Rc, sync::Arc};

use crate::arbiter::Arbiter;
use crate::metrics::Histogram;
use crate::remote::{Job, RemoteJoin, Spawner};

/// Returns the current time.
///
/// Tokio does not expose the start of a runtime turn, so this is
/// [`Instant::now()`].
#[inline]
pub fn now() -> Instant {
    Instant::now()
}

#[inline]
/// Spawns a future on the current Tokio local task set and returns its join handle.
///
/// The future and output need not be `Send`. Registered task callbacks are
/// applied. Dropping the handle detaches the task unless
/// [`cancel_on_drop`](JoinHandle::cancel_on_drop) is enabled. The local task set
/// must keep being driven for it to make progress. An ntex system is not required.
///
/// # Panics
///
/// Panics outside a Tokio context that supports local spawning, such as a
/// [`LocalSet`](tok_io::task::LocalSet).
pub fn spawn<F>(f: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
{
    JoinHandle::new(tok_io::task::spawn_local(crate::task::wrap(f)))
}

#[inline]
/// Spawns a future on the current thread without a join handle.
///
/// Like [`spawn()`], this applies registered task callbacks and supports
/// non-`Send` futures and outputs. The output is discarded when the task
/// completes. No ntex join handle is returned.
///
/// # Panics
///
/// Panics outside a Tokio context that supports local spawning, such as a
/// [`LocalSet`](tok_io::task::LocalSet).
pub fn spawn_detached<F>(f: F)
where
    F: Future + 'static,
{
    drop(tok_io::task::spawn_local(crate::task::wrap(f)));
}

/// Error returned when a Tokio-backed task ends without an output.
///
/// Cancellation and task panics are reported without preserving their cause.
#[derive(Debug, Copy, Clone)]
pub struct JoinError;

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JoinError")
    }
}

impl std::error::Error for JoinError {}

/// A local task handle. It is neither `Send` nor `Sync`.
///
/// Returned by [`spawn()`]. Dropping the handle detaches the task unless
/// [`cancel_on_drop`](Self::cancel_on_drop) is enabled.
/// Awaiting it returns the task output or [`JoinError`] on cancellation or panic.
#[derive(Debug)]
pub struct JoinHandle<T> {
    task: Option<tok_io::task::JoinHandle<T>>,
    cancel_on_drop: bool,
    _local: PhantomData<Rc<()>>,
}

impl<T> JoinHandle<T> {
    fn new(task: tok_io::task::JoinHandle<T>) -> Self {
        Self {
            task: Some(task),
            cancel_on_drop: false,
            _local: PhantomData,
        }
    }

    /// Requests cancellation when this handle is dropped instead of detaching the task.
    ///
    /// Cancellation follows [`cancel`](Self::cancel) and does not wait for cleanup.
    /// A running poll is not interrupted. The deprecated [`detach`](Self::detach)
    /// method still overrides this setting.
    #[must_use]
    pub fn cancel_on_drop(mut self) -> Self {
        self.cancel_on_drop = true;
        self
    }

    /// Requests cancellation and discards the result without waiting for cleanup.
    ///
    /// A running poll is not interrupted; the local task set must make progress
    /// for cancellation of a pending task to finish.
    pub fn cancel(mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }

    /// Detaches the task to let it keep running in the background.
    ///
    /// Overrides [`cancel_on_drop`](Self::cancel_on_drop) without requesting cancellation.
    #[deprecated(
        since = "4.5.0",
        note = "Drop the handle instead if cancel_on_drop() is not enabled"
    )]
    pub fn detach(mut self) {
        self.cancel_on_drop = false;
    }

    /// Returns `true` if the task completed, panicked, or finished cancellation.
    pub fn is_finished(&self) -> bool {
        match &self.task {
            Some(t) => t.is_finished(),
            None => true,
        }
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        if self.cancel_on_drop
            && let Some(t) = &self.task
        {
            t.abort();
        }
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(match self.task.as_mut() {
            Some(t) => ready!(Pin::new(t).poll(cx)).map_err(|_| JoinError),
            None => Err(JoinError),
        })
    }
}

/// A task handle returned by [`Handle::spawn()`].
///
/// It is neither `Send` nor `Sync` and stays on the calling thread.
/// Submissions on the runtime thread retain the local spawn fast path.
/// Dropping the handle detaches the task.
/// Awaiting it returns the output or [`JoinError`] if the task ended without
/// one, including when submission was rejected during shutdown.
#[derive(Debug)]
pub struct RemoteJoinHandle<T> {
    inner: Inner<T>,
}

#[derive(Debug)]
enum Inner<T> {
    Local(JoinHandle<T>),
    Remote(RemoteJoin<T>),
}

impl<T> RemoteJoinHandle<T> {
    fn local(task: JoinHandle<T>) -> Self {
        Self {
            inner: Inner::Local(task),
        }
    }

    fn remote(join: RemoteJoin<T>) -> Self {
        Self {
            inner: Inner::Remote(join),
        }
    }

    /// Requests cancellation and discards the result without waiting for cleanup.
    ///
    /// Accepted futures are dropped on the runtime thread. A running poll is
    /// not interrupted, and the runtime must make progress to finish cancellation.
    pub fn cancel(self) {
        match self.inner {
            Inner::Local(t) => t.cancel(),
            Inner::Remote(t) => t.cancel(),
        }
    }

    /// Detaches the task to let it keep running in the background.
    ///
    /// This method does nothing beyond dropping the handle; dropping it already
    /// detaches the task without canceling it.
    #[deprecated(since = "4.5.0", note = "Use drop(handle) instead")]
    pub fn detach(self) {}

    /// Returns `true` if the task completed or ended without an output.
    pub fn is_finished(&self) -> bool {
        match &self.inner {
            Inner::Local(t) => t.is_finished(),
            Inner::Remote(t) => t.is_finished(),
        }
    }
}

impl<T> Future for RemoteJoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(match &mut self.inner {
            Inner::Local(t) => ready!(Pin::new(t).poll(cx)),
            Inner::Remote(t) => ready!(t.poll_join(cx)).ok_or(JoinError),
        })
    }
}

#[derive(Clone, Debug)]
/// A thread-safe handle for submitting work to a Tokio-backed arbiter.
///
/// Clones share the same target arbiter; they do not create another runtime.
pub struct Handle {
    spawner: Arc<Spawner>,
    rt: Option<tok_io::runtime::Handle>,
}

impl Handle {
    /// Must be called on the arbiter thread, inside its runtime.
    pub(crate) fn new() -> Self {
        // the arbiter runs on the single worker of a current-thread runtime
        let rt = tok_io::runtime::Handle::try_current()
            .ok()
            .filter(|h| h.runtime_flavor() == tok_io::runtime::RuntimeFlavor::CurrentThread);
        Self {
            spawner: Spawner::start(),
            rt,
        }
    }

    /// Returns a handle to the current arbiter's runtime.
    ///
    /// # Panics
    ///
    /// Panics if no arbiter is installed on the current thread. A Tokio runtime
    /// context alone is not sufficient.
    pub fn current() -> Self {
        Arbiter::current().handle().clone()
    }

    #[inline]
    /// Compatibility hook that does nothing on the Tokio backend.
    ///
    /// Task submission and task wakers notify Tokio automatically.
    pub fn notify(&self) {}

    /// Returns the total time the runtime has been busy.
    ///
    /// Reports Tokio's `worker_total_busy_duration`, which excludes time spent
    /// handling driver events. Returns `None` if the arbiter does not run on a
    /// current-thread Tokio runtime.
    pub fn busy_duration(&self) -> Option<Duration> {
        self.rt
            .as_ref()
            .map(|h| h.metrics().worker_total_busy_duration(0))
    }

    /// Returns how long the current runtime turn has been running.
    ///
    /// Tokio does not expose runtime turns; always returns `None`.
    pub fn current_turn(&self) -> Option<Duration> {
        None
    }

    /// Returns the number of runtime polls, not available on Tokio.
    ///
    /// Always returns `None`, the method exists for compatibility with the
    /// other runtime backends.
    pub fn poll_count(&self) -> Option<u64> {
        None
    }

    /// Returns the number of runtime polls that left scheduled tasks, not
    /// available on Tokio.
    ///
    /// Always returns `None`, the method exists for compatibility with the
    /// other runtime backends.
    pub fn poll_again_count(&self) -> Option<u64> {
        None
    }

    /// Returns the number of times a task woke itself while it was polled, not
    /// available on Tokio.
    ///
    /// Always returns `None`, the method exists for compatibility with the
    /// native runtime.
    pub fn self_wake_count(&self) -> Option<u64> {
        None
    }

    /// Returns the spawn location of the task the runtime is polling, not
    /// available on Tokio.
    ///
    /// Always returns `None`, the method exists for compatibility with the
    /// native runtime.
    pub fn current_task(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    /// Returns the histogram of runtime poll durations, not available on Tokio.
    ///
    /// Always returns `None`, the method exists for compatibility with the
    /// other runtime backends.
    pub fn poll_histogram(&self) -> Option<Histogram> {
        None
    }

    /// Spawns a task on the handle's runtime and returns a [`RemoteJoinHandle`].
    ///
    /// There is no guarantee that a spawned task will execute to completion.
    /// Both the future and output must be `Send`, even when submitted on the
    /// runtime thread, which uses the local spawn fast path. Accepted futures
    /// are polled and dropped on the runtime thread, with the output transferred
    /// through the join handle. Rejected submissions are never polled and their
    /// futures may be dropped on the submitting thread.
    ///
    /// The returned join handle is neither `Send` nor `Sync`; dropping it
    /// detaches the task.
    ///
    /// # Panics
    ///
    /// Panics on the owner thread if there is no Tokio context supporting
    /// local spawning. Submissions from other threads do not require one.
    pub fn spawn<F>(&self, future: F) -> RemoteJoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if self.spawner.is_owner() {
            RemoteJoinHandle::local(spawn(future))
        } else {
            let job = Job::new(future, true);
            let join = RemoteJoin::new(job.clone());
            self.spawner.submit(job);
            RemoteJoinHandle::remote(join)
        }
    }

    /// Spawns a new asynchronous task without a join handle.
    ///
    /// The output is dropped on the runtime thread when the task completes,
    /// so it does not have to be `Send`.
    /// The future must be `Send`, even on the owner thread. Rejected submissions
    /// are never polled and their futures may be dropped on the submitting thread.
    ///
    /// # Panics
    ///
    /// Panics on the owner thread if there is no Tokio context supporting
    /// local spawning. Submissions from other threads do not require one.
    pub fn spawn_detached<F>(&self, future: F)
    where
        F: Future + Send + 'static,
    {
        if self.spawner.is_owner() {
            spawn_detached(future);
        } else {
            self.spawner.submit(Job::new(
                async move {
                    future.await;
                },
                false,
            ));
        }
    }
}
