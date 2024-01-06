//! Utilities for futures
use std::{future::poll_fn, future::Future, mem, pin::Pin, task::Context, task::Poll};

pub use futures_core::{Stream, TryFuture};
pub use futures_sink::Sink;

mod either;
mod join;
mod lazy;
mod ready;
mod select;

pub use self::either::Either;
pub use self::join::{join, join_all};
pub use self::lazy::{lazy, Lazy};
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

/// A future that completes after the given item has been fully processed
/// into the sink, including flushing.
pub async fn sink_write<S, I>(sink: &mut S, item: I) -> Result<(), S::Error>
where
    S: Sink<I> + Unpin,
{
    poll_fn(|cx| Pin::new(&mut *sink).poll_ready(cx)).await?;
    Pin::new(&mut *sink).start_send(item)?;
    poll_fn(|cx| Pin::new(&mut *sink).poll_flush(cx)).await
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
