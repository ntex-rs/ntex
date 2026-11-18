use std::{future::Future, future::poll_fn, pin, pin::Pin, task::Context, task::Poll};

use crate::{Ctx, Service, readiness::Generation};

pub(crate) type BoxFuture<'a, R> = Pin<Box<dyn Future<Output = R> + 'a>>;

pin_project_lite::pin_project! {
    /// Waits for readiness, then calls the service.
    ///
    /// Unlike an `async` block, the call state does not keep storage for the
    /// request after it is moved into the service call. `ready` runs on the first
    /// poll and returns `None` if no readiness wait is needed.
    ///
    /// The readiness wait is boxed so that its storage, which grows with each
    /// nested readiness level, is not inlined into every call future. It is only
    /// allocated when the scope is not ready.
    #[project = ReadyThenCallProj]
    pub(crate) enum ReadyThenCall<Req, R, RF, C, CF> {
        Init { req: Option<Req>, ready: Option<R>, call: Option<C> },
        Ready { fut: Pin<Box<RF>>, req: Option<Req>, call: Option<C> },
        Call { #[pin] fut: CF },
    }
}

impl<Req, R, RF, C, CF> ReadyThenCall<Req, R, RF, C, CF> {
    pub(crate) fn new(req: Req, ready: R, call: C) -> Self {
        ReadyThenCall::Init {
            req: Some(req),
            ready: Some(ready),
            call: Some(call),
        }
    }
}

impl<Req, R, RF, C, CF, T, E> Future for ReadyThenCall<Req, R, RF, C, CF>
where
    R: FnOnce() -> Option<RF>,
    RF: Future<Output = Result<(), E>>,
    C: FnOnce(Req) -> CF,
    CF: Future<Output = Result<T, E>>,
{
    type Output = Result<T, E>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let next = match self.as_mut().project() {
                ReadyThenCallProj::Init { req, ready, call } => {
                    let (req, call) = (req.take().unwrap(), call.take().unwrap());
                    match ready.take().unwrap()() {
                        Some(fut) => ReadyThenCall::Ready {
                            fut: Box::pin(fut),
                            req: Some(req),
                            call: Some(call),
                        },
                        None => ReadyThenCall::Call { fut: call(req) },
                    }
                }
                ReadyThenCallProj::Ready { fut, req, call } => {
                    if let Err(err) = std::task::ready!(fut.as_mut().poll(cx)) {
                        return Poll::Ready(Err(err));
                    }
                    ReadyThenCall::Call {
                        fut: call.take().unwrap()(req.take().unwrap()),
                    }
                }
                ReadyThenCallProj::Call { fut } => return fut.poll(cx),
            };
            self.set(next);
        }
    }
}

pin_project_lite::pin_project! {
    /// Keeps `owner` alive until `fut` is dropped.
    pub(crate) struct WithOwner<F, O> {
        // Declared first so it is dropped before `owner`.
        #[pin]
        pub(crate) fut: F,
        pub(crate) owner: O,
    }
}

impl<F: Future, O> Future for WithOwner<F, O> {
    type Output = F::Output;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().fut.poll(cx)
    }
}

pin_project_lite::pin_project! {
    /// Maps the output of `fut` with `f`.
    pub struct MapOutput<F, M> {
        #[pin]
        fut: F,
        f: Option<M>,
    }
}

impl<F, M, O> Future for MapOutput<F, M>
where
    F: Future,
    M: FnOnce(F::Output) -> O,
{
    type Output = O;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<O> {
        let this = self.project();
        let output = std::task::ready!(this.fut.poll(cx));
        let f = this.f.take().expect("`MapOutput` polled after completion");
        Poll::Ready(f(output))
    }
}

/// Maps the output of `fut` with `f`, without an `async` wrapper.
pub fn map_output<F, M, O>(fut: F, f: M) -> MapOutput<F, M>
where
    F: Future,
    M: FnOnce(F::Output) -> O,
{
    MapOutput { fut, f: Some(f) }
}

pub(crate) fn shutdown<S, St, A, B, RA, RB>(
    svc1: &A,
    svc2: &B,
    ctx: Ctx<'_, S, St>,
) -> impl Future<Output = ()>
where
    A: Service<St, RA>,
    B: Service<St, RB>,
{
    join(ctx.shutdown(svc1), ctx.shutdown(svc2))
}

pub(crate) fn ready<S, St, A, B, RA, RB>(
    svc1: &A,
    svc2: &B,
    ctx: Ctx<'_, S, St>,
) -> impl Future<Output = Result<(), A::Error>>
where
    A: Service<St, RA>,
    B: Service<St, RB, Error = A::Error>,
{
    map_output(
        ready_join(
            ctx,
            move |ctx| ctx.ready_generation(svc1),
            move |ctx| ctx.ready_generation(svc2),
        ),
        |res| res.map(|_| ()),
    )
}

/// Polls both futures concurrently until both complete.
pub async fn join<A, B>(fut1: A, fut2: B)
where
    A: Future<Output = ()>,
    B: Future<Output = ()>,
{
    let mut fut1 = pin::pin!(fut1);
    let mut fut2 = pin::pin!(fut2);

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

/// Waits until both readiness checks complete without an intervening invalidation.
///
/// A completed check is restarted when the scope is invalidated before the
/// other one completes. Returns the generation at which both are ready.
/// Both checks are created from a copy of `ctx`.
pub async fn ready_join<'a, S, St, E, F1, Fut1, F2, Fut2>(
    ctx: Ctx<'a, S, St>,
    f1: F1,
    f2: F2,
) -> Result<Generation, E>
where
    F1: Fn(Ctx<'a, S, St>) -> Fut1,
    Fut1: Future<Output = Result<Generation, E>>,
    F2: Fn(Ctx<'a, S, St>) -> Fut2,
    Fut2: Future<Output = Result<Generation, E>>,
{
    let mut fut1 = pin::pin!(Some(f1(ctx)));
    let mut fut2 = pin::pin!(Some(f2(ctx)));
    let scope = ctx.scope();
    let mut generation1 = scope.generation();
    let mut generation2 = scope.generation();

    poll_fn(|cx| {
        if is_idle(&mut fut1) && !scope.is_current(generation1) {
            fut1.set(Some(f1(ctx)));
        }
        if let Some(fut) = fut1.as_mut().as_pin_mut()
            && let Poll::Ready(generation) = fut.poll(cx)?
        {
            generation1 = generation;
            fut1.set(None);
        }

        if is_idle(&mut fut2) && !scope.is_current(generation2) {
            fut2.set(Some(f2(ctx)));
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
            Poll::Ready(Ok(generation1))
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

/// Checks one field's readiness and maps its error.
pub async fn ready_field<S, St, T, Req, E>(
    ctx: Ctx<'_, S, St>,
    svc: &T,
    map_err: impl FnOnce(T::Error) -> E,
) -> Result<Generation, E>
where
    T: Service<St, Req>,
{
    ctx.ready_generation(svc).await.map_err(map_err)
}

// `Option::is_none()` would take `&Option<F>`, which retags the whole future
// and invalidates borrows it holds into itself.
fn is_idle<F>(fut: &mut Pin<&mut Option<F>>) -> bool {
    fut.as_mut().as_pin_mut().is_none()
}

#[cfg(test)]
#[path = "../tests/unit/util.rs"]
pub(crate) mod tests;
