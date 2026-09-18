use std::{fmt, pin::Pin, task::Context, task::Poll, task::ready};

use async_task::Task;

#[derive(Debug)]
/// A spawned task.
pub struct JoinHandle<T> {
    task: Option<Task<T>>,
}

impl<T> JoinHandle<T> {
    pub(crate) fn new(task: Task<T>) -> Self {
        JoinHandle { task: Some(task) }
    }

    /// Cancels the task.
    pub fn cancel(mut self) {
        if let Some(t) = self.task.take() {
            drop(t.cancel());
        }
    }

    /// Detaches the task so it continues running independently.
    pub fn detach(mut self) {
        if let Some(t) = self.task.take() {
            t.detach();
        }
    }

    /// Returns `true` if the task has finished.
    pub fn is_finished(&self) -> bool {
        match &self.task {
            Some(fut) => fut.is_finished(),
            None => true,
        }
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        if let Some(fut) = self.task.take() {
            fut.detach();
        }
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(match self.task.as_mut() {
            Some(fut) => Ok(ready!(Pin::new(fut).poll(cx))),
            None => Err(JoinError),
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
