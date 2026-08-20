use std::{cell, fmt, future, pin::Pin, ptr, rc::Rc, task::Context, task::Poll};

use crate::state::{Noop, State};
use crate::{Ctx, CtxShutdown, IntoService, Service, ctx::WaitersRef, util::BoxFuture};

pub use crate::pl_factory::PipelineFactory;
pub use crate::pl_nost::PipelineNostate;

/// Container for a service.
///
/// Provides a way to call the enclosed service and share its readiness state.
pub struct Pipeline<Req, Res, Err> {
    state: Rc<dyn PipelineApi<Req, Res, Err>>,
}

/// Bound container for a service.
pub struct PipelineBinding<Req, Res, Err> {
    index: u32,
    state: Rc<dyn PipelineApi<Req, Res, Err>>,
}

struct PipelineState<S: Service<St, Req>, St, Req, Ctl> {
    s: S,
    st: St,
    st_ctl: Ctl,
    st_runtime: cell::UnsafeCell<RuntimeState<S::Error>>,
    waiters: WaitersRef,
}

trait PipelineApi<Req, Res, Err> {
    fn reg(&self) -> u32;

    fn unreg(&self, idx: u32);

    fn ready(&self, idx: u32) -> BoxFuture<'_, Result<(), Err>>;

    fn call(&self, idx: u32, req: Req, ready: bool) -> BoxFuture<'_, Result<Res, Err>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Err>>;

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()>;

    fn is_shutdown(&self) -> bool;
}

enum RuntimeState<E> {
    New,
    Readiness(BoxFuture<'static, Result<(), E>>),
    Shutdown(BoxFuture<'static, ()>),
    Done,
}

enum StateRef<'a, T> {
    Ref(&'a T),
    Owned(T),
}

impl<'a, T> StateRef<'a, T> {
    fn get_ref(&'a self) -> &'a T {
        match self {
            StateRef::Ref(t) => t,
            StateRef::Owned(t) => t,
        }
    }
}

impl<S, St, Req, Ctl> PipelineState<S, St, Req, Ctl>
where
    S: Service<St, Req>,
    Ctl: State<St, Req>,
{
    fn st(&self, req: &Req) -> StateRef<'_, St> {
        if let Some(s) = self.st_ctl.on_req(&self.st, req) {
            StateRef::Owned(s)
        } else {
            StateRef::Ref(&self.st)
        }
    }
}

impl<S, St, Req, Ctl> PipelineApi<Req, S::Res, S::Error> for PipelineState<S, St, Req, Ctl>
where
    S: Service<St, Req> + 'static,
    St: 'static,
    Req: 'static,
    Ctl: State<St, Req> + 'static,
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
            let st = self.st(&req);

            if ready {
                Ctx::<'_, S, St>::new(idx, &self.waiters, st.get_ref())
                    .call(&self.s, req)
                    .await
            } else {
                Ctx::<'_, S, St>::new(idx, &self.waiters, st.get_ref())
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
            RuntimeState::Shutdown(_) | RuntimeState::Done => panic!("Pipeline is shutding down"),
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
                let fut = Box::pin(async move { pl.s.shutdown(CtxShutdown::new(&pl.st)).await });
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

impl<Req, Res, Err> Pipeline<Req, Res, Err>
where
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    #[inline]
    /// Construct new service pipeline instance.
    pub fn new<S>(f: impl IntoService<S, (), Req>) -> Self
    where
        S: Service<(), Req, Res = Res, Error = Err> + 'static,
    {
        Self::create(f.into_service(), (), Noop)
    }

    #[inline]
    /// Construct new service pipeline instance with default state.
    pub fn with<S, St>(f: impl IntoService<S, St, Req>) -> Self
    where
        S: Service<St, Req, Res = Res, Error = Err> + 'static,
        St: Default + 'static,
    {
        Self::create(f.into_service(), St::default(), Noop)
    }

    #[inline]
    /// Construct new service pipeline instance with state.
    pub fn with_st<S, St>(st: St, f: impl IntoService<S, St, Req>) -> Self
    where
        S: Service<St, Req, Res = Res, Error = Err> + 'static,
        St: 'static,
    {
        Self::create(f.into_service(), st, Noop)
    }

    #[inline]
    /// Construct new service pipeline instance with state.
    pub fn with_stctl<S, St, Ctl>(st: St, ctl: Ctl, f: impl IntoService<S, St, Req>) -> Self
    where
        S: Service<St, Req, Res = Res, Error = Err> + 'static,
        St: 'static,
        Ctl: State<St, Req> + 'static,
    {
        Self::create(f.into_service(), st, ctl)
    }

    fn create<S, St, Ctl>(s: S, st: St, ctl: Ctl) -> Self
    where
        S: Service<St, Req, Res = Res, Error = Err> + 'static,
        St: 'static,
        Ctl: State<St, Req> + 'static,
    {
        Pipeline {
            state: Rc::new(PipelineState {
                s,
                st,
                waiters: WaitersRef::new(),
                st_ctl: ctl,
                st_runtime: cell::UnsafeCell::new(RuntimeState::New),
            }),
        }
    }

    #[inline]
    /// Returns when the pipeline is ready to process requests.
    pub async fn ready(&self) -> Result<(), Err> {
        self.state.ready(0).await
    }

    #[inline]
    /// Wait for service readiness, then create a future
    /// that resolves to the service call result.
    pub async fn call(&self, req: Req) -> Result<Res, Err> {
        self.state.call(0, req, true).await
    }

    #[inline]
    /// Wait for service readiness, then create a future
    /// that resolves to the service result.
    ///
    /// This call can be completed from different async tasks.
    pub fn call_static(&self, req: Req) -> PipelineCall<Req, Res, Err> {
        PipelineCall::new(self.bind(), req, true)
    }

    #[inline]
    /// Call the service and create a future that resolves to the service result.
    ///
    /// This call can be completed from different async tasks.
    /// Note: this call does not check service readiness.
    pub fn call_nowait(&self, req: Req) -> PipelineCall<Req, Res, Err> {
        PipelineCall::new(self.bind(), req, false)
    }

    #[inline]
    /// Returns `Ready` when the pipeline is ready to process requests.
    ///
    /// # Panics
    ///
    /// Panics if the pipeline is shutting down (i.e., `.shutdown()` or
    /// `.poll_shutdown()` has been called).
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Err>> {
        self.state.poll_ready(cx)
    }

    #[inline]
    /// Returns `Ready` when the service has been properly shut down.
    pub fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        self.state.poll_shutdown(cx)
    }

    #[inline]
    /// Checks whether pipeline shutdown has been initiated.
    pub fn is_shutdown(&self) -> bool {
        self.state.is_shutdown()
    }

    #[inline]
    /// Shuts down the enclosed service.
    pub async fn shutdown(&self) {
        future::poll_fn(|cx| self.state.poll_shutdown(cx)).await;
    }

    #[inline]
    /// Returns the current pipeline binding.
    ///
    /// The binding can be used to check readiness and call the service.
    pub fn bind(&self) -> PipelineBinding<Req, Res, Err> {
        PipelineBinding::new(self)
    }
}

impl<Req, Res, Err> Drop for Pipeline<Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.state.unreg(0);
    }
}

impl<Req, Res, Err> PipelineBinding<Req, Res, Err>
where
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    fn new(pl: &Pipeline<Req, Res, Err>) -> Self {
        Self {
            index: pl.state.reg(),
            state: pl.state.clone(),
        }
    }

    #[inline]
    /// Returns when the pipeline is ready to process requests.
    pub async fn ready(&self) -> Result<(), Err> {
        self.state.ready(self.index).await
    }

    #[inline]
    /// Wait for service readiness, then create a future
    /// that resolves to the service call result.
    pub async fn call(&self, req: Req) -> Result<Res, Err> {
        self.state.call(self.index, req, true).await
    }

    #[inline]
    /// Wait for service readiness, then create a future
    /// that resolves to the service result.
    ///
    /// This call can be completed from different async tasks.
    pub fn call_static(&self, req: Req) -> PipelineCall<Req, Res, Err> {
        PipelineCall::new(self.clone(), req, true)
    }

    #[inline]
    /// Call the service and create a future that resolves to the service result.
    ///
    /// This call can be completed from different async tasks.
    /// Note: this call does not check service readiness.
    pub fn call_nowait(&self, req: Req) -> PipelineCall<Req, Res, Err> {
        PipelineCall::new(self.clone(), req, false)
    }

    #[inline]
    /// Shuts down the enclosed service.
    pub async fn shutdown(&self) {
        future::poll_fn(|cx| self.state.poll_shutdown(cx)).await;
    }
}

impl<Req, Res, Err> Drop for PipelineBinding<Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.state.unreg(self.index);
    }
}

impl<Req, Res, Err> Clone for PipelineBinding<Req, Res, Err> {
    fn clone(&self) -> Self {
        Self {
            index: self.state.reg(),
            state: self.state.clone(),
        }
    }
}

#[must_use = "futures do nothing unless polled"]
/// Pipeline call
pub struct PipelineCall<Req, Res, Err> {
    #[allow(dead_code)]
    pl: PipelineBinding<Req, Res, Err>,
    fut: BoxFuture<'static, Result<Res, Err>>,
}

impl<Req, Res, Err> PipelineCall<Req, Res, Err> {
    #[allow(clippy::missing_transmute_annotations)]
    fn new(pl: PipelineBinding<Req, Res, Err>, req: Req, ready: bool) -> Self {
        // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
        // and it is being kept alive until `self` is alive
        PipelineCall {
            fut: unsafe { std::mem::transmute(pl.state.call(pl.index, req, ready)) },
            pl,
        }
    }
}

impl<Req, Res, Err> future::Future for PipelineCall<Req, Res, Err> {
    type Output = Result<Res, Err>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.as_mut().fut).poll(cx)
    }
}

fn ready<S, St, Req, Ctl>(
    pl: &'static PipelineState<S, St, Req, Ctl>,
) -> impl future::Future<Output = Result<(), S::Error>>
where
    S: Service<St, Req>,
    Ctl: State<St, Req>,
{
    pl.s.ready(Ctx::<'_, S, St>::new(0, &pl.waiters, &pl.st))
}

struct CheckReadiness<S, St, Req, Ctl, F, Fut>
where
    S: Service<St, Req> + 'static,
    St: 'static,
    Req: 'static,
    Ctl: 'static,
{
    f: F,
    fut: Option<Fut>,
    pl: &'static PipelineState<S, St, Req, Ctl>,
}

impl<S: Service<St, Req>, St, Req, Ctl, F, Fut> Unpin for CheckReadiness<S, St, Req, Ctl, F, Fut> {}

impl<S: Service<St, Req>, St, Req, Ctl, F, Fut> Drop for CheckReadiness<S, St, Req, Ctl, F, Fut> {
    fn drop(&mut self) {
        // future got dropped during polling, we must notify other waiters
        if self.fut.is_some() {
            self.pl.waiters.notify();
        }
    }
}

impl<S, St, Req, Ctl, F, Fut> Future for CheckReadiness<S, St, Req, Ctl, F, Fut>
where
    S: Service<St, Req>,
    F: Fn(&'static PipelineState<S, St, Req, Ctl>) -> Fut,
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

impl<Req, Res, Err> fmt::Debug for Pipeline<Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pipeline").finish()
    }
}

impl<Req, Res, Err> fmt::Debug for PipelineBinding<Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineBinding")
            .field("idx", &self.index)
            .finish()
    }
}

impl<Req, Res, Err> fmt::Debug for PipelineCall<Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineCall").finish()
    }
}
