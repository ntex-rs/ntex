use std::{cell, future::Future, pin::Pin, ptr, rc::Rc, task::Context, task::Poll};

use crate::{Ctx, Service, ctx::WaitersRef, util::BoxFuture};

// ======================== PipelaneState ============================

pub(crate) struct PipelineApi<Req, Res, Err>(Rc<dyn PipelineInternalApi<Req, Res, Err>>);

impl<Req, Res, Err> PipelineApi<Req, Res, Err> {
    pub(crate) fn new<S, St>(s: S, st: St) -> Self
    where
        S: Service<St, Req, Res = Res, Error = Err> + 'static,
        St: 'static,
        Req: 'static,
    {
        PipelineApi(Rc::new(PipelineInner {
            s,
            st,
            waiters: WaitersRef::new(),
            st_runtime: cell::UnsafeCell::new(RuntimeState::New),
        }))
    }

    pub(crate) fn with(api: impl PipelineInternalApi<Req, Res, Err> + 'static) -> Self {
        Self(Rc::new(api))
    }
}

impl<Req, Res, Err> PipelineApi<Req, Res, Err> {
    pub(crate) fn reg(&self) -> u32 {
        self.0.reg()
    }

    pub(crate) fn unreg(&self, idx: u32) {
        self.0.unreg(idx)
    }

    pub(crate) fn ready(&self, idx: u32) -> BoxFuture<'_, Result<(), Err>> {
        self.0.ready(idx)
    }

    pub(crate) fn call(&self, idx: u32, req: Req, ready: bool) -> BoxFuture<'_, Result<Res, Err>> {
        self.0.call(idx, req, ready)
    }

    pub(crate) fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Err>> {
        self.0.poll_ready(cx)
    }

    pub(crate) fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        self.0.poll_shutdown(cx)
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.0.is_shutdown()
    }
}

impl<Req, Res, Err> Clone for PipelineApi<Req, Res, Err> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

// ======================== PipelaneInner ============================

struct PipelineInner<S: Service<St, Req>, St, Req> {
    s: S,
    st: St,
    st_runtime: cell::UnsafeCell<RuntimeState<S::Error>>,
    waiters: WaitersRef,
}

enum RuntimeState<E> {
    New,
    Readiness(BoxFuture<'static, Result<(), E>>),
    Shutdown(BoxFuture<'static, ()>),
    Done,
}

// ======================== PipelaneApi ============================

pub(crate) trait PipelineInternalApi<Req, Res, Err> {
    fn reg(&self) -> u32;

    fn unreg(&self, idx: u32);

    fn ready(&self, idx: u32) -> BoxFuture<'_, Result<(), Err>>;

    fn call(&self, idx: u32, req: Req, ready: bool) -> BoxFuture<'_, Result<Res, Err>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Err>>;

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()>;

    fn is_shutdown(&self) -> bool;
}

impl<S, St, Req> PipelineInternalApi<Req, S::Res, S::Error> for PipelineInner<S, St, Req>
where
    S: Service<St, Req> + 'static,
    St: 'static,
    Req: 'static,
{
    fn reg(&self) -> u32 {
        self.waiters.insert()
    }

    fn unreg(&self, index: u32) {
        self.waiters.remove(index);
    }

    fn ready(&self, idx: u32) -> BoxFuture<'_, Result<(), S::Error>> {
        Box::pin(async move {
            Ctx::<'_, S, St>::new(idx, &self.waiters, &self.st)
                .ready(&self.s)
                .await
        })
    }

    fn call(&self, idx: u32, req: Req, ready: bool) -> BoxFuture<'_, Result<S::Res, S::Error>> {
        Box::pin(async move {
            if ready {
                Ctx::<'_, S, St>::new(idx, &self.waiters, &self.st)
                    .call(&self.s, req)
                    .await
            } else {
                Ctx::<'_, S, St>::new(idx, &self.waiters, &self.st)
                    .call_nowait(&self.s, req)
                    .await
            }
        })
    }

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        let st = unsafe { &mut *self.st_runtime.get() };
        match st {
            RuntimeState::New => {
                // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
                // Pipeline::svc is heap allocated(Rc<S>), and it is being kept alive until
                // `self` is alive
                let pl = unsafe { &*(ptr::from_ref(self)) };
                let fut = Box::pin(CheckReadiness {
                    pl,
                    f: ready,
                    fut: None,
                });
                *st = RuntimeState::Readiness(fut);
                self.poll_ready(cx)
            }
            RuntimeState::Readiness(fut) => Pin::new(fut).poll(cx),
            RuntimeState::Shutdown(_) | RuntimeState::Done => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        let st = unsafe { &mut *self.st_runtime.get() };
        match st {
            RuntimeState::New | RuntimeState::Readiness(_) => {
                // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
                // Pipeline::svc is heap allocated(Rc<S>), and it is being kept alive until
                // `self` is alive
                let pl = unsafe { &*(ptr::from_ref(self)) };

                let fut = Box::pin(async move {
                    let ctx = Ctx::<'_, S, St>::new(0, &pl.waiters, &pl.st);
                    pl.s.shutdown(ctx).await;
                });
                *st = RuntimeState::Shutdown(fut);
                pl.waiters.shutdown();
                self.poll_shutdown(cx)
            }
            RuntimeState::Shutdown(fut) => {
                let res = Pin::new(fut).poll(cx);
                if res.is_ready() {
                    *st = RuntimeState::Done;
                }
                res
            }
            RuntimeState::Done => Poll::Ready(()),
        }
    }

    fn is_shutdown(&self) -> bool {
        self.waiters.is_shutdown()
    }
}

fn ready<S, St, Req>(
    pl: &'static PipelineInner<S, St, Req>,
) -> impl Future<Output = Result<(), S::Error>>
where
    S: Service<St, Req>,
{
    pl.s.ready(Ctx::<'_, S, St>::new(0, &pl.waiters, &pl.st))
}

struct CheckReadiness<S, St, Req, F, Fut>
where
    S: Service<St, Req> + 'static,
    St: 'static,
    Req: 'static,
{
    f: F,
    fut: Option<Fut>,
    pl: &'static PipelineInner<S, St, Req>,
}

impl<S, St, Req, F, Fut> Unpin for CheckReadiness<S, St, Req, F, Fut> where S: Service<St, Req> {}

impl<S, St, Req, F, Fut> Drop for CheckReadiness<S, St, Req, F, Fut>
where
    S: Service<St, Req>,
{
    fn drop(&mut self) {
        // future got dropped during polling, we must notify other waiters
        if self.fut.is_some() {
            self.pl.waiters.notify();
        }
    }
}

impl<S, St, Req, F, Fut> Future for CheckReadiness<S, St, Req, F, Fut>
where
    S: Service<St, Req>,
    F: Fn(&'static PipelineInner<S, St, Req>) -> Fut,
    Fut: Future<Output = Result<(), S::Error>>,
{
    type Output = Result<(), S::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut();

        this.pl.waiters.run(0, cx, |cx| {
            if this.fut.is_none() {
                this.fut = Some((this.f)(this.pl));
            }
            let fut = this.fut.as_mut().unwrap();
            let result = unsafe { Pin::new_unchecked(fut) }.poll(cx);
            if result.is_ready() {
                let _ = this.fut.take();
            }
            result
        })
    }
}
