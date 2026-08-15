use std::{cell, fmt, future::Future, pin::Pin, ptr, rc::Rc, task::Context, task::Poll};

use crate::{Ctx, IntoService, ReadyCtx, Service, ctx::WaitersRef};

/// Container for a service.
///
/// Container allows to call enclosed service and adds support of shared readiness.
pub struct Pipeline<S: Service> {
    state: Rc<PipelineState<S>>,
}

struct PipelineState<S: Service> {
    s: S,
    waiters: WaitersRef,
    st: S::St,
    st_check: Option<Box<dyn Fn(&S::St, &S::Req) -> Option<S::St>>>,
    st_runtime: cell::UnsafeCell<RuntimeState<S::Error>>,
}

/// Bound container for a service.
pub struct PipelineBinding<S: Service> {
    index: u32,
    state: Rc<PipelineState<S>>,
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

impl<S: Service> PipelineState<S> {
    pub(crate) fn waiters_ref(&self) -> &WaitersRef {
        &self.waiters
    }

    fn st(&self, req: &S::Req) -> StateRef<'_, S::St> {
        if let Some(f) = &self.st_check {
            if let Some(s) = f(&self.st, req) {
                StateRef::Owned(s)
            } else {
                StateRef::Ref(&self.st)
            }
        } else {
            StateRef::Ref(&self.st)
        }
    }
}

impl<S: Service> Pipeline<S> {
    #[inline]
    /// Construct new service pipeline instance.
    pub fn new<St>(f: impl IntoService<S>) -> Self
    where
        St: Default,
        S: Service<St = St>,
    {
        let (_, waiters) = WaitersRef::new();
        Pipeline {
            state: Rc::new(PipelineState {
                waiters,
                s: f.into_service(),
                st: S::St::default(),
                st_check: None,
                st_runtime: cell::UnsafeCell::new(RuntimeState::New),
            }),
        }
    }

    #[inline]
    /// Return reference to service's shared state
    pub fn st(&self) -> &S::St {
        &self.state.st
    }

    #[inline]
    /// Return reference to enclosed service
    pub fn get_ref(&self) -> &S {
        &self.state.s
    }

    #[inline]
    /// Returns when the pipeline is ready to process requests.
    pub async fn ready(&self) -> Result<(), S::Error> {
        Ctx::<'_, S>::new(0, self.state.waiters_ref(), &self.state.st)
            .ready(&self.state.s)
            .await
    }

    #[inline]
    /// Wait for service readiness, then create a future
    /// that resolves to the service call result.
    pub async fn call(&self, req: S::Req) -> Result<S::Res, S::Error> {
        let st = self.state.st(&req);

        Ctx::<'_, S>::new(0, self.state.waiters_ref(), st.get_ref())
            .call(&self.state.s, req)
            .await
    }

    #[inline]
    /// Wait for service readiness, then create a future
    /// that resolves to the service result.
    ///
    /// This call can be completed from different async tasks.
    pub fn call_static(&self, req: S::Req) -> PipelineCall<S>
    where
        S: 'static,
    {
        let pl = self.bind();

        PipelineCall {
            fut: Box::pin(async move {
                Ctx::<S>::new(pl.index, pl.state.waiters_ref(), &pl.state.st)
                    .call(&pl.state.s, req)
                    .await
            }),
        }
    }

    #[inline]
    /// Call the service and create a future that resolves to the service result.
    ///
    /// This call can be completed from different async tasks.
    /// Note: this call does not check service readiness.
    pub fn call_nowait(&self, req: S::Req) -> PipelineCall<S>
    where
        S: 'static,
    {
        let pl = self.bind();

        PipelineCall {
            fut: Box::pin(async move {
                let st = pl.state.st(&req);
                Ctx::<S>::new(pl.index, pl.state.waiters_ref(), st.get_ref())
                    .call_nowait(&pl.state.s, req)
                    .await
            }),
        }
    }

    #[inline]
    /// Returns `Ready` when the pipeline is ready to process requests.
    ///
    /// # Panics
    ///
    /// Panics if the pipeline is shutting down (i.e., `.shutdown()` or
    /// `.poll_shutdown()` has been called).
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>>
    where
        S: 'static,
    {
        let st = unsafe { &mut *self.state.st_runtime.get() };
        match st {
            RuntimeState::New => {
                // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
                // Pipeline::svc is heap allocated(Rc<S>), and it is being kept alive until
                // `self` is alive
                let pl = unsafe { &*(ptr::from_ref(&self.state)) };
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

    #[inline]
    /// Checks whether pipeline shutdown has been initiated.
    pub fn is_shutdown(&self) -> bool {
        self.state.waiters.is_shutdown()
    }

    #[inline]
    /// Shuts down the enclosed service.
    pub async fn shutdown(&self) {
        self.state.s.shutdown().await;
    }

    #[inline]
    /// Returns `Ready` when the service has been properly shut down.
    pub fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()>
    where
        S: 'static,
    {
        let st = unsafe { &mut *self.state.st_runtime.get() };
        match st {
            RuntimeState::New | RuntimeState::Readiness(_) => {
                // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
                // Pipeline::svc is heap allocated(Rc<S>), and it is being kept alive until
                // `self` is alive
                let pl = unsafe { &*(ptr::from_ref(&self.state)) };
                let fut = Box::pin(async move { pl.s.shutdown().await });
                *st = RuntimeState::Shutdown(fut);
                pl.waiters.shutdown();
                self.poll_shutdown(cx)
            }
            RuntimeState::Shutdown(fut) => Pin::new(fut).poll(cx),
        }
    }

    #[inline]
    /// Returns the current pipeline binding.
    ///
    /// The binding can be used to check readiness and call the service.
    pub fn bind(&self) -> PipelineBinding<S>
    where
        S: 'static,
    {
        PipelineBinding::new(self)
    }
}

impl<S: Service> Drop for Pipeline<S> {
    #[inline]
    fn drop(&mut self) {
        self.state.waiters.remove(0);
    }
}

impl<S: Service + fmt::Debug> fmt::Debug for Pipeline<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pipeline")
            .field("state", &self.state)
            .finish()
    }
}

impl<S: Service + fmt::Debug> fmt::Debug for PipelineState<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineState")
            .field("svc", &self.s)
            .field("waiters", &self.waiters.get().len())
            .finish()
    }
}

impl<S: Service> PipelineBinding<S> {
    fn new(pl: &Pipeline<S>) -> Self {
        Self {
            index: pl.state.waiters.insert(),
            state: pl.state.clone(),
        }
    }

    #[inline]
    /// Return reference to enclosed service
    pub fn get_ref(&self) -> &S {
        &self.state.s
    }

    #[inline]
    /// Returns when the pipeline is ready to process requests.
    pub async fn ready(&self) -> Result<(), S::Error> {
        Ctx::<'_, S>::new(self.index, self.state.waiters_ref(), &self.state.st)
            .ready(&self.state.s)
            .await
    }

    #[inline]
    /// Wait for service readiness, then create a future
    /// that resolves to the service call result.
    pub async fn call(&self, req: S::Req) -> Result<S::Res, S::Error> {
        let st = self.state.st(&req);
        Ctx::<'_, S>::new(self.index, self.state.waiters_ref(), st.get_ref())
            .call(&self.state.s, req)
            .await
    }

    #[inline]
    /// Wait for service readiness, then create a future
    /// that resolves to the service result.
    ///
    /// This call can be completed from different async tasks.
    pub fn call_static(&self, req: S::Req) -> PipelineCall<S>
    where
        S: 'static,
    {
        let pl = self.clone();

        PipelineCall {
            fut: Box::pin(async move {
                let st = pl.state.st(&req);
                Ctx::<S>::new(pl.index, pl.state.waiters_ref(), st.get_ref())
                    .call(&pl.state.s, req)
                    .await
            }),
        }
    }

    #[inline]
    /// Call the service and create a future that resolves to the service result.
    ///
    /// This call can be completed from different async tasks.
    /// Note: this call does not check service readiness.
    pub fn call_nowait(&self, req: S::Req) -> PipelineCall<S>
    where
        S: 'static,
    {
        let pl = self.clone();

        PipelineCall {
            fut: Box::pin(async move {
                let st = pl.state.st(&req);
                Ctx::<S>::new(pl.index, pl.state.waiters_ref(), st.get_ref())
                    .call_nowait(&pl.state.s, req)
                    .await
            }),
        }
    }

    #[inline]
    /// Shuts down the enclosed service.
    pub async fn shutdown(&self) {
        self.state.s.shutdown().await;
    }
}

impl<S: Service> Clone for PipelineBinding<S> {
    fn clone(&self) -> Self {
        Self {
            index: self.state.waiters.insert(),
            state: self.state.clone(),
        }
    }
}

impl<S: Service> Drop for PipelineBinding<S> {
    #[inline]
    fn drop(&mut self) {
        self.state.waiters.remove(self.index);
    }
}

impl<S: Service + fmt::Debug> fmt::Debug for PipelineBinding<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineBinding")
            .field("idx", &self.index)
            .field("state", &self.state)
            .finish()
    }
}

#[must_use = "futures do nothing unless polled"]
/// Pipeline call
pub struct PipelineCall<S: Service> {
    fut: Call<S::Res, S::Error>,
}

type Call<R, E> = Pin<Box<dyn Future<Output = Result<R, E>> + 'static>>;

impl<S: Service> Future for PipelineCall<S> {
    type Output = Result<S::Res, S::Error>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.as_mut().fut).poll(cx)
    }
}

impl<S: Service> fmt::Debug for PipelineCall<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineCall").finish()
    }
}

fn ready<S>(pl: &'static PipelineState<S>) -> impl Future<Output = Result<(), S::Error>>
where
    S: Service,
{
    pl.s.ready(ReadyCtx::<'_, S>::new(0, pl.waiters_ref(), &pl.st))
}

struct CheckReadiness<S, F, Fut>
where
    S: Service + 'static,
{
    f: F,
    fut: Option<Fut>,
    pl: &'static PipelineState<S>,
}

impl<S: Service, F, Fut> Unpin for CheckReadiness<S, F, Fut> {}

impl<S: Service, F, Fut> Drop for CheckReadiness<S, F, Fut> {
    fn drop(&mut self) {
        // future got dropped during polling, we must notify other waiters
        if self.fut.is_some() {
            self.pl.waiters.notify();
        }
    }
}

impl<S, F, Fut> Future for CheckReadiness<S, F, Fut>
where
    S: Service,
    F: Fn(&'static PipelineState<S>) -> Fut,
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
