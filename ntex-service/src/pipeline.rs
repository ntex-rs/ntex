use std::{fmt, future, pin::Pin, task::Context, task::Poll};

use crate::pl_inner::PipelineApi;
use crate::{BindCtx, IntoService, ReadyCallback, Service, ServiceCaller, util::BoxFuture};

pub use crate::pl_factory::PipelineFactory;
pub use crate::pl_state::{PipelineState, PipelineStateBinding};

/// Execution container for a service and its state.
///
/// A pipeline coordinates readiness, calls, and shutdown for the enclosed
/// service chain.
/// Successful readiness is shared by every binding until a subscribed
/// [`crate::ReadyNotifier`] invalidates it. Calls do not consume readiness.
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
    /// Reuses valid shared readiness without allocating a readiness future.
    /// Cancelling the active caller drops its check and lets another waiter
    /// take ownership. Returns successfully if pipeline shutdown has started,
    /// like [`Self::poll_ready`].
    ///
    /// The first caller that finds readiness invalid runs the shared check in its
    /// own future, and other waiters wait for it. Like a held lock, a pending
    /// future must be polled or dropped: keeping it alive without polling stalls
    /// every other readiness waiter on this pipeline.
    pub async fn ready(&self) -> Result<(), Err> {
        let mut ready = std::pin::pin!(self.api.ready(0));
        future::poll_fn(|cx| {
            if self.api.is_shutdown() {
                Poll::Ready(Ok(()))
            } else {
                ready.as_mut().poll(cx)
            }
        })
        .await
    }

    #[inline]
    /// Returns `true` while the pipeline's shared readiness is valid.
    ///
    /// Does not run or register a readiness check. `false` means readiness is
    /// unknown or was invalidated, not that the service is not ready.
    pub fn is_ready(&self) -> bool {
        self.api.is_ready()
    }

    #[inline]
    /// Waits for readiness, then calls the service.
    ///
    /// Reuses valid shared readiness without consuming it.
    pub async fn call(&self, req: Req) -> Result<Res, Err> {
        let pl = self.bind();
        pl.api.call(pl.idx, req).await
    }

    #[inline]
    /// Returns an owned future that waits for readiness and calls the service.
    ///
    /// Unlike [`Pipeline::call`], the returned future does not borrow the
    /// pipeline and can be moved between local tasks. Shared readiness is
    /// consulted when the future is polled and the call is dispatched, not
    /// when the future is constructed.
    pub fn call_static(&self, req: Req) -> PipelineCall<Req, Res, Err> {
        PipelineCall::new(self.bind(), req)
    }

    #[inline]
    /// Returns `Ready` when the pipeline is ready to process requests.
    ///
    /// Registers the task for readiness changes and reuses valid shared
    /// readiness. A pending check is retained by the pipeline, even if this
    /// task stops polling; other callers can continue driving it.
    /// The retained check and its wake sources run on this pipeline's thread.
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

    #[inline]
    /// Registers a callback for notifications reaching this pipeline's scope.
    ///
    /// Registration is shared with all bindings and persists until the scope
    /// is dropped. Repeated registration of the same handle is idempotent.
    /// See [`ReadyCallback`] for notification semantics.
    pub fn on_notify(&self, callback: &ReadyCallback) {
        self.api.on_notify(callback);
    }

    #[inline]
    /// Registers this pipeline as a readiness dependency of the enclosing service.
    ///
    /// Call from [`Service::bind`] so changes to this pipeline's readiness also
    /// invalidate the enclosing scope. Repeated registration is idempotent and
    /// does not rebind the inner service or create another runtime handle.
    pub fn bind_to(&self, ctx: &BindCtx<'_>) {
        self.api.bind_to(ctx);
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
    /// Reuses valid shared readiness without consuming it or allocating a
    /// readiness future.
    ///
    /// The first caller that finds readiness invalid runs the shared check in its
    /// own future, and other waiters wait for it. Like a held lock, a pending
    /// future must be polled or dropped: keeping it alive without polling stalls
    /// every other readiness waiter on this pipeline.
    pub async fn ready(&self) -> Result<(), Err> {
        self.api.ready(self.idx).await
    }

    #[inline]
    /// Waits for readiness, then calls the service.
    ///
    /// Reuses valid shared readiness without consuming it.
    pub async fn call(&self, req: Req) -> Result<Res, Err> {
        let pl = self.clone();
        pl.api.call(pl.idx, req).await
    }

    #[inline]
    /// Returns an owned future that waits for readiness and calls the service.
    ///
    /// The returned future does not borrow this binding and can be moved between
    /// local tasks. Shared readiness is consulted when the future is polled
    /// and the call is dispatched, not when the future is constructed.
    pub fn call_static(&self, req: Req) -> PipelineCall<Req, Res, Err> {
        PipelineCall::new(self.clone(), req)
    }

    #[inline]
    /// Registers a callback on the shared pipeline scope.
    ///
    /// Dropping this binding does not remove the registration.
    /// See [`Pipeline::on_notify`] and [`ReadyCallback`] for details.
    pub fn on_notify(&self, callback: &ReadyCallback) {
        self.api.on_notify(callback);
    }

    #[inline]
    /// Registers this handle's pipeline as a readiness dependency.
    ///
    /// Call from [`Service::bind`]. See [`Pipeline::bind_to`] for details.
    pub fn bind_to(&self, ctx: &BindCtx<'_>) {
        self.api.bind_to(ctx);
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
