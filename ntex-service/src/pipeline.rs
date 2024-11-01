use std::task::{Context, Poll};
use std::{fmt, future::poll_fn, future::Future, pin::Pin, rc::Rc};

use crate::{ctx::WaitersRef, Service, ServiceCtx};

#[derive(Debug)]
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
            state: Rc::new(PipelineState {
                svc,
                waiters
            })
        }
    }

    #[inline]
    /// Return reference to enclosed service
    pub fn get_ref(&self) -> &S {
        &self.state.svc
    }

    #[inline]
    /// Returns when the service is able to process requests.
    pub async fn ready<R>(
        &self,
    ) -> Option<impl Future<Output = Result<(), S::Error>> + use<'_, S, R>>
    where
        S: Service<R>,
    {
        self.state.svc.ready().await
    }

    #[inline]
    /// Wait for service readiness and then create future object
    /// that resolves to service result.
    pub async fn call<R>(&self, req: R) -> Result<S::Response, S::Error>
    where
        S: Service<R>,
    {
        let ctx = ServiceCtx::<'_, S>::new(self.index, self.state.waiters_ref());

        // check service readiness
        ctx.check_readiness(&self.state.svc).await?;

        // call service
        self.state.svc.call(req, ctx).await
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
                ServiceCtx::<S>::new(pl.index, pl.state.waiters_ref())
                    .call(&pl.state.svc, req)
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
                ServiceCtx::<S>::new(pl.index, pl.state.waiters_ref())
                    .call_nowait(&pl.state.svc, req)
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
        self.state.svc.shutdown().await
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

impl<S> Drop for Pipeline<S> {
    #[inline]
    fn drop(&mut self) {
        self.state.waiters.remove_index(self.index);
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

impl<S> fmt::Debug for PipelineState<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineState")
            .field("waiters", &self.waiters.get().len())
            .finish()
    }
}

/// Bound container for a service.
pub struct Readiness<S, R>
where
    S: Service<R>,
{
    st: Rc<PipelineState<S>>,
    fut: Pin<Box<dyn Future<Output = Result<(), S::Error>>>>,
}

impl<S, R> Readiness<S, R>
where
    S: Service<R> + 'static,
    R: 'static,
{
    fn new(pl: Pipeline<S>) -> Self {
        let st = pl.state.clone();
        let fut = Box::pin(async move {
            println!("READINESS");
            loop {
                if let Some(fut) = pl.state.svc.ready().await {
                    println!("ready");
                    let result = fut.await;
                    println!("not ready");
                    result?
                } else {
                    println!("always ready");
                    std::future::pending().await
                }
            }
        });

        Readiness { st, fut }
    }

    #[inline]
    /// Returns when the service is able to process requests.
    pub async fn ready(&mut self) -> Result<(), S::Error> {
        poll_fn(|cx| self.poll_ready(cx)).await
    }

    #[inline]
    /// Returns `Ready` when the pipeline is able to process requests.
    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        println!("\n\n== POLL-READY ==");
        if let Poll::Ready(res) = Pin::new(&mut self.fut).poll(cx) {
            println!("READY -- 1");
            Poll::Ready(res)
        } else {
            Poll::Pending
        }
    }
}

impl<S, R> fmt::Debug for Readiness<S, R>
where
    S: Service<R> + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Readiness")
            .field("waiters", &self.st.waiters.len())
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
