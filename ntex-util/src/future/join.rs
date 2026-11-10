use std::{future::Future, mem, pin::Pin, task::Context, task::Poll};

use crate::future::MaybeDone;

/// Future for the `join` combinator, waiting for two futures to complete.
pub async fn join<A, B>(fut_a: A, fut_b: B) -> (A::Output, B::Output)
where
    A: Future,
    B: Future,
{
    Join {
        fut_a: MaybeDone::pending(fut_a),
        fut_b: MaybeDone::pending(fut_b),
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
pub fn join_all<I>(i: I) -> impl Future<Output = Vec<<I::Item as Future>::Output>>
where
    I: IntoIterator,
    I::Item: Future,
{
    JoinAll {
        elems: i
            .into_iter()
            .map(|f| MaybeDone::pending(Box::pin(f)))
            .collect(),
    }
}

// Each future is boxed, so elements are `Unpin` and need no slice pin projection.
pub(crate) struct JoinAll<T: Future> {
    elems: Box<[MaybeDone<Pin<Box<T>>>]>,
}

impl<T> Future for JoinAll<T>
where
    T: Future,
{
    type Output = Vec<T::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut all_done = true;

        for elem in &mut this.elems {
            if Pin::new(elem).poll(cx).is_pending() {
                all_done = false;
            }
        }
        if all_done {
            let result = mem::take(&mut this.elems)
                .iter_mut()
                .map(|e| Pin::new(e).take_output().unwrap())
                .collect();
            Poll::Ready(result)
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::ready;

    use super::*;
    use crate::time;

    /// STATEMENT: Join helpers collect all outputs despite different completion times.
    ///
    /// HOW:
    /// - Join an immediately ready value with a sleep and compare the output pair.
    /// - Join two different-duration sleeps and check both outputs are collected.
    #[ntex::test]
    async fn join_tests() {
        let res = join(ready(Ok::<_, ()>("test")), time::sleep(time::Millis(50))).await;
        assert_eq!(res, (Ok("test"), ()));

        let res = join_all([time::sleep(time::Millis(50)), time::sleep(time::Millis(60))]).await;
        assert_eq!(res, vec![(), ()]);
    }

    /// STATEMENT: `join_all` pin-projects its boxed slice and returns every output in
    /// input order without undefined behavior.
    ///
    /// HOW:
    /// - Join three futures that stay pending for zero, one and two polls before
    ///   returning a `String`.
    /// - Poll with `Waker::noop` and check the first two polls are pending.
    /// - Check the third poll returns the outputs in input order.
    #[test]
    fn miri_join_all_collects_outputs() {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let futs = (0..3).map(|i| {
            let mut pending = i;
            std::future::poll_fn(move |_| {
                if pending == 0 {
                    Poll::Ready(i.to_string())
                } else {
                    pending -= 1;
                    Poll::Pending
                }
            })
        });
        let mut fut = std::pin::pin!(join_all(futs));
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            fut.as_mut().poll(&mut cx),
            Poll::Ready(vec!["0".to_string(), "1".to_string(), "2".to_string()])
        );
    }
}
