use std::{future::Future, pin::Pin, task::Context, task::Poll};

pub mod buffer;
pub mod counter;
mod extensions;
pub mod inflight;
pub mod keepalive;
pub mod sink;
pub mod stream;
pub mod time;
pub mod timeout;
pub mod variant;

pub use self::extensions::Extensions;

pub use ntex_service::util::{lazy, Either, Lazy, Ready};

pub use bytes::{Buf, BufMut, Bytes, BytesMut};
pub use bytestring::ByteString;

pub type HashMap<K, V> = std::collections::HashMap<K, V, ahash::RandomState>;
pub type HashSet<V> = std::collections::HashSet<V, ahash::RandomState>;

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

/// Creates a future that resolves to the next item in the stream.
pub async fn next<S>(stream: &mut S) -> Option<S::Item>
where
    S: crate::Stream + Unpin,
{
    poll_fn(|cx| Pin::new(&mut *stream).poll_next(cx)).await
}

/// A future that completes after the given item has been fully processed
/// into the sink, including flushing.
pub async fn send<S, I>(sink: &mut S, item: I) -> Result<(), S::Error>
where
    S: crate::Sink<I> + Unpin,
{
    poll_fn(|cx| Pin::new(&mut *sink).poll_ready(cx)).await?;
    Pin::new(&mut *sink).start_send(item)?;
    poll_fn(|cx| Pin::new(&mut *sink).poll_flush(cx)).await
}

/// Future for the `join` combinator, waiting for two futures to
/// complete.
pub async fn join<A, B>(fut_a: A, fut_b: B) -> (A::Output, B::Output)
where
    A: Future,
    B: Future,
{
    tokio::pin!(fut_a);
    tokio::pin!(fut_b);

    let mut res_a = None;
    let mut res_b = None;

    poll_fn(|cx| {
        if res_a.is_none() {
            if let Poll::Ready(item) = Pin::new(&mut fut_a).poll(cx) {
                res_a = Some(item)
            }
        }
        if res_b.is_none() {
            if let Poll::Ready(item) = Pin::new(&mut fut_b).poll(cx) {
                res_b = Some(item)
            }
        }
        if res_a.is_some() && res_b.is_some() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;

    (res_a.unwrap(), res_b.unwrap())
}

/// Waits for either one of two differently-typed futures to complete.
pub async fn select<A, B>(fut_a: A, fut_b: B) -> Either<A::Output, B::Output>
where
    A: Future,
    B: Future,
{
    tokio::pin!(fut_a);
    tokio::pin!(fut_b);

    poll_fn(|cx| {
        if let Poll::Ready(item) = Pin::new(&mut fut_a).poll(cx) {
            Poll::Ready(Either::Left(item))
        } else if let Poll::Ready(item) = Pin::new(&mut fut_b).poll(cx) {
            Poll::Ready(Either::Right(item))
        } else {
            Poll::Pending
        }
    })
    .await
}

enum MaybeDone<T>
where
    T: Future,
{
    Pending(T),
    Done(T::Output),
}

/// Creates a future which represents a collection of the outputs of the futures given.
pub async fn join_all<F, T>(fut: Vec<F>) -> Vec<T>
where
    F: Future<Output = T>,
{
    let mut futs: Vec<_> = fut
        .into_iter()
        .map(|f| MaybeDone::Pending(Box::pin(f)))
        .collect();

    poll_fn(|cx| {
        let mut pending = false;
        for item in &mut futs {
            if let MaybeDone::Pending(ref mut fut) = item {
                if let Poll::Ready(res) = fut.as_mut().poll(cx) {
                    *item = MaybeDone::Done(res);
                } else {
                    pending = true;
                }
            }
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;

    futs.into_iter()
        .map(|item| {
            if let MaybeDone::Done(item) = item {
                item
            } else {
                unreachable!()
            }
        })
        .collect()
}
