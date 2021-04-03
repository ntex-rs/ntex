use std::{future::Future, pin::Pin, task::Context, task::Poll};

use crate::future::Either;

/// Waits for either one of two differently-typed futures to complete.
pub async fn select<A, B>(fut_a: A, fut_b: B) -> Either<A::Output, B::Output>
where
    A: Future,
    B: Future,
{
    Select { fut_a, fut_b }.await
}

pin_project_lite::pin_project! {
    pub(crate) struct Select<A, B> {
        #[pin]
        fut_a: A,
        #[pin]
        fut_b: B,
    }
}

impl<A, B> Future for Select<A, B>
where
    A: Future,
    B: Future,
{
    type Output = Either<A::Output, B::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        if let Poll::Ready(item) = this.fut_a.poll(cx) {
            return Poll::Ready(Either::Left(item));
        }

        if let Poll::Ready(item) = this.fut_b.poll(cx) {
            return Poll::Ready(Either::Right(item));
        }

        Poll::Pending
    }
}
