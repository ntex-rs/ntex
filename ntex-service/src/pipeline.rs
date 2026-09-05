use std::{fmt, future, pin::Pin, task::Context, task::Poll};

use crate::pl_inner::PipelineApi;
use crate::{IntoService, Service, util::BoxFuture};

pub use crate::pl_factory::PipelineFactory;
pub use crate::pl_state::{PipelineState, PipelineStateBinding};

/// Container for a service.
///
/// Provides a way to call the enclosed service and share its readiness state.
pub struct Pipeline<Req, Res, Err> {
    api: PipelineApi<Req, Res, Err>,
}

/// Bound container for a service.
pub struct PipelineBinding<Req, Res, Err> {
    idx: u32,
    api: PipelineApi<Req, Res, Err>,
}

impl<Req, Res, Err> Pipeline<Req, Res, Err>
where
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    #[inline]
    /// Construct new service pipeline instance with default state.
    pub fn new<S, St>(st: St, f: impl IntoService<S, St, Req>) -> Self
    where
        S: Service<St, Req, Res = Res, Error = Err> + 'static,
        St: 'static,
    {
        Pipeline {
            api: PipelineApi::new(f.into_service(), st),
        }
    }

    #[inline]
    /// Returns when the pipeline is ready to process requests.
    pub async fn ready(&self) -> Result<(), Err> {
        future::poll_fn(|cx| self.api.poll_ready(cx)).await
    }

    #[inline]
    /// Wait for service readiness, then create a future
    /// that resolves to the service call result.
    pub async fn call(&self, req: Req) -> Result<Res, Err> {
        let pl = self.bind();
        pl.api.call(pl.idx, req, true).await
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
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Err>> {
        self.api.poll_ready(cx)
    }

    #[inline]
    /// Returns `Ready` when the service has been properly shut down.
    pub fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        self.api.poll_shutdown(cx)
    }

    #[inline]
    /// Checks whether pipeline shutdown has been initiated.
    pub fn is_shutdown(&self) -> bool {
        self.api.is_shutdown()
    }

    #[inline]
    /// Shuts down the enclosed service.
    pub async fn shutdown(&self) {
        future::poll_fn(|cx| self.api.poll_shutdown(cx)).await;
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
        self.api.unreg(0);
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
            idx: pl.api.reg(),
            api: pl.api.clone(),
        }
    }

    pub(crate) fn with(idx: u32, api: PipelineApi<Req, Res, Err>) -> Self {
        Self { idx, api }
    }

    #[inline]
    /// Returns when the pipeline is ready to process requests.
    pub async fn ready(&self) -> Result<(), Err> {
        self.api.ready(self.idx).await
    }

    #[inline]
    /// Wait for service readiness, then create a future
    /// that resolves to the service call result.
    pub async fn call(&self, req: Req) -> Result<Res, Err> {
        let pl = self.clone();
        pl.api.call(pl.idx, req, true).await
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
}

impl<Req, Res, Err> Drop for PipelineBinding<Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.api.unreg(self.idx);
    }
}

impl<Req, Res, Err> Clone for PipelineBinding<Req, Res, Err> {
    fn clone(&self) -> Self {
        Self {
            idx: self.api.reg(),
            api: self.api.clone(),
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
            fut: unsafe { std::mem::transmute(pl.api.call(pl.idx, req, ready)) },
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

impl<Req, Res, Err> fmt::Debug for Pipeline<Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pipeline").finish()
    }
}

impl<Req, Res, Err> fmt::Debug for PipelineBinding<Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineBinding")
            .field("idx", &self.idx)
            .finish()
    }
}

impl<Req, Res, Err> fmt::Debug for PipelineCall<Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineCall").finish()
    }
}
