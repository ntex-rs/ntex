use std::{cell, fmt, future::Future, marker, pin::Pin, ptr, rc::Rc, task::Context, task::Poll};

use crate::{Ctx, IntoService, ReadyCtx, Service, ServiceFactory, ctx::WaitersRef};

/// Trait for types that can serve as pipeline state.
pub trait State<Req>: Default + 'static {
    #[inline]
    /// Updates the state for a request.
    fn check(&self, _: &Req) -> Option<Self> {
        None
    }
}

pub trait FromState<St>: Sized {
    fn from(st: &St) -> Option<Self>;
}

/// Container for a service.
///
/// Container allows to call enclosed service and adds support of shared readiness.
pub struct Pipeline<Req, Res, Err> {
    state: Rc<dyn PipelineApi<Req, Res, Err>>,
}

type BoxFuture<'a, I, E> = Pin<Box<dyn Future<Output = Result<I, E>> + 'a>>;

/// Factory for service pipeline.
pub struct PipelineFactory<St, Req, Res, Err, InitCfg, InitErr> {
    inner:
        Box<dyn for<'r> Fn(&'r InitCfg, &'r St) -> BoxFuture<'r, Pipeline<Req, Res, Err>, InitErr>>,
}

struct PipelineFactoryInner<St, Sf, Ust, Req> {
    sf: Sf,
    st: marker::PhantomData<(St, Ust, Req)>,
}

impl<St, Sf, Ust, Req> PipelineFactoryInner<St, Sf, Ust, Req>
where
    Sf: ServiceFactory<Req, Ust> + 'static,
    Ust: State<Req> + FromState<St> + 'static,
    Req: 'static,
{
    async fn create(
        &self,
        cfg: &Sf::InitCfg,
        st: &St,
    ) -> Result<Pipeline<Req, Sf::Res, Sf::Error>, Sf::InitError> {
        let svc = self.sf.create(cfg).await?;
        Ok(Pipeline::with_chained::<Sf::Service, Ust, St>(svc, st))
    }
}

impl<St, Req, Res, Err, InitCfg, InitErr> PipelineFactory<St, Req, Res, Err, InitCfg, InitErr> {
    pub fn new<Ust, Sf>(sf: Sf) -> Self
    where
        St: 'static,
        Ust: State<Req> + FromState<St> + 'static,
        Sf: ServiceFactory<Req, Ust, Res = Res, Error = Err, InitCfg = InitCfg, InitError = InitErr>
            + 'static,
        Req: 'static,
    {
        let inner = Rc::new(PipelineFactoryInner {
            sf,
            st: marker::PhantomData,
        });
        Self {
            inner: Box::new(move |cfg_, st_| {
                let pf = inner.clone();
                Box::pin(async move { pf.create(cfg_, st_).await })
            }),
        }
    }

    pub async fn create(&self, cfg: &InitCfg, st: &St) -> Result<Pipeline<Req, Res, Err>, InitErr> {
        (self.inner)(cfg, st).await
    }
}

/// Bound container for a service.
pub struct PipelineBinding<Req, Res, Err> {
    index: u32,
    state: Rc<dyn PipelineApi<Req, Res, Err>>,
}

trait PipelineApi<Req, Res, Err> {
    fn register(&self) -> u32;

    fn unregister(&self, idx: u32);

    fn ready(&self, idx: u32) -> Pin<Box<dyn Future<Output = Result<(), Err>> + '_>>;

    fn call(
        &self,
        idx: u32,
        req: Req,
        ready: bool,
    ) -> Pin<Box<dyn Future<Output = Result<Res, Err>> + '_>>;

    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + '_>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Err>>;

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()>;

    fn is_shutdown(&self) -> bool;
}

struct PipelineState<S: Service<St>, St> {
    s: S,
    waiters: WaitersRef,
    st: St,
    st_check: Box<dyn Fn(&St, &S::Req) -> Option<St>>,
    st_runtime: cell::UnsafeCell<RuntimeState<S::Error>>,
}

enum RuntimeState<E> {
    New,
    Readiness(Pin<Box<dyn Future<Output = Result<(), E>>>>),
    Shutdown(Pin<Box<dyn Future<Output = ()>>>),
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

impl<S: Service<St>, St> PipelineState<S, St> {
    pub(crate) fn waiters_ref(&self) -> &WaitersRef {
        &self.waiters
    }

    fn st(&self, req: &S::Req) -> StateRef<'_, St> {
        if let Some(s) = (self.st_check)(&self.st, req) {
            StateRef::Owned(s)
        } else {
            StateRef::Ref(&self.st)
        }
    }
}

impl<S: Service<St>, St> PipelineApi<S::Req, S::Res, S::Error> for PipelineState<S, St>
where
    S: 'static,
    St: 'static,
{
    fn register(&self) -> u32 {
        self.waiters.insert()
    }

    fn unregister(&self, index: u32) {
        self.waiters.remove(index);
    }

    fn ready(&self, idx: u32) -> Pin<Box<dyn Future<Output = Result<(), S::Error>> + '_>> {
        Box::pin(async move {
            Ctx::<'_, S, St>::new(idx, self.waiters_ref(), &self.st)
                .ready(&self.s)
                .await
        })
    }

    fn call(
        &self,
        idx: u32,
        req: S::Req,
        ready: bool,
    ) -> Pin<Box<dyn Future<Output = Result<S::Res, S::Error>> + '_>> {
        Box::pin(async move {
            let st = self.st(&req);

            if ready {
                Ctx::<'_, S, St>::new(idx, self.waiters_ref(), st.get_ref())
                    .call(&self.s, req)
                    .await
            } else {
                Ctx::<'_, S, St>::new(idx, self.waiters_ref(), st.get_ref())
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
            RuntimeState::Shutdown(_) => panic!("Pipeline is shutding down"),
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
                let fut = Box::pin(async move { pl.s.shutdown().await });
                *st = RuntimeState::Shutdown(fut);
                pl.waiters.shutdown();
                self.poll_shutdown(cx)
            }
            RuntimeState::Shutdown(fut) => Pin::new(fut).poll(cx),
        }
    }

    fn is_shutdown(&self) -> bool {
        self.waiters.is_shutdown()
    }

    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        if self.waiters.is_shutdown() {
            Box::pin(async {})
        } else {
            self.waiters.shutdown();
            Box::pin(self.s.shutdown())
        }
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
    pub fn new<S>(f: impl IntoService<S, ()>) -> Self
    where
        S: Service<(), Req = Req, Res = Res, Error = Err> + 'static,
    {
        Self::create(f.into_service(), ())
    }

    #[inline]
    /// Construct new service pipeline instance with state.
    pub fn with<S, St>(f: impl IntoService<S, St>) -> Self
    where
        S: Service<St, Req = Req, Res = Res, Error = Err> + 'static,
        St: State<Req> + 'static,
    {
        Self::create(f.into_service(), St::default())
    }

    #[inline]
    /// Construct new service pipeline instance with state.
    pub fn with_chained<S, St, Chained>(f: impl IntoService<S, St>, chained: &Chained) -> Self
    where
        S: Service<St, Req = Req, Res = Res, Error = Err> + 'static,
        St: State<Req> + FromState<Chained> + 'static,
    {
        Self::create(
            f.into_service(),
            St::from(chained).unwrap_or_else(|| St::default()),
        )
    }

    fn create<S, St>(s: S, st: St) -> Self
    where
        S: Service<St, Req = Req, Res = Res, Error = Err> + 'static,
        St: State<Req> + 'static,
    {
        Pipeline {
            state: Rc::new(PipelineState {
                s,
                st,
                waiters: WaitersRef::new(),
                st_check: Box::new(|st: &St, req: &S::Req| st.check(req)),
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
    pub fn call_static(&self, req: Req) -> PipelineCall<Res, Err> {
        let pl = self.bind();
        PipelineCall(Box::pin(
            async move { pl.state.call(pl.index, req, true).await },
        ))
    }

    #[inline]
    /// Call the service and create a future that resolves to the service result.
    ///
    /// This call can be completed from different async tasks.
    /// Note: this call does not check service readiness.
    pub fn call_nowait(&self, req: Req) -> PipelineCall<Res, Err> {
        let pl = self.bind();
        PipelineCall(Box::pin(async move {
            pl.state.call(pl.index, req, false).await
        }))
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
        self.state.shutdown().await;
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
        self.state.unregister(0);
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
            index: pl.state.register(),
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
    pub fn call_static(&self, req: Req) -> PipelineCall<Res, Err> {
        let pl = self.clone();
        PipelineCall(Box::pin(
            async move { pl.state.call(pl.index, req, true).await },
        ))
    }

    #[inline]
    /// Call the service and create a future that resolves to the service result.
    ///
    /// This call can be completed from different async tasks.
    /// Note: this call does not check service readiness.
    pub fn call_nowait(&self, req: Req) -> PipelineCall<Res, Err> {
        let pl = self.clone();
        PipelineCall(Box::pin(async move {
            pl.state.call(pl.index, req, false).await
        }))
    }

    #[inline]
    /// Shuts down the enclosed service.
    pub async fn shutdown(&self) {
        self.state.shutdown().await;
    }
}

impl<Req, Res, Err> Clone for PipelineBinding<Req, Res, Err> {
    fn clone(&self) -> Self {
        Self {
            index: self.state.register(),
            state: self.state.clone(),
        }
    }
}

impl<Req, Res, Err> Drop for PipelineBinding<Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.state.unregister(self.index);
    }
}

#[must_use = "futures do nothing unless polled"]
/// Pipeline call
pub struct PipelineCall<R, E>(Call<R, E>);

type Call<R, E> = Pin<Box<dyn Future<Output = Result<R, E>> + 'static>>;

impl<R, E> Future for PipelineCall<R, E> {
    type Output = Result<R, E>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.as_mut().0).poll(cx)
    }
}

fn ready<S, St>(pl: &'static PipelineState<S, St>) -> impl Future<Output = Result<(), S::Error>>
where
    S: Service<St>,
{
    pl.s.ready(ReadyCtx::<'_, S, St>::new(0, pl.waiters_ref(), &pl.st))
}

struct CheckReadiness<S, St, F, Fut>
where
    S: Service<St> + 'static,
    St: 'static,
{
    f: F,
    fut: Option<Fut>,
    pl: &'static PipelineState<S, St>,
}

impl<S: Service<St>, St, F, Fut> Unpin for CheckReadiness<S, St, F, Fut> {}

impl<S: Service<St>, St, F, Fut> Drop for CheckReadiness<S, St, F, Fut> {
    fn drop(&mut self) {
        // future got dropped during polling, we must notify other waiters
        if self.fut.is_some() {
            self.pl.waiters.notify();
        }
    }
}

impl<S, St, F, Fut> Future for CheckReadiness<S, St, F, Fut>
where
    S: Service<St>,
    F: Fn(&'static PipelineState<S, St>) -> Fut,
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

impl<Req> State<Req> for () {}

impl<St> FromState<St> for () {
    fn from(_: &St) -> Option<()> {
        None
    }
}

impl<Req, Res, Err> fmt::Debug for Pipeline<Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pipeline").finish()
    }
}

impl<St, Req, Res, Err, Cfg, InitErr> fmt::Debug
    for PipelineFactory<St, Req, Res, Err, Cfg, InitErr>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineFactory").finish()
    }
}

impl<Req, Res, Err> fmt::Debug for PipelineBinding<Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineBinding")
            .field("idx", &self.index)
            .finish()
    }
}

impl<R, E> fmt::Debug for PipelineCall<R, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineCall").finish()
    }
}
