use std::{fmt, marker::PhantomPinned, ptr, rc::Rc, task::Context, task::Poll};

use crate::{BindCtx, Ctx, IntoService, ReadyCallback, Service, ctx::WaitersRef, util::BoxFuture};

use crate::pipeline::PipelineBinding;
use crate::pl_inner::{PipelineApi, PipelineInternalApi, PipelineReadiness};
use crate::readiness::ReadyScope;

/// Execution container for a service whose state is supplied per operation.
///
/// Unlike [`crate::Pipeline`], this type does not own a state value. Callers
/// provide a state reference when checking readiness, calling, or shutting
/// down the service.
///
/// All bindings share one readiness cache and checker, including bindings with
/// different state values. Readiness may inspect state, but must describe the
/// shared pipeline-wide gate. Independent gates require separate pipelines.
pub struct PipelineState<St, Req, Res, Err> {
    api: Rc<dyn PipelineStateApi<St, Req, Res, Err>>,
}

impl<St, Req, Res, Err> PipelineState<St, Req, Res, Err>
where
    St: 'static,
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    #[inline]
    /// Creates a state-independent pipeline containing `service`.
    pub fn new<S>(service: impl IntoService<S, St, Req>) -> Self
    where
        S: Service<St, Req, Res = Res, Error = Err> + 'static,
        St: 'static,
    {
        let waiters = WaitersRef::new();
        let s = service.into_service();
        s.bind(BindCtx::new(waiters.scope()));
        PipelineState {
            api: Rc::new(PipelineInner {
                s,
                readiness: PipelineReadiness::new(waiters.scope()),
                waiters,
                _pinned: PhantomPinned,
            }),
        }
    }

    #[inline]
    /// Returns when the pipeline is ready to process requests.
    ///
    /// Successful readiness is shared and reused until a subscribed
    /// [`crate::ReadyNotifier`] invalidates it. Calls do not consume it.
    /// Reusing valid readiness does not allocate a readiness future.
    ///
    /// The first caller that finds readiness invalid runs the shared check in its
    /// own future, and other waiters wait for it. Like a held lock, a pending
    /// future must be polled or dropped: keeping it alive without polling stalls
    /// every other readiness waiter on this pipeline.
    pub async fn ready(&self, st: &St) -> Result<(), Err> {
        if self.is_ready() {
            Ok(())
        } else {
            self.api.ready(0, st).await
        }
    }

    #[inline]
    /// Returns `true` while the pipeline's shared readiness is valid.
    ///
    /// Does not run or register a readiness check. `false` means readiness is
    /// unknown or was invalidated, not that the service is not ready.
    pub fn is_ready(&self) -> bool {
        self.api.scope().is_ready()
    }

    #[inline]
    /// Waits for readiness, then calls the service with `st`.
    ///
    /// Reuses valid shared readiness without consuming it.
    pub async fn call(&self, req: Req, st: &St) -> Result<Res, Err> {
        let pl = self.binding();
        self.api.call(pl.idx, req, st).await
    }

    #[inline]
    /// Shuts down the enclosed service.
    pub async fn shutdown(&self, st: &St) {
        self.api.shutdown(0, st).await;
    }

    #[inline]
    /// Returns `Ready` when the pipeline is ready to process requests.
    ///
    /// Registers the task for readiness changes and reuses valid shared
    /// readiness. A pending check is retained by the pipeline, even if this
    /// task stops polling; other callers can continue driving it.
    /// The retained check and its wake sources run on this pipeline's thread.
    ///
    /// # Panics
    ///
    /// Panics if `.shutdown()` has been called. Unlike [`crate::Pipeline::poll_ready`],
    /// it does not return `Ready(Ok(()))` after shutdown.
    pub fn poll_ready(&self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), Err>>
    where
        St: Clone,
    {
        self.api.poll_ready(cx, st)
    }

    fn binding(&self) -> Binding<'_, St, Req, Res, Err> {
        Binding {
            idx: self.api.reg(),
            api: self.api.as_ref(),
        }
    }

    #[inline]
    /// Creates a binding that accepts state per call.
    ///
    /// The binding can be used to call the service.
    pub fn bind(&self) -> PipelineStateBinding<St, Req, Res, Err> {
        PipelineStateBinding {
            idx: self.api.reg(),
            api: self.api.clone(),
        }
    }

    #[inline]
    /// Creates a standard pipeline binding by attaching an owned state value.
    ///
    /// The binding can be used to call the service.
    pub fn bind_state(&self, st: St) -> PipelineBinding<Req, Res, Err>
    where
        St: Clone,
    {
        let internal = PipelineInternal {
            st,
            api: self.api.clone(),
        };

        PipelineBinding::with(self.api.reg(), PipelineApi::with(internal))
    }

    #[inline]
    /// Registers a callback for notifications reaching this pipeline's scope.
    ///
    /// Registration is shared by all bindings, regardless of their state, and
    /// persists until the scope is dropped. Repeated registration of the same
    /// handle is idempotent. See [`ReadyCallback`] for notification semantics.
    pub fn on_notify(&self, callback: &ReadyCallback) {
        self.api.scope().on_notify(callback);
    }

    #[inline]
    /// Registers this pipeline as a readiness dependency of the enclosing service.
    ///
    /// Call from [`Service::bind`] so changes to this pipeline's readiness also
    /// invalidate the enclosing scope. Repeated registration is idempotent and
    /// does not rebind the inner service or create another runtime handle.
    pub fn bind_to(&self, ctx: &BindCtx<'_>) {
        ctx.depend_on(self.api.scope());
    }
}

impl<St, Req, Res, Err> Drop for PipelineState<St, Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.api.unreg(0);
    }
}

struct Binding<'a, St, Req, Res, Err> {
    idx: u32,
    api: &'a dyn PipelineStateApi<St, Req, Res, Err>,
}

impl<St, Req, Res, Err> Drop for Binding<'_, St, Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.api.unreg(self.idx);
    }
}

// ========================== `PipelineStateBinding` ===========================

/// An independently registered handle to a [`PipelineState`].
pub struct PipelineStateBinding<St, Req, Res, Err> {
    idx: u32,
    api: Rc<dyn PipelineStateApi<St, Req, Res, Err>>,
}

impl<St, Req, Res, Err> Drop for PipelineStateBinding<St, Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.api.unreg(self.idx);
    }
}

impl<St, Req, Res, Err> Clone for PipelineStateBinding<St, Req, Res, Err> {
    #[inline]
    fn clone(&self) -> Self {
        PipelineStateBinding {
            idx: self.api.reg(),
            api: self.api.clone(),
        }
    }
}

impl<St, Req, Res, Err> PipelineStateBinding<St, Req, Res, Err>
where
    St: 'static,
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    #[inline]
    /// Waits for readiness, then calls the service with `st`.
    ///
    /// Reuses valid shared readiness without consuming it, regardless of `st`.
    pub async fn call(&self, req: Req, st: &St) -> Result<Res, Err> {
        let pl = Binding {
            idx: self.api.reg(),
            api: self.api.as_ref(),
        };
        pl.api.call(pl.idx, req, st).await
    }

    #[inline]
    /// Registers a callback on the shared pipeline scope, independently of state.
    ///
    /// Dropping this binding does not remove the registration.
    /// See [`PipelineState::on_notify`] and [`ReadyCallback`] for details.
    pub fn on_notify(&self, callback: &ReadyCallback) {
        self.api.scope().on_notify(callback);
    }

    #[inline]
    /// Registers this handle's pipeline as a readiness dependency.
    ///
    /// Call from [`Service::bind`]. See [`PipelineState::bind_to`] for details.
    pub fn bind_to(&self, ctx: &BindCtx<'_>) {
        ctx.depend_on(self.api.scope());
    }
}

// ========================== `PipelineApi` ===========================

struct PipelineInternal<St, Req, Res, Err> {
    st: St,
    api: Rc<dyn PipelineStateApi<St, Req, Res, Err>>,
}

impl<St, Req, Res, Err> PipelineInternalApi<Req, Res, Err> for PipelineInternal<St, Req, Res, Err> {
    fn scope(&self) -> &ReadyScope {
        self.api.scope()
    }

    fn reg(&self) -> u32 {
        self.api.reg()
    }

    fn unreg(&self, idx: u32) {
        self.api.unreg(idx);
    }

    fn ready(&self, idx: u32) -> BoxFuture<'_, Result<(), Err>> {
        self.api.ready(idx, &self.st)
    }

    fn call(&self, idx: u32, req: Req) -> BoxFuture<'_, Result<Res, Err>> {
        self.api.call(idx, req, &self.st)
    }

    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Err>> {
        unreachable!()
    }

    fn poll_shutdown(&self, _: &mut Context<'_>) -> Poll<()> {
        unreachable!()
    }

    fn is_shutdown(&self) -> bool {
        self.api.is_shutdown()
    }
}

// ========================== `PipelineStateApi` ===========================

struct PipelineInner<S, E> {
    s: S,
    waiters: WaitersRef,
    readiness: PipelineReadiness<E>,
    // The pending readiness future borrows the other fields. `!Unpin` keeps
    // `&mut self` in `Drop` from invalidating those borrows.
    _pinned: PhantomPinned,
}

impl<S, E> Drop for PipelineInner<S, E> {
    fn drop(&mut self) {
        // The readiness future borrows `s` and `waiters`, so it must be dropped
        // before the fields it references
        self.readiness.cancel();
    }
}

trait PipelineStateApi<St, Req, Res, Err> {
    fn scope(&self) -> &ReadyScope;

    fn reg(&self) -> u32;
    fn unreg(&self, idx: u32);

    fn call<'a>(&'a self, idx: u32, req: Req, st: &'a St) -> BoxFuture<'a, Result<Res, Err>>
    where
        Req: 'a;

    fn ready<'a>(&'a self, idx: u32, st: &'a St) -> BoxFuture<'a, Result<(), Err>>
    where
        Req: 'a;

    fn poll_ready(&self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), Err>>
    where
        St: Clone;

    fn shutdown<'a>(&'a self, idx: u32, st: &'a St) -> BoxFuture<'a, ()>;

    fn is_shutdown(&self) -> bool;
}

impl<S, St, Req, E> PipelineStateApi<St, Req, S::Res, S::Error> for PipelineInner<S, E>
where
    S: Service<St, Req, Error = E> + 'static,
    St: 'static,
    Req: 'static,
    E: 'static,
{
    fn scope(&self) -> &ReadyScope {
        self.waiters.scope()
    }

    fn reg(&self) -> u32 {
        self.waiters.insert()
    }

    fn unreg(&self, idx: u32) {
        if idx == 0 {
            self.readiness.unregister();
        }
        self.waiters.remove(idx);
    }

    fn ready<'a>(&'a self, idx: u32, st: &'a St) -> BoxFuture<'a, Result<(), S::Error>>
    where
        Req: 'a,
    {
        Box::pin(self.readiness.ready(move || async move {
            Ctx::<'_, S, St>::new(idx, &self.waiters, st)
                .ready_generation(&self.s)
                .await
        }))
    }

    fn shutdown<'a>(&'a self, idx: u32, st: &'a St) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.waiters.shutdown();
            self.readiness.cancel();

            Ctx::<'_, S, St>::new(idx, &self.waiters, st)
                .shutdown(&self.s)
                .await;
        })
    }

    fn call<'a>(&'a self, idx: u32, req: Req, st: &'a St) -> BoxFuture<'a, Result<S::Res, S::Error>>
    where
        Req: 'a,
    {
        Box::pin(async move {
            let ctx = Ctx::<'_, S, St>::new(idx, &self.waiters, st);
            self.readiness
                .ready(|| ctx.ready_generation(&self.s))
                .await?;
            self.s.call(req, ctx).await
        })
    }

    fn poll_ready(&self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), S::Error>>
    where
        St: Clone,
    {
        assert!(!self.waiters.is_shutdown(), "Pipeline is shutting down");
        self.readiness.poll_ready(cx, || {
            // SAFETY: `self` is heap allocated (`Rc<PipelineInner>`) and never moves.
            // `fut` is stored in `self.readiness`, which is reset before other
            // fields are dropped (see `Drop for PipelineInner`), so `pl` outlives `fut`.
            let pl = unsafe { &*(ptr::from_ref(self)) };
            let st = st.clone();
            Box::pin(async move {
                let registration = pl.waiters.register();
                Ctx::<'_, S, St>::new(registration.idx, &pl.waiters, &st)
                    .ready_generation(&pl.s)
                    .await
            })
        })
    }

    fn is_shutdown(&self) -> bool {
        self.waiters.is_shutdown()
    }
}

impl<St, Req, Res, Err> fmt::Debug for PipelineState<St, Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineState").finish()
    }
}

impl<St, Req, Res, Err> fmt::Debug for PipelineStateBinding<St, Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineStateBinding").finish()
    }
}

#[cfg(test)]
#[path = "../tests/unit/pl_state.rs"]
mod tests;
