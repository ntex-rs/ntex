use std::{fmt, future, pin::Pin, task::Context, task::Poll};

use crate::pl_inner::PipelineApi;
use crate::{IntoService, Service, ServiceCaller, util::BoxFuture};

pub use crate::pl_factory::PipelineFactory;
pub use crate::pl_state::{PipelineState, PipelineStateBinding};

/// Execution container for a service and its state.
///
/// A pipeline coordinates readiness, calls, and shutdown for the enclosed
/// service chain.
pub struct Pipeline<Req, Res, Err> {
    api: PipelineApi<Req, Res, Err>,
}

/// An independently registered handle to a [`Pipeline`].
///
/// Bindings share the pipeline and its readiness state. Cloning a binding
/// registers another handle.
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
    /// Creates a pipeline containing `service` and `state`.
    pub fn new<S, St>(st: St, service: impl IntoService<S, St, Req>) -> Self
    where
        S: Service<St, Req, Res = Res, Error = Err> + 'static,
        St: 'static,
    {
        Pipeline {
            api: PipelineApi::new(service.into_service(), st),
        }
    }

    #[inline]
    /// Returns when the pipeline is ready to process requests.
    ///
    /// A successful check is consumed by the next call, which then skips
    /// its own readiness check.
    pub async fn ready(&self) -> Result<(), Err> {
        future::poll_fn(|cx| self.api.poll_ready(cx)).await
    }

    #[inline]
    /// Waits for readiness, then calls the service.
    ///
    /// The readiness check is skipped if the last pipeline readiness check
    /// succeeded and no call has started since.
    pub async fn call(&self, req: Req) -> Result<Res, Err> {
        let pl = self.bind();
        pl.api.call(pl.idx, req).await
    }

    #[inline]
    /// Returns an owned future that waits for readiness and calls the service.
    ///
    /// Unlike [`Pipeline::call`], the returned future does not borrow the
    /// pipeline and can be moved between local tasks. The readiness check is
    /// skipped if the last pipeline readiness check succeeded and no call has
    /// started since; this is decided when the future is first polled.
    pub fn call_static(&self, req: Req) -> PipelineCall<Req, Res, Err> {
        PipelineCall::new(self.bind(), req)
    }

    #[inline]
    /// Returns `Ready` when the pipeline is ready to process requests.
    ///
    /// A successful check is consumed by the next call, which then skips
    /// its own readiness check.
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
    /// Creates a new binding to this pipeline.
    ///
    /// The binding can be used to check readiness and call the service.
    pub fn bind(&self) -> PipelineBinding<Req, Res, Err> {
        PipelineBinding::new(self)
    }
}

impl<Req, Res, Err> ServiceCaller<Req, Res, Err> for Pipeline<Req, Res, Err>
where
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    #[inline]
    async fn call_service(&self, req: Req) -> Result<Res, Err> {
        let pl = self.bind();
        pl.api.call(pl.idx, req).await
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
    /// Waits until the pipeline is ready to process a request.
    ///
    /// A successful check is consumed by the next call, which then skips
    /// its own readiness check.
    pub async fn ready(&self) -> Result<(), Err> {
        self.api.ready(self.idx).await
    }

    #[inline]
    /// Waits for readiness, then calls the service.
    ///
    /// The readiness check is skipped if the last pipeline readiness check
    /// succeeded and no call has started since.
    pub async fn call(&self, req: Req) -> Result<Res, Err> {
        let pl = self.clone();
        pl.api.call(pl.idx, req).await
    }

    #[inline]
    /// Returns an owned future that waits for readiness and calls the service.
    ///
    /// The returned future does not borrow this binding and can be moved between
    /// local tasks. The readiness check is skipped if the last pipeline readiness
    /// check succeeded and no call has started since; this is decided when the
    /// future is first polled.
    pub fn call_static(&self, req: Req) -> PipelineCall<Req, Res, Err> {
        PipelineCall::new(self.clone(), req)
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
/// An owned future for a pipeline service call.
///
/// Created by [`Pipeline::call_static`] and [`PipelineBinding::call_static`]. The
/// future keeps the pipeline alive. It does not check whether the pipeline is shut
/// down; the request is passed to the service as usual.
pub struct PipelineCall<Req, Res, Err> {
    // `fut` borrows from `pl`, so it must be declared (and dropped) first
    fut: BoxFuture<'static, Result<Res, Err>>,
    #[allow(dead_code)]
    pl: PipelineBinding<Req, Res, Err>,
}

impl<Req, Res, Err> PipelineCall<Req, Res, Err> {
    #[allow(clippy::missing_transmute_annotations)]
    fn new(pl: PipelineBinding<Req, Res, Err>, req: Req) -> Self {
        // SAFETY: `fut` borrows from `pl.api` (`Rc`-allocated, never moves).
        // `fut` is declared before `pl` in `PipelineCall`, so it is dropped first.
        PipelineCall {
            fut: unsafe { std::mem::transmute(pl.api.call(pl.idx, req)) },
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
