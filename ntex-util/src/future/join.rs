use std::{future::Future, mem, pin::Pin, task::Context, task::Poll};

use crate::future::MaybeDone;

/// Future for the `join` combinator, waiting for two futures to complete.
pub async fn join<A, B>(fut_a: A, fut_b: B) -> (A::Output, B::Output)
where
    A: Future,
    B: Future,
{
    Join {
        fut_a: MaybeDone::Pending(fut_a),
        fut_b: MaybeDone::Pending(fut_b),
    }
    .await
}

pin_project_lite::pin_project! {
    pub(crate) struct Join<A: Future, B: Future> {
        #[pin]
        fut_a: MaybeDone<A>,
        #[pin]
        fut_b: MaybeDone<B>,
    }
}

impl<A, B> Future for Join<A, B>
where
    A: Future,
    B: Future,
{
    type Output = (A::Output, B::Output);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        let mut all_done = true;
        all_done &= this.fut_a.as_mut().poll(cx).is_ready();
        all_done &= this.fut_b.as_mut().poll(cx).is_ready();
        if all_done {
            Poll::Ready((
                this.fut_a.take_output().unwrap(),
                this.fut_b.take_output().unwrap(),
            ))
        } else {
            Poll::Pending
        }
    }
}

/// Creates a future which represents a collection of the outputs of the futures
/// given.
pub async fn join_all<I>(i: I) -> Vec<<I::Item as Future>::Output>
where
    I: IntoIterator,
    I::Item: Future,
{
    let elems: Box<[_]> = i.into_iter().map(MaybeDone::Pending).collect();
    JoinAll {
        elems: elems.into(),
    }
    .await
}

pub(crate) struct JoinAll<T: Future> {
    elems: Pin<Box<[MaybeDone<T>]>>,
}

fn iter_pin_mut<T>(slice: Pin<&mut [T]>) -> impl Iterator<Item = Pin<&mut T>> {
    // Safety: `std` _could_ make this unsound if it were to decide Pin's
    // invariants aren't required to transmit through slices. Otherwise this has
    // the same safety as a normal field pin projection.
    unsafe { slice.get_unchecked_mut() }
        .iter_mut()
        .map(|t| unsafe { Pin::new_unchecked(t) })
}

impl<T> Future for JoinAll<T>
where
    T: Future,
{
    type Output = Vec<T::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut all_done = true;

        for elem in iter_pin_mut(self.elems.as_mut()) {
            if elem.poll(cx).is_pending() {
                all_done = false;
            }
        }
        if all_done {
            let mut elems = mem::replace(&mut self.elems, Box::pin([]));
            let result = iter_pin_mut(elems.as_mut())
                .map(|e| e.take_output().unwrap())
                .collect();
            Poll::Ready(result)
        } else {
            Poll::Pending
        }
    }
}
