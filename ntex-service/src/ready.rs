//! Definition of the `Ready` (immediately finished) future
use std::{future::Future, pin::Pin, task::Context, task::Poll};

/// A future representing a value that is immediately ready.
///
/// Created by the `result` function.
#[derive(Debug, Clone)]
#[must_use = "futures do nothing unless polled"]
pub struct Ready<T, E>(Option<Result<T, E>>);

impl<T, E> Ready<T, E> {
    #[inline]
    /// Creates a new "leaf future" which will resolve with the given result.
    pub fn result(r: Result<T, E>) -> Ready<T, E> {
        Ready(Some(r))
    }

    #[inline]
    /// Creates a "leaf future" from an immediate value of a finished and
    /// successful computation.
    pub fn ok(t: T) -> Ready<T, E> {
        Self(Some(Ok(t)))
    }

    #[inline]
    /// Creates a "leaf future" from an immediate value of a failed computation.
    pub fn err(e: E) -> Ready<T, E> {
        Self(Some(Err(e)))
    }
}

impl<T, E> Unpin for Ready<T, E> {}

impl<T, E> Future for Ready<T, E> {
    type Output = Result<T, E>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(self.0.take().expect("cannot poll Ready future twice"))
    }
}

impl<T, E> From<Result<T, E>> for Ready<T, E> {
    fn from(r: Result<T, E>) -> Self {
        Self::result(r)
    }
}
