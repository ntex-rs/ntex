use std::{cell, fmt, future, pin::Pin, ptr, rc::Rc, task::Context, task::Poll};

use crate::{Ctx, IntoService, Service, ctx::WaitersRef, util::BoxFuture};

use crate::pipeline::PipelineBinding;
use crate::pl_inner::{PipelineApi, PipelineInternalApi};

/// Container for a service.
///
/// Provides a way to call the enclosed service and share its readiness state.
pub struct PipelineState<St, Req, Res, Err> {
    api: Rc<dyn PipelineStateApi<St, Req, Res, Err>>,
}

impl<St, Req, Res, Err> PipelineState<St, Req, Res, Err>
where
    St: 'static,
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    #[inline]
    /// Construct new service pipeline instance with default state.
    pub fn new<S>(f: impl IntoService<S, St, Req>) -> Self
    where
        S: Service<St, Req, Res = Res, Error = Err> + 'static,
        St: 'static,
    {
        PipelineState {
            api: Rc::new(PipelineInner {
                s: f.into_service(),
                waiters: WaitersRef::new(),
                st_runtime: cell::UnsafeCell::new(RuntimeState::New),
            }),
        }
    }

    #[inline]
    /// Returns when the pipeline is ready to process requests.
    pub async fn ready(&self, st: &St) -> Result<(), Err> {
        self.api.ready(0, st).await
    }

    #[inline]
    /// Wait for service readiness, then create a future
    /// that resolves to the service call result.
    pub async fn call(&self, req: Req, st: &St) -> Result<Res, Err> {
        let pl = self.binding();
        self.api.call(pl.idx, req, st, true).await
    }

    #[inline]
    /// Call the service and create a future that resolves to the service result.
    ///
    /// This call can be completed from different async tasks.
    /// Note: this call does not check service readiness.
    pub async fn call_nowait(&self, req: Req, st: &St) -> Result<Res, Err> {
        let pl = self.binding();
        pl.api.call(pl.idx, req, st, false).await
    }

    #[inline]
    /// Shuts down the enclosed service.
    pub async fn shutdown(&self, st: &St) {
        self.api.shutdown(0, st).await;
    }

    #[inline]
    /// Returns `Ready` when the pipeline is ready to process requests.
    ///
    /// # Panics
    ///
    /// Panics if the pipeline is shutting down (i.e., `.shutdown()` or
    /// `.poll_shutdown()` has been called).
    pub fn poll_ready(&self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), Err>>
    where
        St: Clone,
    {
        self.api.poll_ready(cx, st)
    }

    fn binding(&self) -> Binding<'_, St, Req, Res, Err> {
        Binding {
            idx: self.api.reg(),
            api: self.api.as_ref(),
        }
    }

    #[inline]
    /// Returns the current pipeline binding.
    ///
    /// The binding can be used to call the service.
    pub fn bind(&self) -> PipelineStateBinding<St, Req, Res, Err> {
        PipelineStateBinding {
            idx: self.api.reg(),
            api: self.api.clone(),
        }
    }

    #[inline]
    /// Returns the current pipeline binding.
    ///
    /// The binding can be used to call the service.
    pub fn bind_state(&self, st: St) -> PipelineBinding<Req, Res, Err>
    where
        St: Clone,
    {
        let internal = PipelineInternal {
            st,
            api: self.api.clone(),
        };

        PipelineBinding::with(self.api.reg(), PipelineApi::with(internal))
    }
}

impl<St, Req, Res, Err> Drop for PipelineState<St, Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.api.unreg(0);
    }
}

struct Binding<'a, St, Req, Res, Err> {
    idx: u32,
    api: &'a dyn PipelineStateApi<St, Req, Res, Err>,
}

impl<St, Req, Res, Err> Drop for Binding<'_, St, Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.api.unreg(self.idx);
    }
}

// ========================== `PipelineStateBinding` ===========================

pub struct PipelineStateBinding<St, Req, Res, Err> {
    idx: u32,
    api: Rc<dyn PipelineStateApi<St, Req, Res, Err>>,
}

impl<St, Req, Res, Err> Drop for PipelineStateBinding<St, Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.api.unreg(self.idx);
    }
}

impl<St, Req, Res, Err> Clone for PipelineStateBinding<St, Req, Res, Err> {
    #[inline]
    fn clone(&self) -> Self {
        PipelineStateBinding {
            idx: self.api.reg(),
            api: self.api.clone(),
        }
    }
}

impl<St, Req, Res, Err> PipelineStateBinding<St, Req, Res, Err>
where
    St: 'static,
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    #[inline]
    /// Wait for service readiness, then create a future
    /// that resolves to the service call result.
    pub async fn call(&self, req: Req, st: &St) -> Result<Res, Err> {
        let pl = Binding {
            idx: self.api.reg(),
            api: self.api.as_ref(),
        };
        pl.api.call(pl.idx, req, st, true).await
    }

    #[inline]
    /// Call the service and create a future that resolves to the service result.
    ///
    /// This call can be completed from different async tasks.
    /// Note: this call does not check service readiness.
    pub async fn call_nowait(&self, req: Req, st: &St) -> Result<Res, Err> {
        let pl = Binding {
            idx: self.api.reg(),
            api: self.api.as_ref(),
        };
        pl.api.call(pl.idx, req, st, false).await
    }
}

// ========================== `PipelineApi` ===========================

struct PipelineInternal<St, Req, Res, Err> {
    st: St,
    api: Rc<dyn PipelineStateApi<St, Req, Res, Err>>,
}

impl<St, Req, Res, Err> PipelineInternalApi<Req, Res, Err> for PipelineInternal<St, Req, Res, Err> {
    fn reg(&self) -> u32 {
        self.api.reg()
    }

    fn unreg(&self, idx: u32) {
        self.api.unreg(idx);
    }

    fn ready(&self, idx: u32) -> BoxFuture<'_, Result<(), Err>> {
        self.api.ready(idx, &self.st)
    }

    fn call(&self, idx: u32, req: Req, ready: bool) -> BoxFuture<'_, Result<Res, Err>> {
        self.api.call(idx, req, &self.st, ready)
    }

    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Err>> {
        unreachable!()
    }

    fn poll_shutdown(&self, _: &mut Context<'_>) -> Poll<()> {
        unreachable!()
    }

    fn is_shutdown(&self) -> bool {
        self.api.is_shutdown()
    }
}

// ========================== `PipelineStateApi` ===========================

struct PipelineInner<S, St, E> {
    s: S,
    waiters: WaitersRef,
    st_runtime: cell::UnsafeCell<RuntimeState<St, E>>,
}

enum RuntimeState<St, E> {
    New,
    Readiness(Box<dyn CheckReadiness<St, E>>),
    Shutdown,
}

trait PipelineStateApi<St, Req, Res, Err> {
    fn reg(&self) -> u32;
    fn unreg(&self, idx: u32);

    fn call<'a>(
        &'a self,
        idx: u32,
        req: Req,
        st: &'a St,
        ready: bool,
    ) -> BoxFuture<'a, Result<Res, Err>>
    where
        Req: 'a;

    fn ready<'a>(&'a self, idx: u32, st: &'a St) -> BoxFuture<'a, Result<(), Err>>
    where
        Req: 'a;

    fn poll_ready(&self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), Err>>
    where
        St: Clone;

    fn shutdown<'a>(&'a self, idx: u32, st: &'a St) -> BoxFuture<'a, ()>;

    fn is_shutdown(&self) -> bool;
}

impl<S, St, Req, E> PipelineStateApi<St, Req, S::Res, S::Error> for PipelineInner<S, St, E>
where
    S: Service<St, Req, Error = E> + 'static,
    St: 'static,
    Req: 'static,
    E: 'static,
{
    fn reg(&self) -> u32 {
        self.waiters.insert()
    }

    fn unreg(&self, idx: u32) {
        self.waiters.remove(idx);
    }

    fn ready<'a>(&'a self, idx: u32, st: &'a St) -> BoxFuture<'a, Result<(), S::Error>>
    where
        Req: 'a,
    {
        Box::pin(async move {
            Ctx::<'_, S, St>::new(idx, &self.waiters, st)
                .ready(&self.s)
                .await
        })
    }

    fn shutdown<'a>(&'a self, idx: u32, st: &'a St) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let pl_state = unsafe { &mut *self.st_runtime.get() };
            *pl_state = RuntimeState::Shutdown;

            Ctx::<'_, S, St>::new(idx, &self.waiters, st)
                .shutdown(&self.s)
                .await;
        })
    }

    fn call<'a>(
        &'a self,
        idx: u32,
        req: Req,
        st: &'a St,
        ready: bool,
    ) -> BoxFuture<'a, Result<S::Res, S::Error>>
    where
        Req: 'a,
    {
        Box::pin(async move {
            if ready {
                Ctx::<'_, S, St>::new(idx, &self.waiters, st)
                    .call(&self.s, req)
                    .await
            } else {
                Ctx::<'_, S, St>::new(idx, &self.waiters, st)
                    .call_nowait(&self.s, req)
                    .await
            }
        })
    }

    fn poll_ready(&self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), S::Error>>
    where
        St: Clone,
    {
        let pl_state = unsafe { &mut *self.st_runtime.get() };
        match pl_state {
            RuntimeState::New => {
                // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
                // Pipeline::svc is heap allocated(Rc<S>), and it is being kept alive until
                // `self` is alive
                let pl = unsafe { &*(ptr::from_ref(self)) };
                let fut = Box::new(CheckReadinessFut {
                    pl,
                    f: ready,
                    st: st.clone(),
                    fut: None,
                });
                *pl_state = RuntimeState::Readiness(fut);
                self.poll_ready(cx, st)
            }
            RuntimeState::Readiness(fut) => fut.poll(cx, st),
            RuntimeState::Shutdown => panic!("Pipeline is shutding down"),
        }
    }

    fn is_shutdown(&self) -> bool {
        self.waiters.is_shutdown()
    }
}

trait CheckReadiness<St, E> {
    fn poll(&mut self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), E>>;
}

struct CheckReadinessFut<S, St, Req, F, Fut>
where
    S: Service<St, Req> + 'static,
    St: 'static,
    Req: 'static,
{
    f: F,
    st: St,
    fut: Option<Fut>,
    pl: &'static PipelineInner<S, St, S::Error>,
}

fn ready<S, St, Req>(
    st: &'static St,
    pl: &'static PipelineInner<S, St, S::Error>,
) -> impl future::Future<Output = Result<(), S::Error>>
where
    S: Service<St, Req>,
{
    pl.s.ready(Ctx::<'_, S, St>::new(0, &pl.waiters, st))
}

impl<S: Service<St, Req>, St, Req, F, Fut> Drop for CheckReadinessFut<S, St, Req, F, Fut> {
    fn drop(&mut self) {
        // future got dropped during polling, we must notify other waiters
        if self.fut.is_some() {
            self.pl.waiters.notify();
        }
    }
}

impl<S, St, Req, F, Fut> CheckReadiness<St, S::Error> for CheckReadinessFut<S, St, Req, F, Fut>
where
    St: Clone,
    S: Service<St, Req>,
    F: Fn(&'static St, &'static PipelineInner<S, St, S::Error>) -> Fut,
    Fut: Future<Output = Result<(), S::Error>>,
{
    fn poll(&mut self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), S::Error>> {
        self.pl.waiters.run(0, cx, |cx| {
            if self.fut.is_none() {
                self.st = st.clone();
                let st: &'static St = unsafe { std::mem::transmute(&self.st) };
                self.fut = Some((self.f)(st, self.pl));
            }
            let fut = self.fut.as_mut().unwrap();
            let result = unsafe { Pin::new_unchecked(fut) }.poll(cx);
            if result.is_ready() {
                let _ = self.fut.take();
            }
            result
        })
    }
}

impl<St, Req, Res, Err> fmt::Debug for PipelineState<St, Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineState").finish()
    }
}

impl<St, Req, Res, Err> fmt::Debug for PipelineStateBinding<St, Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineStateBinding").finish()
    }
}
