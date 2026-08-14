use std::{cell, fmt, future, marker, pin::Pin, ptr, rc::Rc, task::Context, task::Poll};

use crate::{Ctx, IntoService, ReadyCtx, Service, ctx::WaitersRef};

/// Container for a service.
///
/// Container allows to call enclosed service and adds support of shared readiness.
pub struct Pipeline<S, St> {
    index: u32,
    state: Rc<PipelineState<S>>,
    st: marker::PhantomData<St>,
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

impl<S, St> Pipeline<S, St> {
    #[inline]
    /// Construct new container instance.
    pub fn new(svc: S) -> Self {
        let (index, waiters) = WaitersRef::new();
        Pipeline {
            index,
            st: marker::PhantomData,
            state: Rc::new(PipelineState { svc, waiters }),
        }
    }

    #[inline]
    /// Return reference to enclosed service
    pub fn get_ref(&self) -> &S {
        &self.state.svc
    }

    #[inline]
    /// Returns when the pipeline is able to process requests.
    pub async fn ready<Req>(&self, st: &St) -> Result<(), S::Error>
    where
        S: Service<St, Req>,
    {
        Ctx::<'_, S, St>::new(self.index, self.state.waiters_ref(), st)
            .ready(&self.state.svc)
            .await
    }

    #[inline]
    /// Wait for service readiness and then create future object
    /// that resolves to service result.
    pub async fn call<Req>(&self, req: Req, st: &St) -> Result<S::Res, S::Error>
    where
        S: Service<St, Req>,
    {
        Ctx::<'_, S, St>::new(self.index, self.state.waiters_ref(), st)
            .call(&self.state.svc, req)
            .await
    }

    #[inline]
    /// Wait for service readiness and then create future object
    /// that resolves to service result.
    pub fn call_static<Req>(&self, req: Req, st: St) -> PipelineCall<S::Res, S::Error>
    where
        S: Service<St, Req> + 'static,
        St: 'static,
        Req: 'static,
    {
        let pl = self.clone();

        PipelineCall {
            fut: Box::pin(async move {
                Ctx::<S, St>::new(pl.index, pl.state.waiters_ref(), &st)
                    .call(&pl.state.svc, req)
                    .await
            }),
        }
    }

    #[inline]
    /// Call service and create future object that resolves to service result.
    ///
    /// Note, this call does not check service readiness.
    pub fn call_nowait<Req>(&self, req: Req, st: St) -> PipelineCall<S::Res, S::Error>
    where
        S: Service<St, Req> + 'static,
        St: 'static,
        Req: 'static,
    {
        let pl = self.clone();

        PipelineCall {
            fut: Box::pin(async move {
                Ctx::<S, St>::new(pl.index, pl.state.waiters_ref(), &st)
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
    pub async fn shutdown<Req>(&self)
    where
        S: Service<St, Req>,
    {
        self.state.svc.shutdown().await;
    }

    #[inline]
    pub fn poll<Req>(&self, cx: &mut Context<'_>) -> Result<(), S::Error>
    where
        S: Service<St, Req>,
    {
        self.state.svc.poll(cx)
    }

    #[inline]
    /// Get current pipeline.
    pub fn bind<Req>(self) -> PipelineBinding<S, St, Req>
    where
        S: Service<St, Req> + 'static,
        St: Default + 'static,
        Req: 'static,
    {
        PipelineBinding::new(self, St::default())
    }

    #[inline]
    /// Bind pipeline to a state.
    pub fn bind_state<Req>(self, st: St) -> PipelineBinding<S, St, Req>
    where
        S: Service<St, Req> + 'static,
        St: 'static,
        Req: 'static,
    {
        PipelineBinding::new(self, st)
    }
}

impl<S, St> Clone for Pipeline<S, St> {
    fn clone(&self) -> Self {
        Pipeline {
            index: self.state.waiters.insert(),
            state: self.state.clone(),
            st: marker::PhantomData,
        }
    }
}

impl<S, St> Drop for Pipeline<S, St> {
    #[inline]
    fn drop(&mut self) {
        self.state.waiters.remove(self.index);
    }
}

impl<S: fmt::Debug, St> fmt::Debug for Pipeline<S, St> {
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
pub struct PipelineSvc<S, St> {
    inner: Pipeline<S, St>,
}

impl<S, St> PipelineSvc<S, St> {
    #[inline]
    /// Construct new `PipelineSvc`
    pub fn new(inner: Pipeline<S, St>) -> Self {
        Self { inner }
    }
}

impl<S, St, Req> Service<St, Req> for PipelineSvc<S, St>
where
    S: Service<St, Req>,
{
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, S::Error> {
        ctx.call(self.inner.get_ref(), req).await
    }

    #[inline]
    async fn ready(&self, ctx: ReadyCtx<'_, Self, St>) -> Result<(), Self::Error> {
        ctx.ready(self.inner.get_ref()).await
    }

    #[inline]
    async fn shutdown(&self) {
        self.inner.shutdown().await;
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        self.inner.poll(cx)
    }
}

impl<S, St> Clone for PipelineSvc<S, St> {
    fn clone(&self) -> Self {
        PipelineSvc {
            inner: self.inner.clone(),
        }
    }
}

impl<S, St, Req> IntoService<PipelineSvc<S, St>, St, Req> for Pipeline<S, St>
where
    S: Service<St, Req>,
{
    #[inline]
    fn into_service(self) -> PipelineSvc<S, St> {
        PipelineSvc::new(self)
    }
}

/// Bound container for a service.
pub struct PipelineBinding<S, St, Req>
where
    S: Service<St, Req>,
{
    pl: Pipeline<S, St>,
    state: Rc<St>,
    st: cell::UnsafeCell<State<S::Error>>,
}

enum State<E> {
    New,
    Readiness(Pin<Box<dyn future::Future<Output = Result<(), E>> + 'static>>),
    Shutdown(Pin<Box<dyn future::Future<Output = ()> + 'static>>),
}

impl<S, St, Req> PipelineBinding<S, St, Req>
where
    S: Service<St, Req> + 'static,
    St: 'static,
    Req: 'static,
{
    fn new(pl: Pipeline<S, St>, state: St) -> Self {
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
    pub fn pipeline(&self) -> Pipeline<S, St> {
        self.pl.clone()
    }

    #[inline]
    pub fn poll(&self, cx: &mut Context<'_>) -> Result<(), S::Error> {
        self.pl.poll(cx)
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
                let pl: &'static Pipeline<S, St> = unsafe { std::mem::transmute(&self.pl) };
                let state: &'static St = unsafe { &*ptr::from_ref(self.state.as_ref()) };
                let fut = Box::pin(CheckReadiness {
                    pl,
                    state,
                    f: ready::<S, St, Req>,
                    fut: None,
                    _t: marker::PhantomData,
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
                let pl: &'static Pipeline<S, St> = unsafe { std::mem::transmute(&self.pl) };
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
    pub fn call(&self, req: Req) -> PipelineCall<S::Res, S::Error> {
        let pl = self.pl.clone();
        let state = self.state.clone();

        PipelineCall {
            fut: Box::pin(async move {
                Ctx::<S, St>::new(pl.index, pl.state.waiters_ref(), &state)
                    .call(&pl.state.svc, req)
                    .await
            }),
        }
    }

    #[inline]
    /// Call service and create future object that resolves to service result.
    ///
    /// Note, this call does not check service readiness.
    pub fn call_nowait(&self, req: Req) -> PipelineCall<S::Res, S::Error> {
        let pl = self.pl.clone();
        let state = self.state.clone();

        PipelineCall {
            fut: Box::pin(async move {
                Ctx::<S, St>::new(pl.index, pl.state.waiters_ref(), &state)
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

impl<S, St, Req> Drop for PipelineBinding<S, St, Req>
where
    S: Service<St, Req>,
{
    fn drop(&mut self) {
        self.st = cell::UnsafeCell::new(State::New);
    }
}

impl<S, St, Req> Clone for PipelineBinding<S, St, Req>
where
    S: Service<St, Req>,
{
    #[inline]
    fn clone(&self) -> Self {
        Self {
            pl: self.pl.clone(),
            state: self.state.clone(),
            st: cell::UnsafeCell::new(State::New),
        }
    }
}

impl<S, St, Req> Service<St, Req> for PipelineBinding<S, St, Req>
where
    S: Service<St, Req>,
{
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, S::Error> {
        ctx.call_with_st(&self.pl.state.svc, req, &self.state).await
    }

    #[inline]
    async fn ready(&self, ctx: ReadyCtx<'_, Self, St>) -> Result<(), Self::Error> {
        ctx.ready_with_st(&self.pl.state.svc, &self.state).await
    }

    #[inline]
    async fn shutdown(&self) {
        self.pl.shutdown().await;
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        self.pl.poll(cx)
    }
}

impl<S, St, Req> fmt::Debug for PipelineBinding<S, St, Req>
where
    S: Service<St, Req> + fmt::Debug,
    St: fmt::Debug,
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
pub struct PipelineCall<R, E> {
    fut: Call<R, E>,
}

type Call<R, E> = Pin<Box<dyn future::Future<Output = Result<R, E>> + 'static>>;

impl<R, E> future::Future for PipelineCall<R, E> {
    type Output = Result<R, E>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.as_mut().fut).poll(cx)
    }
}

impl<R, E> fmt::Debug for PipelineCall<R, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineCall").finish()
    }
}

fn ready<S, St, Req>(
    pl: &'static Pipeline<S, St>,
    st: &St,
) -> impl future::Future<Output = Result<(), S::Error>>
where
    S: Service<St, Req>,
{
    pl.state.svc.ready(ReadyCtx::<'_, S, St>::new(
        pl.index,
        pl.state.waiters_ref(),
        Some(st),
    ))
}

struct CheckReadiness<S, St, Req, F, Fut>
where
    S: Service<St, Req> + 'static,
    St: 'static,
{
    f: F,
    fut: Option<Fut>,
    pl: &'static Pipeline<S, St>,
    state: &'static St,
    _t: marker::PhantomData<Req>,
}

impl<S: Service<St, Req>, St, Req, F, Fut> Unpin for CheckReadiness<S, St, Req, F, Fut> {}

impl<S: Service<St, Req>, St, Req, F, Fut> Drop for CheckReadiness<S, St, Req, F, Fut> {
    fn drop(&mut self) {
        // future got dropped during polling, we must notify other waiters
        if self.fut.is_some() {
            self.pl.state.waiters.notify();
        }
    }
}

impl<S, St, Req, F, Fut> future::Future for CheckReadiness<S, St, Req, F, Fut>
where
    S: Service<St, Req>,
    St: 'static,
    F: Fn(&'static Pipeline<S, St>, &'static St) -> Fut,
    Fut: future::Future<Output = Result<(), S::Error>>,
{
    type Output = Result<(), S::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut();

        this.pl.poll(cx)?;

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
    use std::{cell::Cell, future::poll_fn, rc::Rc};

    use super::*;

    #[derive(Debug, Default, Clone)]
    struct Srv(Rc<Cell<usize>>);

    impl Service<(), ()> for Srv {
        type Res = ();
        type Error = ();

        async fn ready(&self, _: ReadyCtx<'_, Self, ()>) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn call(&self, _m: (), _: Ctx<'_, Self, ()>) -> Result<(), ()> {
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
        let svc = Srv(cnt_sht.clone()).map(|()| "ok");
        let srv = Pipeline::new(PipelineSvc::from(&svc));
        let res = srv.call((), &()).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "ok");

        let res = srv.ready(&()).await;
        assert_eq!(res, Ok(()));

        let res = poll_fn(|cx| Poll::Ready(srv.poll(cx))).await;
        assert_eq!(res, Ok(()));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 1);
        let _ = format!("{srv:?}");
    }
}
