use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};
use std::{fmt, future::Future, pin::Pin};

use async_channel::Sender;

use crate::arbiter::{Arbiter, ArbiterCommand};

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
    let task = tok_io::task::spawn_local(crate::task::wrap(f));

    JoinHandle {
        task: Some(Either::Task(task)),
    }
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

#[derive(Debug)]
enum Either<T> {
    Task(tok_io::task::JoinHandle<T>),
    Spawn(crate::task::RemoteJoin<T>),
}

#[derive(Debug)]
pub struct JoinHandle<T> {
    task: Option<Either<T>>,
}

impl<T> JoinHandle<T> {
    /// Cancels the task.
    pub fn cancel(mut self) {
        match self.task.take() {
            Some(Either::Task(fut)) => fut.abort(),
            Some(Either::Spawn(fut)) => fut.cancel(),
            None => (),
        }
    }

    /// Detaches the task to let it keep running in the background.
    pub fn detach(mut self) {
        self.task.take();
    }

    /// Returns true if the current task is finished.
    pub fn is_finished(&self) -> bool {
        match &self.task {
            Some(Either::Task(fut)) => fut.is_finished(),
            Some(Either::Spawn(fut)) => fut.is_finished(),
            None => true,
        }
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        self.task.take();
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(match self.task.as_mut() {
            Some(Either::Task(fut)) => ready!(Pin::new(fut).poll(cx)).map_err(|_| JoinError),
            Some(Either::Spawn(fut)) => ready!(Pin::new(fut).poll(cx)).map_err(|_| JoinError),
            None => Err(JoinError),
        })
    }
}

#[derive(Clone, Debug)]
/// Handle to the runtime.
pub struct Handle {
    sender: Sender<ArbiterCommand>,
    rt: Option<tok_io::runtime::Handle>,
}

impl Handle {
    /// Must be called on the arbiter thread.
    pub(crate) fn new(sender: Sender<ArbiterCommand>) -> Self {
        // the arbiter runs on the single worker of a current-thread runtime
        let rt = tok_io::runtime::Handle::try_current()
            .ok()
            .filter(|h| h.runtime_flavor() == tok_io::runtime::RuntimeFlavor::CurrentThread);
        Self { sender, rt }
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

    /// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
    ///
    /// Spawning a task enables the task to execute concurrently to other tasks.
    /// There is no guarantee that a spawned task will execute to completion.
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        JoinHandle {
            task: Some(Either::Spawn(crate::task::RemoteJoin::spawn(
                &self.sender,
                future,
            ))),
        }
    }

    /// Spawns a new asynchronous task without a [`JoinHandle`].
    ///
    /// The output is dropped on the runtime thread when the task completes,
    /// so it does not have to be `Send`.
    pub fn spawn_detached<F>(&self, future: F)
    where
        F: Future + Send + 'static,
    {
        let _ = self
            .sender
            .try_send(ArbiterCommand::Execute(Box::pin(async move {
                future.await;
            })));
    }
}
