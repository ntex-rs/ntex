use std::{fmt, future::poll_fn, marker::PhantomPinned, rc::Rc, task::Context, task::Poll};

use crate::ready_call::ReadyThenCall;
use crate::util::{BoxFuture, WithOwner, box_future};
use crate::{BindCtx, Ctx, IntoService, ReadyCallback, ReadyLink, Service, ctx::WaitersRef};

use crate::pipeline::PipelineBinding;
use crate::pl_inner::{PipelineApi, PipelineInternalApi, PipelineReadiness, detach};
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
///
/// Shutdown is advisory, as for [`crate::Pipeline`]: readiness checks return
/// `Ok(())` once shutdown has started, and calls are still forwarded to the
/// service. Callers that must stop admitting work check
/// [`PipelineState::is_shutdown`].
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
    /// Returns successfully if pipeline shutdown has started.
    pub fn ready<'a>(&'a self, st: &'a St) -> impl Future<Output = Result<(), Err>> {
        let mut check = None;
        poll_fn(move |cx| {
            let check = match check {
                Some(ref mut check) => check,
                None if self.is_ready() || self.is_shutdown() => return Poll::Ready(Ok(())),
                None => check.insert(self.api.ready(0, st)),
            };
            check.as_mut().poll(cx)
        })
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
    /// Reuses valid shared readiness without consuming it. Calls are forwarded
    /// even after shutdown has started; see [`PipelineState`].
    pub fn call<'a>(&'a self, req: Req, st: &'a St) -> impl Future<Output = Result<Res, Err>> {
        self.api.call(req, st)
    }

    #[inline]
    /// Shuts down the enclosed service.
    ///
    /// Marks the pipeline as shutting down, then waits for the service's
    /// shutdown. Later calls are not rejected; see [`PipelineState`].
    pub fn shutdown<'a>(&'a self, st: &'a St) -> impl Future<Output = ()> {
        self.api.shutdown(0, st)
    }

    #[inline]
    /// Checks whether pipeline shutdown has been initiated.
    ///
    /// The pipeline does not reject calls after shutdown; callers use this to
    /// stop admitting new work.
    pub fn is_shutdown(&self) -> bool {
        self.api.is_shutdown()
    }

    #[inline]
    /// Returns `Ready` when the pipeline is ready to process requests.
    ///
    /// Registers the task for readiness changes and reuses valid shared
    /// readiness. A pending check is retained by the pipeline, even if this
    /// task stops polling; other callers can continue driving it.
    /// The retained check and its wake sources run on this pipeline's thread.
    /// Its checker wakers must also be finally dropped on that thread, even
    /// after the pipeline is dropped; foreign wakes or final drops panic.
    /// Returns `Ready(Ok(()))` if pipeline shutdown has started, like
    /// [`crate::Pipeline::poll_ready`].
    pub fn poll_ready(&self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), Err>>
    where
        St: Clone,
    {
        if self.is_shutdown() {
            Poll::Ready(Ok(()))
        } else {
            self.api.poll_ready(cx, st)
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
    /// handle is idempotent. The callback receives `0` for this root scope.
    /// See [`ReadyCallback`] for notification semantics.
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
        ctx.subscribe_pipeline(&self.readiness_link());
    }

    /// Returns this pipeline's readiness endpoint, shared by all state bindings.
    ///
    /// Register with [`BindCtx::subscribe_pipeline`]. The link propagates
    /// invalidation without checking readiness or retaining the pipeline.
    pub fn readiness_link(&self) -> ReadyLink {
        self.api.scope().readiness_link()
    }
}

impl<St, Req, Res, Err> Drop for PipelineState<St, Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.api.unreg(0);
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
    /// Calls are forwarded even after shutdown has started; see [`PipelineState`].
    pub fn call<'a>(&'a self, req: Req, st: &'a St) -> impl Future<Output = Result<Res, Err>> {
        self.api.call(req, st)
    }

    #[inline]
    /// Checks whether pipeline shutdown has been initiated.
    ///
    /// See [`PipelineState::is_shutdown`].
    pub fn is_shutdown(&self) -> bool {
        self.api.is_shutdown()
    }

    #[inline]
    /// Registers a callback on the shared pipeline scope, independently of state.
    ///
    /// The callback receives `0` for this root scope.
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
        ctx.subscribe_pipeline(&self.readiness_link());
    }

    /// Returns the shared pipeline's readiness endpoint.
    ///
    /// See [`PipelineState::readiness_link`] and [`ReadyLink`].
    pub fn readiness_link(&self) -> ReadyLink {
        self.api.scope().readiness_link()
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

    fn call(&self, req: Req) -> BoxFuture<'_, Result<Res, Err>> {
        self.api.call(req, &self.st)
    }

    fn call_static(self: Rc<Self>, req: Req) -> BoxFuture<'static, Result<Res, Err>> {
        self.api.clone().call_static(req, self)
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

impl<S, E> PipelineInner<S, E> {
    fn call_impl<'a, St, Req>(
        &'a self,
        idx: u32,
        req: Req,
        st: &'a St,
    ) -> impl Future<Output = Result<S::Res, E>>
    where
        S: Service<St, Req, Error = E>,
    {
        ReadyThenCall::new(
            (self, Ctx::<'a, S, St>::new(idx, &self.waiters, st)),
            req,
            |&(this, ctx): &(&'a Self, Ctx<'a, S, St>)| {
                (!this.waiters.scope().is_ready())
                    .then(|| this.readiness.ready(move || ctx.ready_generation(&this.s)))
            },
            |(this, ctx): (&'a Self, Ctx<'a, S, St>), req| this.s.call(req, ctx.into_call()),
        )
    }
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

    /// Calls the service; the returned future owns the call's waiter slot.
    fn call<'a>(&'a self, req: Req, st: &'a St) -> BoxFuture<'a, Result<Res, Err>>
    where
        Req: 'a;

    /// Calls with a future that keeps `self` and `owner`, which holds the state, alive.
    fn call_static(
        self: Rc<Self>,
        req: Req,
        owner: Rc<PipelineInternal<St, Req, Res, Err>>,
    ) -> BoxFuture<'static, Result<Res, Err>>;

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

    fn call<'a>(&'a self, req: Req, st: &'a St) -> BoxFuture<'a, Result<S::Res, S::Error>>
    where
        Req: 'a,
    {
        box_future(|| {
            let reg = self.waiters.register();
            WithOwner {
                fut: self.call_impl(reg.idx, req, st),
                owner: reg,
            }
        })
    }

    fn call_static(
        self: Rc<Self>,
        req: Req,
        owner: Rc<PipelineInternal<St, Req, S::Res, S::Error>>,
    ) -> BoxFuture<'static, Result<S::Res, S::Error>> {
        // SAFETY: both are `Rc`-allocated, and `WithOwner` drops the future and
        // then the registration before them
        let (pl, st) = unsafe { (detach(&*self), &detach(&*owner).st) };
        box_future(move || {
            let reg = pl.waiters.register();
            // Tuple fields are dropped in order, so `reg` is released before `self`
            WithOwner {
                fut: pl.call_impl(reg.idx, req, st),
                owner: (reg, self, owner),
            }
        })
    }

    fn poll_ready(&self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), S::Error>>
    where
        St: Clone,
    {
        self.readiness.poll_ready(cx, || {
            // SAFETY: the future is stored in `self.readiness`
            let pl = unsafe { detach(self) };
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
