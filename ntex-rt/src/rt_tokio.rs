use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};
use std::{fmt, future::Future, marker::PhantomData, pin::Pin, rc::Rc, sync::Arc};

use crate::arbiter::Arbiter;
use crate::remote::{Job, RemoteJoin, Spawner};

/// Returns the current time.
///
/// tokio does not expose the start of a runtime turn, this is
/// [`Instant::now()`].
#[inline]
pub fn now() -> Instant {
    Instant::now()
}

#[inline]
/// Spawn a future on the current thread.
///
/// This does not create a new Arbiter or Arbiter address,
/// it is simply a helper for spawning futures on the current thread.
///
/// # Panics
///
/// This function panics if ntex system is not running.
pub fn spawn<F>(f: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
{
    JoinHandle::new(tok_io::task::spawn_local(crate::task::wrap(f)))
}

#[inline]
/// Spawns a future on the current thread without a join handle.
///
/// The output is dropped when the task completes. Unlike dropping the
/// [`JoinHandle`] returned by [`spawn()`], this does not allocate a handle.
///
/// # Panics
///
/// This function panics if ntex system is not running.
pub fn spawn_detached<F>(f: F)
where
    F: Future + 'static,
{
    drop(tok_io::task::spawn_local(crate::task::wrap(f)));
}

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
/// Returned by [`spawn()`]. Dropping the handle detaches the task.
#[derive(Debug)]
pub struct JoinHandle<T> {
    task: Option<tok_io::task::JoinHandle<T>>,
    _local: PhantomData<Rc<()>>,
}

impl<T> JoinHandle<T> {
    fn new(task: tok_io::task::JoinHandle<T>) -> Self {
        Self {
            task: Some(task),
            _local: PhantomData,
        }
    }

    /// Cancels the task.
    pub fn cancel(mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }

    /// Detaches the task to let it keep running in the background.
    pub fn detach(self) {}

    /// Returns true if the current task is finished.
    pub fn is_finished(&self) -> bool {
        match &self.task {
            Some(t) => t.is_finished(),
            None => true,
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

    /// Cancels the task, dropping its future on the runtime thread.
    pub fn cancel(self) {
        match self.inner {
            Inner::Local(t) => t.cancel(),
            Inner::Remote(t) => t.cancel(),
        }
    }

    /// Detaches the task to let it keep running in the background.
    pub fn detach(self) {}

    /// Returns true if the current task is finished.
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
/// Handle to the runtime.
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

    pub fn current() -> Self {
        Arbiter::current().handle().clone()
    }

    #[inline]
    /// Wake up runtime.
    pub fn notify(&self) {}

    /// Returns the total time the runtime has been busy.
    ///
    /// Reports tokio's `worker_total_busy_duration`, which excludes time spent
    /// handling driver events. Returns `None` if the arbiter does not run on a
    /// current-thread tokio runtime.
    pub fn busy_duration(&self) -> Option<Duration> {
        self.rt
            .as_ref()
            .map(|h| h.metrics().worker_total_busy_duration(0))
    }

    /// Returns how long the current runtime turn has been running.
    ///
    /// tokio does not expose runtime turns, always returns `None`.
    pub fn current_turn(&self) -> Option<Duration> {
        None
    }

    /// Returns the number of runtime polls, not available on tokio.
    ///
    /// Always returns `None`, the method exists for compatibility with the
    /// other runtime backends.
    pub fn poll_count(&self) -> Option<u64> {
        None
    }

    /// Returns the number of runtime polls that left scheduled tasks, not
    /// available on tokio.
    ///
    /// Always returns `None`, the method exists for compatibility with the
    /// other runtime backends.
    pub fn poll_again_count(&self) -> Option<u64> {
        None
    }

    /// Returns the number of times a task woke itself while it was polled, not
    /// available on tokio.
    ///
    /// Always returns `None`, the method exists for compatibility with the
    /// neon runtime.
    pub fn self_wake_count(&self) -> Option<u64> {
        None
    }

    /// Returns the spawn location of the task the runtime is polling, not
    /// available on tokio.
    ///
    /// Always returns `None`, the method exists for compatibility with the
    /// neon runtime.
    pub fn current_task(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    /// Returns the histogram of runtime poll durations, not available on tokio.
    ///
    /// Always returns `None`, the method exists for compatibility with the
    /// other runtime backends.
    pub fn poll_histogram(&self) -> Option<crate::Histogram> {
        None
    }

    /// Spawns a task on the handle's runtime and returns a [`RemoteJoinHandle`].
    ///
    /// There is no guarantee that a spawned task will execute to completion.
    /// On the runtime thread the task is spawned directly. From other threads
    /// the future and output must be `Send`, the future is polled and dropped
    /// on the runtime thread and its output is returned through the handle.
    /// The handle is neither `Send` nor `Sync`, dropping it detaches the task.
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
