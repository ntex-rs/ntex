//! Definition of the `Ready` (immediately finished) future
use std::{future::Future, pin::Pin, task::Context, task::Poll};

/// A future representing a value that is immediately ready.
///
/// Created by the `result` function.
#[derive(Debug, Clone)]
#[must_use = "futures do nothing unless polled"]
pub enum Ready<T, E> {
    Ok(T),
    Err(E),
    Done(Sealed),
}

#[derive(Debug, Clone)]
pub struct Sealed;

impl<T, E> Unpin for Ready<T, E> {}

impl<T, E> Future for Ready<T, E> {
    type Output = Result<T, E>;

    #[inline]
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        let result = std::mem::replace(self.get_mut(), Ready::Done(Sealed));
        match result {
            Ready::Ok(ok) => Poll::Ready(Ok(ok)),
            Ready::Err(err) => Poll::Ready(Err(err)),
            Ready::Done(_) => panic!("cannot poll completed Ready future"),
        }
    }
}

impl<T, E> From<Result<T, E>> for Ready<T, E> {
    fn from(r: Result<T, E>) -> Self {
        match r {
            Ok(v) => Ready::Ok(v),
            Err(e) => Ready::Err(e),
        }
    }
}
