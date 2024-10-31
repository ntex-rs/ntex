use std::{future::poll_fn, future::Future, marker, pin, task::Context, task::Poll};

use crate::Service;

pub(crate) struct Ready<E>(marker::PhantomData<E>);

impl<E> Future for Ready<E> {
    type Output = Result<(), E>;

    fn poll(self: pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Pending
    }
}

pub(crate) async fn shutdown<A, AR, B, BR>(svc1: &A, svc2: &B)
where
    A: Service<AR>,
    B: Service<BR>,
{
    let mut fut1 = pin::pin!(svc1.shutdown());
    let mut fut2 = pin::pin!(svc2.shutdown());

    let mut ready1 = false;
    let mut ready2 = false;

    poll_fn(move |cx| {
        if !ready1 && pin::Pin::new(&mut fut1).poll(cx).is_ready() {
            ready1 = true;
        }
        if !ready2 && pin::Pin::new(&mut fut2).poll(cx).is_ready() {
            ready2 = true
        }
        if ready1 && ready2 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await
}

pub(crate) async fn ready<'a, A, AR, B, BR>(
    svc1: &'a A,
    svc2: &'a B,
) -> Option<impl Future<Output = Result<(), A::Error>> + use<'a, A, AR, B, BR>>
where
    A: Service<AR>,
    B: Service<BR, Error = A::Error>,
{
    let mut fut1 = pin::pin!(svc1.ready());
    let mut fut2 = pin::pin!(svc2.ready());

    let mut ready1 = false;
    let mut ready2 = false;

    let mut r_fut1 = None;
    let mut r_fut2 = None;

    poll_fn(|cx| {
        if !ready1 {
            if let Poll::Ready(res) = pin::Pin::new(&mut fut1).poll(cx) {
                r_fut1 = res;
                ready1 = true;
            }
        }
        if !ready2 {
            if let Poll::Ready(res) = pin::Pin::new(&mut fut2).poll(cx) {
                r_fut2 = res;
                ready2 = true;
            }
        }
        if ready1 && ready2 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;

    if r_fut1.is_none() && r_fut2.is_none() {
        None
    } else {
        Some(async move {
            match (r_fut1, r_fut2) {
                (Some(fut), None) => fut.await?,
                (None, Some(fut)) => fut.await?,
                (Some(fut1), Some(fut2)) => match select(fut1, fut2).await {
                    Either::Left(res) => res?,
                    Either::Right(res) => res?,
                },
                (None, None) => (),
            }
            Ok(())
        })
    }
}

pub(crate) enum Either<A, B> {
    /// First branch of the type
    Left(A),
    /// Second branch of the type
    Right(B),
}

/// Waits for either one of two differently-typed futures to complete.
pub(crate) async fn select<A, B>(fut_a: A, fut_b: B) -> Either<A::Output, B::Output>
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

    fn poll(self: pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
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
