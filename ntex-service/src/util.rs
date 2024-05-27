use std::{future::poll_fn, future::Future, pin, task::Poll};

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
        if !ready1 {
            match pin::Pin::new(&mut fut1).poll(cx) {
                Poll::Ready(_) => ready1 = true,
                Poll::Pending => (),
            }
        }
        if !ready2 {
            match pin::Pin::new(&mut fut2).poll(cx) {
                Poll::Ready(_) => ready2 = true,
                Poll::Pending => (),
            }
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
        if !ready1 {
            match pin::Pin::new(&mut fut1).poll(cx) {
                Poll::Ready(Ok(())) => ready1 = true,
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                Poll::Pending => (),
            }
        }
        if !ready2 {
            match pin::Pin::new(&mut fut2).poll(cx) {
                Poll::Ready(Ok(())) => ready2 = true,
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                Poll::Pending => (),
            };
        }
        if ready1 && ready2 {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    })
    .await
}
