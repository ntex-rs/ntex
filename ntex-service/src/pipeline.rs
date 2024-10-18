use std::task::{Context, Poll};
use std::{fmt, future::poll_fn, future::Future, pin::Pin, rc::Rc};

use crate::{ctx::Waiters, ctx::WaitersRef, Service, ServiceCtx};

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
    /// Returns when the service is able to process requests.
    pub async fn unready<R>(&self) -> Result<(), S::Error>
    where
        S: Service<R>,
    {
        self.waiters.set_ready();
        let result = self.svc.unready().await;
        self.waiters.set_notready();
        result
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
        ctx.ready(self.svc.as_ref()).await?;

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
    /// Get readiness object for current pipeline.
    ///
    /// Panics if more that one `Readiness` instance get created
    pub fn bind<R>(&self) -> Readiness<S, R>
    where
        S: Service<R> + 'static,
        R: 'static,
    {
        Readiness::new(self.clone())
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
pub struct Readiness<S, R>
where
    S: Service<R>,
{
    wt: Rc<WaitersRef>,
    fut: Pin<Box<dyn Future<Output = Result<(), S::Error>>>>,
}

impl<S, R> Readiness<S, R>
where
    S: Service<R> + 'static,
    R: 'static,
{
    fn new(pl: Pipeline<S>) -> Self {
        if pl.waiters.is_bound() {
            panic!("Cannot bind pipeline more that one time");
        }
        let wt = pl.waiters.waiters.clone();
        wt.bind();

        let fut = Box::pin(async move {
            loop {
                pl.svc
                    .ready(ServiceCtx::<'_, S>::new_bound(&pl.waiters))
                    .await?;
                pl.unready().await?;
            }
        });

        Readiness { wt, fut }
    }

    #[inline]
    /// Returns when the service is able to process requests.
    pub async fn ready(&mut self) -> Result<(), S::Error> {
        poll_fn(|cx| self.poll_ready(cx)).await
    }

    #[inline]
    /// Returns `Ready` when the pipeline is able to process requests.
    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        if let Poll::Ready(res) = Pin::new(&mut self.fut).poll(cx) {
            Poll::Ready(res)
        } else if self.wt.is_ready() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

impl<S, R> Drop for Readiness<S, R>
where
    S: Service<R>,
{
    fn drop(&mut self) {
        self.wt.unbind();
    }
}

impl<S, R> fmt::Debug for Readiness<S, R>
where
    S: Service<R> + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Readiness")
            .field("waiters", &self.wt.len())
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
