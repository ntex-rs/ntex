use std::{future::Future, future::poll_fn, pin, pin::Pin, task::Poll};

use crate::{Ctx, Service};

pub(crate) type BoxFuture<'a, R> = Pin<Box<dyn Future<Output = R> + 'a>>;

pub(crate) async fn shutdown<S, St, A, B, RA, RB>(svc1: &A, svc2: &B, ctx: Ctx<'_, S, St>)
where
    A: Service<St, RA>,
    B: Service<St, RB>,
{
    let mut fut1 = pin::pin!(ctx.shutdown(svc1));
    let mut fut2 = pin::pin!(ctx.shutdown(svc2));

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

pub(crate) async fn ready<S, St, A, B, RA, RB>(
    svc1: &A,
    svc2: &B,
    ctx: Ctx<'_, S, St>,
) -> Result<(), A::Error>
where
    A: Service<St, RA>,
    B: Service<St, RB, Error = A::Error>,
{
    let mut fut1 = pin::pin!(Some(ctx.ready_generation(svc1)));
    let mut fut2 = pin::pin!(Some(ctx.ready_generation(svc2)));
    let scope = ctx.scope();
    let mut generation1 = scope.generation();
    let mut generation2 = scope.generation();

    poll_fn(|cx| {
        if is_idle(&mut fut1) && !scope.is_current(generation1) {
            fut1.set(Some(ctx.ready_generation(svc1)));
        }
        if let Some(fut) = fut1.as_mut().as_pin_mut()
            && let Poll::Ready(generation) = fut.poll(cx)?
        {
            generation1 = generation;
            fut1.set(None);
        }

        if is_idle(&mut fut2) && !scope.is_current(generation2) {
            fut2.set(Some(ctx.ready_generation(svc2)));
        }
        if let Some(fut) = fut2.as_mut().as_pin_mut()
            && let Poll::Ready(generation) = fut.poll(cx)?
        {
            generation2 = generation;
            fut2.set(None);
        }

        let current1 = scope.is_current(generation1);
        let current2 = scope.is_current(generation2);
        if is_idle(&mut fut1) && is_idle(&mut fut2) && current1 && current2 {
            Poll::Ready(Ok(()))
        } else {
            // A later poll or future drop can invalidate a completed sibling.
            // Retain pending work, but schedule another check for the dirty child.
            if (is_idle(&mut fut1) && !current1) || (is_idle(&mut fut2) && !current2) {
                cx.waker().wake_by_ref();
            }
            Poll::Pending
        }
    })
    .await
}

// `Option::is_none()` would take `&Option<F>`, which retags the whole future
// and invalidates borrows it holds into itself.
fn is_idle<F>(fut: &mut Pin<&mut Option<F>>) -> bool {
    fut.as_mut().as_pin_mut().is_none()
}

#[cfg(test)]
#[path = "../tests/unit/util.rs"]
pub(crate) mod tests;
