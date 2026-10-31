use crate::{ReadyCallback, ReadyLink, ReadyNotifier, Service, readiness::ReadyScope};

/// Initialization-only access to a service's readiness dependencies.
///
/// A pipeline or [`crate::Ctx::create`] supplies this borrowed context to
/// [`Service::bind`] before the new service's readiness checks, calls, or shutdown.
/// Subscribe every notification source and bind every existing child used by
/// the service, including call-only children.
/// Dependencies must be available from the service itself. Binding cannot
/// perform runtime operations.
///
/// The context cannot be constructed by services or retained for later use.
/// Runtime [`crate::Ctx`] cannot register dependencies directly; its explicit
/// [`create`](crate::Ctx::create) method initializes and binds new children.
///
/// ```
/// use ntex_service::{BindCtx, Ctx, ReadyCallback, ReadyNotifier, Service};
///
/// struct Forward<S> {
///     inner: S,
///     changed: ReadyNotifier,
///     on_notify: ReadyCallback,
/// }
///
/// impl<S: Service<(), u8>> Service<(), u8> for Forward<S> {
///     type Res = S::Res;
///     type Error = S::Error;
///
///     fn bind(&self, ctx: BindCtx<'_>) {
///         ctx.subscribe(&self.changed);
///         ctx.on_notify(&self.on_notify);
///         ctx.bind(&self.inner);
///     }
///
///     async fn ready(&self, ctx: Ctx<'_, Self>) -> Result<(), Self::Error> {
///         ctx.ready(&self.inner).await
///     }
///
///     async fn call(&self, req: u8, ctx: Ctx<'_, Self>) -> Result<Self::Res, Self::Error> {
///         ctx.call(&self.inner, req).await
///     }
/// }
/// ```
///
/// ```compile_fail
/// use ntex_service::{Ctx, ReadyNotifier};
/// fn register_later(ctx: Ctx<'_, ()>, changed: &ReadyNotifier) {
///     ctx.subscribe(changed);
/// }
/// ```
///
/// ```compile_fail
/// use ntex_service::{BindCtx, Ctx, Service};
/// fn bind_later<S: Service<(), ()>>(ctx: Ctx<'_, ()>, service: &S) {
///     service.bind(ctx);
/// }
/// ```
///
/// ```compile_fail
/// use ntex_service::{BindCtx, Ctx, Service};
/// struct Retained(std::cell::RefCell<Option<BindCtx<'static>>>);
/// impl Service<(), ()> for Retained {
///     type Res = ();
///     type Error = ();
///     fn bind(&self, ctx: BindCtx<'_>) {
///         *self.0.borrow_mut() = Some(ctx);
///     }
///     async fn call(&self, _: (), _: Ctx<'_, Self>) -> Result<(), ()> {
///         Ok(())
///     }
/// }
/// ```
#[derive(Debug)]
pub struct BindCtx<'a> {
    scope: &'a ReadyScope,
}

impl<'a> BindCtx<'a> {
    pub(crate) fn new(scope: &'a ReadyScope) -> Self {
        Self { scope }
    }

    /// Subscribes this scope to a source of readiness changes.
    ///
    /// Subscriptions are appended without deduplication and persist across
    /// completed or cancelled checks. Repeated subscriptions cause repeated
    /// invalidations and callback delivery. Entries for dropped scopes are
    /// ignored and remain stored until the source is dropped.
    ///
    /// # Panics
    ///
    /// Panics if the source is currently delivering a notification.
    pub fn subscribe(&self, source: &ReadyNotifier) {
        self.scope.subscribe(source);
    }

    /// Subscribes this scope to another pipeline's readiness invalidations.
    ///
    /// Obtain the endpoint with [`crate::Pipeline::readiness_link`]. Repeated
    /// registration is idempotent and survives dropping the link handle.
    /// This does not run a readiness check: the service must still await the
    /// source pipeline's readiness. A dropped source makes the link inert.
    pub fn subscribe_pipeline(&self, link: &ReadyLink) {
        self.scope.subscribe_pipeline(link);
    }

    /// Registers a callback for notifications reaching this readiness scope.
    ///
    /// Raw services share this scope; a [`crate::ReadyBinding`] owns its own.
    /// Repeated registration of the same handle is idempotent. See
    /// [`ReadyCallback`] for notification and lifetime semantics.
    /// The callback receives this scope's data from
    /// [`crate::ReadyBinding::with_data`], or `0` by default.
    ///
    /// # Panics
    ///
    /// Panics if this scope is currently executing notification callbacks.
    pub fn on_notify(&self, callback: &ReadyCallback) {
        self.scope.on_notify(callback);
    }

    /// Binds a child service to this readiness scope.
    ///
    /// Raw services share this scope. A [`crate::ReadyBinding`] uses its own
    /// scope and registers it as a dependency of this one.
    /// Use explicit type arguments when the child has multiple [`Service`]
    /// implementations.
    /// For pipeline children, use [`crate::Pipeline::bind_to`] instead.
    pub fn bind<S, St, Req>(&self, service: &S)
    where
        S: Service<St, Req>,
    {
        service.bind(Self::new(self.scope));
    }

    pub(crate) fn depend_on(&self, scope: &ReadyScope) {
        self.scope.depend_on(scope);
    }
}
