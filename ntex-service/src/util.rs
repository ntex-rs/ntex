use std::{future::poll_fn, future::Future, pin, pin::Pin, task::Poll};

use crate::{Service, ServiceCtx};

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
        if !ready1 && Pin::new(&mut fut1).poll(cx).is_ready() {
            ready1 = true;
        }
        if !ready2 && Pin::new(&mut fut2).poll(cx).is_ready() {
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

pub(crate) async fn ready<S, A, AR, B, BR>(
    svc1: &A,
    svc2: &B,
    ctx: ServiceCtx<'_, S>,
) -> Result<(), A::Error>
where
    A: Service<AR>,
    B: Service<BR, Error = A::Error>,
{
    let mut fut1 = pin::pin!(ctx.ready(svc1));
    let mut fut2 = pin::pin!(ctx.ready(svc2));

    let mut ready1 = false;
    let mut ready2 = false;

    poll_fn(move |cx| {
        if !ready1 && Pin::new(&mut fut1).poll(cx)?.is_ready() {
            ready1 = true;
        }
        if !ready2 && Pin::new(&mut fut2).poll(cx)?.is_ready() {
            ready2 = true;
        }
        if ready1 && ready2 {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    })
    .await
}

/// Waits for either one of two differently-typed futures to complete.
pub(crate) async fn select<A, B>(fut1: A, fut2: B) -> A::Output
where
    A: Future,
    B: Future<Output = A::Output>,
{
    let mut fut1 = pin::pin!(fut1);
    let mut fut2 = pin::pin!(fut2);

    poll_fn(|cx| {
        if let Poll::Ready(item) = Pin::new(&mut fut1).poll(cx) {
            return Poll::Ready(item);
        }
        if let Poll::Ready(item) = Pin::new(&mut fut2).poll(cx) {
            return Poll::Ready(item);
        }
        Poll::Pending
    })
    .await
}
