//! Utilities for futures
use std::{future::Future, mem, pin::Pin, task::Context, task::Poll};

pub use futures_core::Stream;
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

/// Creates a new future wrapping around a function returning [`Poll`].
///
/// Polling the returned future delegates to the wrapped function.
pub fn poll_fn<T, F>(f: F) -> impl Future<Output = T>
where
    F: FnMut(&mut Context<'_>) -> Poll<T>,
{
    PollFn { f }
}

/// Future for the [`poll_fn`] function.
#[must_use = "futures do nothing unless you `.await` or poll them"]
struct PollFn<F> {
    f: F,
}

impl<F> Unpin for PollFn<F> {}

impl<T, F> Future for PollFn<F>
where
    F: FnMut(&mut Context<'_>) -> Poll<T>,
{
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        (&mut self.f)(cx)
    }
}

#[doc(hidden)]
pub async fn next<S>(stream: &mut S) -> Option<S::Item>
where
    S: Stream + Unpin,
{
    stream_recv(stream).await
}

/// Creates a future that resolves to the next item in the stream.
pub async fn stream_recv<S>(stream: &mut S) -> Option<S::Item>
where
    S: Stream + Unpin,
{
    poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

#[doc(hidden)]
pub async fn send<S, I>(sink: &mut S, item: I) -> Result<(), S::Error>
where
    S: Sink<I> + Unpin,
{
    sink_write(sink, item).await
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
