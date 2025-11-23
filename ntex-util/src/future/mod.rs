//! Utilities for futures
use std::{future::Future, future::poll_fn, mem, pin::Pin, task::Context, task::Poll};

pub use futures_core::{Stream, TryFuture};

mod either;
mod join;
mod lazy;
mod on_drop;
mod ready;
mod select;

pub use self::either::Either;
pub use self::join::{join, join_all};
pub use self::lazy::{Lazy, lazy};
pub use self::on_drop::{OnDropFn, OnDropFuture, OnDropFutureExt};
pub use self::ready::Ready;
pub use self::select::select;

/// An owned dynamically typed Future for use in cases where
/// you can't statically type your result or need to add some indirection.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Creates a future that resolves to the next item in the stream.
pub async fn stream_recv<S>(stream: &mut S) -> Option<S::Item>
where
    S: Stream + Unpin,
{
    poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

enum MaybeDone<F>
where
    F: Future,
{
    Pending(F),
    Done(F::Output),
    Gone,
}

impl<F: Future> MaybeDone<F> {
    fn take_output(self: Pin<&mut Self>) -> Option<F::Output> {
        match &*self {
            Self::Done(_) => {}
            Self::Pending(_) | Self::Gone => return None,
        }
        unsafe {
            match mem::replace(self.get_unchecked_mut(), Self::Gone) {
                MaybeDone::Done(output) => Some(output),
                _ => unreachable!(),
            }
        }
    }
}

impl<F: Future> Future for MaybeDone<F> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            match self.as_mut().get_unchecked_mut() {
                MaybeDone::Pending(f) => {
                    let res = futures_core::ready!(Pin::new_unchecked(f).poll(cx));
                    self.set(Self::Done(res));
                }
                MaybeDone::Done(_) => {}
                MaybeDone::Gone => panic!("MaybeDone polled after value taken"),
            }
        }
        Poll::Ready(())
    }
}
