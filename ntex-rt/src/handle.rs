use std::{fmt, panic::Location, pin::Pin, task::Context, task::Poll, task::ready};

use crate::remote::RemoteJoin;

type Task<T> = crate::rt::task::Task<T>;

/// The location a task was spawned at.
pub(crate) type SpawnLocation = &'static Location<'static>;

/// A local task handle. It is neither `Send` nor `Sync`.
///
/// Returned by [`crate::spawn()`] and [`crate::Runtime::spawn()`]. Dropping
/// the handle detaches the task.
#[derive(Debug)]
pub struct JoinHandle<T> {
    task: Task<T>,
}

impl<T> JoinHandle<T> {
    pub(crate) fn new(task: Task<T>) -> Self {
        JoinHandle { task }
    }

    /// Cancels the task.
    pub fn cancel(self) {
        self.task.cancel();
    }

    /// Detaches the task so it continues running independently.
    pub fn detach(self) {}

    /// Returns `true` if the task has finished.
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

/// A task handle returned by [`crate::Handle::spawn()`].
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

    /// Cancels the task, dropping its future on the runtime thread.
    pub fn cancel(self) {
        match self.inner {
            Inner::Local(t) => t.cancel(),
            Inner::Remote(t) => t.cancel(),
        }
    }

    /// Detaches the task so it continues running independently.
    pub fn detach(self) {}

    /// Returns `true` if the task has finished.
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

/// Error returned when a task handle no longer contains a task.
#[derive(Debug, Copy, Clone)]
pub struct JoinError;

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JoinError")
    }
}

impl std::error::Error for JoinError {}

#[cfg(all(test, not(feature = "tokio"), not(feature = "compio")))]
mod tests {
    use std::future::{pending, poll_fn};

    use crate::{Handle, System, testing::TestRunner};

    #[test]
    fn join_handle() {
        assert_eq!(size_of::<crate::JoinHandle<usize>>(), size_of::<usize>());
        System::new("test", TestRunner).block_on(async {
            let hnd = crate::spawn(pending::<()>());
            assert!(!hnd.is_finished());
            hnd.cancel();

            assert_eq!(crate::spawn(async { 1 }).await.unwrap(), 1);

            let hnd = crate::spawn(async {});
            poll_fn(|cx| {
                if hnd.is_finished() {
                    std::task::Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
            })
            .await;

            let hnd = Handle::current().clone();
            hnd.notify().unwrap();
        });
        assert_eq!(super::JoinError.to_string(), "JoinError");
        assert_eq!(crate::rt_default::JoinError.to_string(), "JoinError");
    }
}
