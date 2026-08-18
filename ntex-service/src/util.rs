use std::{future::Future, future::poll_fn, pin, pin::Pin, task::Poll};

use crate::{ReadyCtx, Service};

pub(crate) async fn shutdown<A, B, St>(svc1: &A, svc2: &B)
where
    A: Service<St>,
    B: Service<St>,
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
            ready2 = true;
        }
        if ready1 && ready2 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

pub(crate) async fn ready<S, St, A, B>(
    svc1: &A,
    svc2: &B,
    ctx: ReadyCtx<'_, S, St>,
) -> Result<(), A::Error>
where
    S: Service<St>,
    A: Service<St>,
    B: Service<St, Error = A::Error>,
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
