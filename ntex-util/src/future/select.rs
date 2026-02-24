use std::{future::Future, pin::Pin, task::Context, task::Poll};

use crate::future::Either;

/// Waits for either one of two differently-typed futures to complete.
///
/// Polls `fut_a` first. If it is ready, returns `Either::Left` with its
/// output. Otherwise polls `fut_b` and returns `Either::Right` if it is
/// ready. If neither future is ready the current task is registered for
/// wakeup and the call returns `Poll::Pending`.
///
/// Note that `fut_a` has priority: if both futures happen to be ready on
/// the same poll, the output of `fut_a` is returned.
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

#[cfg(test)]
mod tests {
    use futures_util::future::pending;

    use super::*;
    use crate::{future::Ready, time};

    #[ntex::test]
    async fn select_tests() {
        let res = select(Ready::<_, ()>::Ok("test"), pending::<()>()).await;
        assert_eq!(res, Either::Left(Ok("test")));

        let res = select(pending::<()>(), time::sleep(time::Millis(50))).await;
        assert_eq!(res, Either::Right(()));
    }
}
