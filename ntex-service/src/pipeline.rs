use std::{cell, fmt, future, pin::Pin, ptr, rc::Rc, task::Context, task::Poll};

use crate::{Ctx, IntoService, ReadyCtx, Service, ctx::WaitersRef};

/// Container for a service.
///
/// Container allows to call enclosed service and adds support of shared readiness.
pub struct Pipeline<S> {
    index: u32,
    state: Rc<PipelineState<S>>,
}

struct PipelineState<S> {
    svc: S,
    waiters: WaitersRef,
}

impl<S> PipelineState<S> {
    pub(crate) fn waiters_ref(&self) -> &WaitersRef {
        &self.waiters
    }
}

impl<S> Pipeline<S> {
    #[inline]
    /// Construct new container instance.
    pub fn new(svc: S) -> Self {
        let (index, waiters) = WaitersRef::new();
        Pipeline {
            index,
            state: Rc::new(PipelineState { svc, waiters }),
        }
    }

    #[inline]
    /// Return reference to enclosed service
    pub fn get_ref(&self) -> &S {
        &self.state.svc
    }
}

impl<S: Service> Pipeline<S> {
    #[inline]
    /// Returns when the pipeline is able to process requests.
    pub async fn ready(&self, st: &S::St) -> Result<(), S::Error> {
        Ctx::<'_, S>::new(self.index, self.state.waiters_ref(), st)
            .ready(&self.state.svc)
            .await
    }

    #[inline]
    /// Wait for service readiness and then create future object
    /// that resolves to service result.
    pub async fn call(&self, req: S::Req, st: &S::St) -> Result<S::Res, S::Error> {
        Ctx::<'_, S>::new(self.index, self.state.waiters_ref(), st)
            .call(&self.state.svc, req)
            .await
    }

    #[inline]
    /// Wait for service readiness and then create future object
    /// that resolves to service result.
    pub fn call_static(&self, req: S::Req, st: S::St) -> PipelineCall<S>
    where
        S: 'static,
    {
        let pl = self.clone();

        PipelineCall {
            fut: Box::pin(async move {
                Ctx::<S>::new(pl.index, pl.state.waiters_ref(), &st)
                    .call(&pl.state.svc, req)
                    .await
            }),
        }
    }

    #[inline]
    /// Call service and create future object that resolves to service result.
    ///
    /// Note, this call does not check service readiness.
    pub fn call_nowait(&self, req: S::Req, st: S::St) -> PipelineCall<S>
    where
        S: 'static,
    {
        let pl = self.clone();

        PipelineCall {
            fut: Box::pin(async move {
                Ctx::<S>::new(pl.index, pl.state.waiters_ref(), &st)
                    .call_nowait(&pl.state.svc, req)
                    .await
            }),
        }
    }

    #[inline]
    /// Check if shutdown is initiated.
    pub fn is_shutdown(&self) -> bool {
        self.state.waiters.is_shutdown()
    }

    #[inline]
    /// Shutdown enclosed service.
    pub async fn shutdown(&self) {
        self.state.svc.shutdown().await;
    }

    #[inline]
    /// Get current pipeline.
    pub fn bind(self) -> PipelineBinding<S>
    where
        S: 'static,
        S::St: Default,
    {
        PipelineBinding::new(self, S::St::default())
    }

    #[inline]
    /// Bind pipeline to a state.
    pub fn bind_state(self, st: S::St) -> PipelineBinding<S>
    where
        S: 'static,
    {
        PipelineBinding::new(self, st)
    }
}

impl<S> Clone for Pipeline<S> {
    fn clone(&self) -> Self {
        Pipeline {
            index: self.state.waiters.insert(),
            state: self.state.clone(),
        }
    }
}

impl<S> Drop for Pipeline<S> {
    #[inline]
    fn drop(&mut self) {
        self.state.waiters.remove(self.index);
    }
}

impl<S: fmt::Debug> fmt::Debug for Pipeline<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pipeline")
            .field("idx", &self.index)
            .field("state", &self.state)
            .finish()
    }
}

impl<S: fmt::Debug> fmt::Debug for PipelineState<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineState")
            .field("svc", &self.svc)
            .field("waiters", &self.waiters.get().len())
            .finish()
    }
}

#[derive(Debug)]
/// Service wrapper for Pipeline
pub struct PipelineSvc<S> {
    inner: Pipeline<S>,
}

impl<S> PipelineSvc<S> {
    #[inline]
    /// Construct new `PipelineSvc`
    pub fn new(inner: Pipeline<S>) -> Self {
        Self { inner }
    }
}

impl<S: Service> Service for PipelineSvc<S> {
    type St = S::St;
    type Req = S::Req;
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    async fn call(&self, req: S::Req, ctx: Ctx<'_, Self>) -> Result<S::Res, S::Error> {
        ctx.call(self.inner.get_ref(), req).await
    }

    #[inline]
    async fn ready(&self, ctx: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
        ctx.ready(self.inner.get_ref()).await
    }

    #[inline]
    async fn shutdown(&self) {
        self.inner.shutdown().await;
    }
}

impl<S> Clone for PipelineSvc<S> {
    fn clone(&self) -> Self {
        PipelineSvc {
            inner: self.inner.clone(),
        }
    }
}

impl<S: Service> IntoService<PipelineSvc<S>> for Pipeline<S> {
    #[inline]
    fn into_service(self) -> PipelineSvc<S> {
        PipelineSvc::new(self)
    }
}

/// Bound container for a service.
pub struct PipelineBinding<S: Service> {
    pl: Pipeline<S>,
    state: Rc<S::St>,
    st: cell::UnsafeCell<State<S::Error>>,
}

enum State<E> {
    New,
    Readiness(Pin<Box<dyn future::Future<Output = Result<(), E>> + 'static>>),
    Shutdown(Pin<Box<dyn future::Future<Output = ()> + 'static>>),
}

impl<S: Service + 'static> PipelineBinding<S> {
    fn new(pl: Pipeline<S>, state: S::St) -> Self {
        PipelineBinding {
            pl,
            state: Rc::new(state),
            st: cell::UnsafeCell::new(State::New),
        }
    }

    #[inline]
    /// Return reference to enclosed service
    pub fn get_ref(&self) -> &S {
        &self.pl.state.svc
    }

    #[inline]
    /// Get pipeline
    pub fn pipeline(&self) -> Pipeline<S> {
        self.pl.clone()
    }

    #[inline]
    /// Returns `Ready` when the pipeline is able to process requests.
    ///
    /// # Panics
    ///
    /// Call panics if `.poll_shutdown()` was called before.
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        let st = unsafe { &mut *self.st.get() };

        match st {
            State::New => {
                // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
                // Pipeline::svc is heap allocated(Rc<S>), and it is being kept alive until
                // `self` is alive
                let pl: &'static Pipeline<S> = unsafe { std::mem::transmute(&self.pl) };
                let state: &'static S::St = unsafe { &*ptr::from_ref(self.state.as_ref()) };
                let fut = Box::pin(CheckReadiness {
                    pl,
                    state,
                    f: ready,
                    fut: None,
                });
                *st = State::Readiness(fut);
                self.poll_ready(cx)
            }
            State::Readiness(fut) => Pin::new(fut).poll(cx),
            State::Shutdown(_) => panic!("Pipeline is shutding down"),
        }
    }

    #[inline]
    /// Returns `Ready` when the service is properly shutdowns.
    pub fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        let st = unsafe { &mut *self.st.get() };

        match st {
            State::New | State::Readiness(_) => {
                // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
                // Pipeline::svc is heap allocated(Rc<S>), and it is being kept alive until
                // `self` is alive
                let pl: &'static Pipeline<S> = unsafe { std::mem::transmute(&self.pl) };
                *st = State::Shutdown(Box::pin(async move { pl.shutdown().await }));
                pl.state.waiters.shutdown();
                self.poll_shutdown(cx)
            }
            State::Shutdown(fut) => Pin::new(fut).poll(cx),
        }
    }

    #[inline]
    /// Wait for service readiness and then create future object
    /// that resolves to service result.
    pub fn call(&self, req: S::Req) -> PipelineCall<S> {
        let pl = self.pl.clone();
        let state = self.state.clone();

        PipelineCall {
            fut: Box::pin(async move {
                Ctx::<S>::new(pl.index, pl.state.waiters_ref(), &state)
                    .call(&pl.state.svc, req)
                    .await
            }),
        }
    }

    #[inline]
    /// Call service and create future object that resolves to service result.
    ///
    /// Note, this call does not check service readiness.
    pub fn call_nowait(&self, req: S::Req) -> PipelineCall<S> {
        let pl = self.pl.clone();
        let state = self.state.clone();

        PipelineCall {
            fut: Box::pin(async move {
                Ctx::<S>::new(pl.index, pl.state.waiters_ref(), &state)
                    .call_nowait(&pl.state.svc, req)
                    .await
            }),
        }
    }

    #[inline]
    /// Check if shutdown is initiated.
    pub fn is_shutdown(&self) -> bool {
        self.pl.state.waiters.is_shutdown()
    }

    #[inline]
    /// Shutdown enclosed service.
    pub async fn shutdown(&self) {
        self.pl.state.svc.shutdown().await;
    }
}

impl<S: Service> Drop for PipelineBinding<S> {
    fn drop(&mut self) {
        self.st = cell::UnsafeCell::new(State::New);
    }
}

impl<S: Service> Clone for PipelineBinding<S> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            pl: self.pl.clone(),
            state: self.state.clone(),
            st: cell::UnsafeCell::new(State::New),
        }
    }
}

impl<S: Service> Service for PipelineBinding<S> {
    type St = S::St;
    type Req = S::Req;
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    async fn call(&self, req: S::Req, ctx: Ctx<'_, Self>) -> Result<S::Res, S::Error> {
        ctx.call_with_st(&self.pl.state.svc, req, &self.state).await
    }

    #[inline]
    async fn ready(&self, ctx: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
        ctx.ready_with_st(&self.pl.state.svc, &self.state).await
    }

    #[inline]
    async fn shutdown(&self) {
        self.pl.shutdown().await;
    }
}

impl<S> fmt::Debug for PipelineBinding<S>
where
    S: Service + fmt::Debug,
    S::St: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineBinding")
            .field("pipeline", &self.pl)
            .field("state", &self.st)
            .finish()
    }
}

#[must_use = "futures do nothing unless polled"]
/// Pipeline call future
pub struct PipelineCall<S: Service> {
    fut: Call<S::Res, S::Error>,
}

type Call<R, E> = Pin<Box<dyn future::Future<Output = Result<R, E>> + 'static>>;

impl<S: Service> future::Future for PipelineCall<S> {
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

fn ready<S>(
    pl: &'static Pipeline<S>,
    st: &S::St,
) -> impl future::Future<Output = Result<(), S::Error>>
where
    S: Service,
{
    pl.state.svc.ready(ReadyCtx::<'_, S>::new(
        pl.index,
        pl.state.waiters_ref(),
        Some(st),
    ))
}

struct CheckReadiness<S, F, Fut>
where
    S: Service + 'static,
{
    f: F,
    fut: Option<Fut>,
    pl: &'static Pipeline<S>,
    state: &'static S::St,
}

impl<S: Service, F, Fut> Unpin for CheckReadiness<S, F, Fut> {}

impl<S: Service, F, Fut> Drop for CheckReadiness<S, F, Fut> {
    fn drop(&mut self) {
        // future got dropped during polling, we must notify other waiters
        if self.fut.is_some() {
            self.pl.state.waiters.notify();
        }
    }
}

impl<S, F, Fut> future::Future for CheckReadiness<S, F, Fut>
where
    S: Service,
    F: Fn(&'static Pipeline<S>, &'static S::St) -> Fut,
    Fut: future::Future<Output = Result<(), S::Error>>,
{
    type Output = Result<(), S::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut();

        this.pl.state.waiters.run(this.pl.index, cx, |cx| {
            if this.fut.is_none() {
                this.fut = Some((this.f)(this.pl, this.state));
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

#[cfg(test)]
#[allow(clippy::unused_async_trait_impl)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;

    #[derive(Debug, Default, Clone)]
    struct Srv(Rc<Cell<usize>>);

    impl Service for Srv {
        type St = ();
        type Req = ();
        type Res = ();
        type Error = ();

        async fn ready(&self, _: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn call(&self, _m: (), _: Ctx<'_, Self>) -> Result<(), ()> {
            Ok(())
        }

        async fn shutdown(&self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[ntex::test]
    async fn pipeline_service() {
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = Pipeline::new(
            Pipeline::new(Srv(cnt_sht.clone()).map(|()| "ok"))
                .into_service()
                .clone(),
        );
        let res = srv.call((), &()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "ok");

        let res = srv.ready(&()).await;
        assert_eq!(res, Ok(()));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 1);
        let _ = format!("{srv:?}");

        let cnt_sht = Rc::new(Cell::new(0));
        let svc = Pipeline::new(Srv(cnt_sht.clone()).map(|()| "ok"));
        let srv = Pipeline::new(PipelineSvc::new(svc));
        let res = srv.call((), &()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "ok");

        let res = srv.ready(&()).await;
        assert_eq!(res, Ok(()));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 1);
        let _ = format!("{srv:?}");
    }
}
