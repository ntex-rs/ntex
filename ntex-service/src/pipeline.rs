use std::{cell, fmt, future::Future, pin::Pin, rc::Rc, task::Context, task::Poll};

use crate::{ctx::Waiters, Service, ServiceCtx};

#[derive(Debug)]
/// Container for a service.
///
/// Container allows to call enclosed service and adds support of shared readiness.
pub struct Pipeline<S> {
    svc: Rc<S>,
    pub(crate) waiters: Waiters,
}

impl<S> Pipeline<S> {
    #[inline]
    /// Construct new container instance.
    pub fn new(svc: S) -> Self {
        Pipeline {
            svc: Rc::new(svc),
            waiters: Waiters::new(),
        }
    }

    #[inline]
    /// Return reference to enclosed service
    pub fn get_ref(&self) -> &S {
        self.svc.as_ref()
    }

    #[inline]
    /// Returns when the service is able to process requests.
    pub async fn ready<R>(&self) -> Result<(), S::Error>
    where
        S: Service<R>,
    {
        ServiceCtx::<'_, S>::new(&self.waiters)
            .ready(self.svc.as_ref())
            .await
    }

    #[inline]
    /// Wait for service readiness and then create future object
    /// that resolves to service result.
    pub async fn call<R>(&self, req: R) -> Result<S::Response, S::Error>
    where
        S: Service<R>,
    {
        let ctx = ServiceCtx::<'_, S>::new(&self.waiters);

        // check service readiness
        self.svc.as_ref().ready(ctx).await?;

        // call service
        self.svc.as_ref().call(req, ctx).await
    }

    #[inline]
    /// Wait for service readiness and then create future object
    /// that resolves to service result.
    pub fn call_static<R>(&self, req: R) -> PipelineCall<S, R>
    where
        S: Service<R> + 'static,
        R: 'static,
    {
        let pl = self.clone();

        PipelineCall {
            fut: Box::pin(async move {
                ServiceCtx::<S>::new(&pl.waiters)
                    .call(pl.svc.as_ref(), req)
                    .await
            }),
        }
    }

    #[inline]
    /// Call service and create future object that resolves to service result.
    ///
    /// Note, this call does not check service readiness.
    pub fn call_nowait<R>(&self, req: R) -> PipelineCall<S, R>
    where
        S: Service<R> + 'static,
        R: 'static,
    {
        let pl = self.clone();

        PipelineCall {
            fut: Box::pin(async move {
                ServiceCtx::<S>::new(&pl.waiters)
                    .call_nowait(pl.svc.as_ref(), req)
                    .await
            }),
        }
    }

    #[inline]
    /// Shutdown enclosed service.
    pub async fn shutdown<R>(&self)
    where
        S: Service<R>,
    {
        self.svc.as_ref().shutdown().await
    }

    #[inline]
    /// Convert to lifetime object.
    pub fn bind<R>(self) -> PipelineBinding<S, R>
    where
        S: Service<R> + 'static,
        R: 'static,
    {
        PipelineBinding::new(self)
    }
}

impl<S> From<S> for Pipeline<S> {
    #[inline]
    fn from(svc: S) -> Self {
        Pipeline::new(svc)
    }
}

impl<S> Clone for Pipeline<S> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            svc: self.svc.clone(),
            waiters: self.waiters.clone(),
        }
    }
}

/// Bound container for a service.
pub struct PipelineBinding<S, R>
where
    S: Service<R>,
{
    pl: Pipeline<S>,
    st: cell::UnsafeCell<State<S::Error>>,
}

enum State<E> {
    New,
    Readiness(Pin<Box<dyn Future<Output = Result<(), E>> + 'static>>),
    Shutdown(Pin<Box<dyn Future<Output = ()> + 'static>>),
}

impl<S, R> PipelineBinding<S, R>
where
    S: Service<R> + 'static,
    R: 'static,
{
    fn new(pl: Pipeline<S>) -> Self {
        PipelineBinding {
            pl,
            st: cell::UnsafeCell::new(State::New),
        }
    }

    #[inline]
    /// Return reference to enclosed service
    pub fn get_ref(&self) -> &S {
        self.pl.svc.as_ref()
    }

    #[inline]
    /// Returns `Ready` when the pipeline is able to process requests.
    ///
    /// panics if .poll_shutdown() was called before.
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        let st = unsafe { &mut *self.st.get() };

        match st {
            State::New => {
                // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
                // Pipeline::svc is heap allocated(Rc<S>), and it is being kept alive until
                // `self` is alive
                let pl: &'static Pipeline<S> = unsafe { std::mem::transmute(&self.pl) };
                let fut = Box::pin(CheckReadiness {
                    fut: None,
                    f: ready,
                    pl,
                });
                *st = State::Readiness(fut);
                self.poll_ready(cx)
            }
            State::Readiness(ref mut fut) => Pin::new(fut).poll(cx),
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
                self.poll_shutdown(cx)
            }
            State::Shutdown(ref mut fut) => Pin::new(fut).poll(cx),
        }
    }

    #[inline]
    /// Wait for service readiness and then create future object
    /// that resolves to service result.
    pub fn call(&self, req: R) -> PipelineCall<S, R> {
        let pl = self.pl.clone();

        PipelineCall {
            fut: Box::pin(async move {
                ServiceCtx::<S>::new(&pl.waiters)
                    .call(pl.svc.as_ref(), req)
                    .await
            }),
        }
    }

    #[inline]
    /// Call service and create future object that resolves to service result.
    ///
    /// Note, this call does not check service readiness.
    pub fn call_nowait(&self, req: R) -> PipelineCall<S, R> {
        let pl = self.pl.clone();

        PipelineCall {
            fut: Box::pin(async move {
                ServiceCtx::<S>::new(&pl.waiters)
                    .call_nowait(pl.svc.as_ref(), req)
                    .await
            }),
        }
    }

    #[inline]
    /// Shutdown enclosed service.
    pub async fn shutdown(&self) {
        self.pl.svc.as_ref().shutdown().await
    }
}

impl<S, R> Drop for PipelineBinding<S, R>
where
    S: Service<R>,
{
    fn drop(&mut self) {
        self.st = cell::UnsafeCell::new(State::New);
    }
}

impl<S, R> Clone for PipelineBinding<S, R>
where
    S: Service<R>,
{
    #[inline]
    fn clone(&self) -> Self {
        Self {
            pl: self.pl.clone(),
            st: cell::UnsafeCell::new(State::New),
        }
    }
}

impl<S, R> fmt::Debug for PipelineBinding<S, R>
where
    S: Service<R> + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineBinding")
            .field("pipeline", &self.pl)
            .finish()
    }
}

#[must_use = "futures do nothing unless polled"]
/// Pipeline call future
pub struct PipelineCall<S, R>
where
    S: Service<R>,
    R: 'static,
{
    fut: Call<S::Response, S::Error>,
}

type Call<R, E> = Pin<Box<dyn Future<Output = Result<R, E>> + 'static>>;

impl<S, R> Future for PipelineCall<S, R>
where
    S: Service<R>,
{
    type Output = Result<S::Response, S::Error>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.as_mut().fut).poll(cx)
    }
}

impl<S, R> fmt::Debug for PipelineCall<S, R>
where
    S: Service<R>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineCall").finish()
    }
}

fn ready<S, R>(pl: &'static Pipeline<S>) -> impl Future<Output = Result<(), S::Error>>
where
    S: Service<R>,
    R: 'static,
{
    pl.svc.ready(ServiceCtx::<'_, S>::new(&pl.waiters))
}

struct CheckReadiness<S: 'static, F, Fut> {
    f: F,
    fut: Option<Fut>,
    pl: &'static Pipeline<S>,
}

impl<S, F, Fut> Unpin for CheckReadiness<S, F, Fut> {}

impl<S, F, Fut> Drop for CheckReadiness<S, F, Fut> {
    fn drop(&mut self) {
        // future fot dropped during polling, we must notify other waiters
        if self.fut.is_some() {
            self.pl.waiters.notify();
        }
    }
}

impl<T, S, F, Fut> Future for CheckReadiness<S, F, Fut>
where
    F: Fn(&'static Pipeline<S>) -> Fut,
    Fut: Future<Output = T>,
{
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut slf = self.as_mut();

        // register pipeline tag
        slf.pl.waiters.register_pipeline(cx);

        if slf.pl.waiters.can_check(cx) {
            if let Some(ref mut fut) = slf.fut {
                match unsafe { Pin::new_unchecked(fut) }.poll(cx) {
                    Poll::Pending => {
                        slf.pl.waiters.register(cx);
                        Poll::Pending
                    }
                    Poll::Ready(res) => {
                        let _ = slf.fut.take();
                        slf.pl.waiters.notify();
                        Poll::Ready(res)
                    }
                }
            } else {
                slf.fut = Some((slf.f)(slf.pl));
                self.poll(cx)
            }
        } else {
            Poll::Pending
        }
    }
}
