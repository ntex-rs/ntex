use std::{fmt, panic::Location, pin::Pin, task::Context, task::Poll, task::ready};

use crate::remote::RemoteJoin;

type Task<T> = crate::rt::task::Task<T>;

/// The location a task was spawned at.
pub(crate) type SpawnLocation = &'static Location<'static>;

/// A native local task handle. It is neither `Send` nor `Sync`.
///
/// Returned by [`crate::rt::spawn()`] and [`crate::rt::Runtime::spawn()`]. Dropping
/// the handle detaches the task.
/// Awaiting it yields the task's output, or an error if no output is available,
/// for example after cancellation, a task panic, or runtime shutdown.
#[derive(Debug)]
pub struct JoinHandle<T> {
    task: Task<T>,
}

impl<T> JoinHandle<T> {
    pub(crate) fn new(task: Task<T>) -> Self {
        JoinHandle { task }
    }

    /// Requests cancellation and discards the result without waiting for cleanup.
    ///
    /// The future is dropped when the runtime next processes the task, or
    /// during runtime shutdown. A running poll is not interrupted.
    pub fn cancel(self) {
        self.task.cancel();
    }

    /// Detaches the task so it continues running independently.
    pub fn detach(self) {}

    /// Returns `true` if the task completed or was closed without an output.
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.task.poll_join(cx).map(|out| out.ok_or(JoinError))
    }
}

/// A native task handle returned by [`crate::rt::Handle::spawn()`].
///
/// It is neither `Send` nor `Sync` and stays on the calling thread.
/// Submissions on the runtime thread retain the local spawn fast path.
/// Dropping the handle detaches the task.
/// Awaiting it yields the output or an error if the task ended without one,
/// including when submission was rejected during shutdown.
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
    pub(crate) fn local(task: JoinHandle<T>) -> Self {
        Self {
            inner: Inner::Local(task),
        }
    }

    pub(crate) fn remote(join: RemoteJoin<T>) -> Self {
        Self {
            inner: Inner::Remote(join),
        }
    }

    /// Requests cancellation and discards the result without waiting for cleanup.
    ///
    /// An accepted future is dropped on the runtime thread, which must make
    /// progress to observe a remote cancellation request. A running poll is not
    /// interrupted.
    pub fn cancel(self) {
        match self.inner {
            Inner::Local(t) => t.cancel(),
            Inner::Remote(t) => t.cancel(),
        }
    }

    /// Detaches the task so it continues running independently.
    pub fn detach(self) {}

    /// Returns `true` if the task completed or was closed without an output.
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

/// Error returned by a native join handle when no task output is available.
///
/// The task may have been canceled, panicked, or ended during runtime shutdown.
/// This error does not retain a cancellation reason or panic payload.
#[derive(Debug, Copy, Clone)]
pub struct JoinError;

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JoinError")
    }
}

impl std::error::Error for JoinError {}

#[cfg(all(test, not(feature = "tokio"), not(feature = "compio")))]
#[path = "../tests/unit/handle.rs"]
mod tests;
