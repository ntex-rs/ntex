use crate::{BindCtx, Ctx, Middleware, Service, readiness::ReadyScope};

/// Middleware that wraps each service in a fresh [`ServiceBinding`].
///
/// Each created service gets an independent, notification-invalidated readiness
/// cache. Successful readiness survives calls and changes to sibling services.
/// The inner service must subscribe to readiness-change notifications in
/// [`Service::bind`], as described by [`ServiceBinding`].
///
/// Construction does not retain state, bind the service, or check readiness.
/// The enclosing pipeline or service must bind the returned wrapper before use.
///
/// # Example
///
/// ```
/// use ntex_service::{ServiceBindingMiddleware, factory, fn_service};
///
/// # async fn example() {
/// let factory = factory(fn_service(async |req: u32| Ok::<_, ()>(req + 1)))
///     .apply(ServiceBindingMiddleware);
///
/// let pipeline = factory.pipeline(()).await.unwrap();
/// assert_eq!(pipeline.call(1).await, Ok(2));
/// # }
/// ```
#[derive(Copy, Clone, Default, Debug)]
pub struct ServiceBindingMiddleware;

impl<S, St> Middleware<S, St> for ServiceBindingMiddleware {
    type Service = ServiceBinding<S>;

    #[inline]
    fn create(&self, _: &St, service: S) -> Self::Service {
        ServiceBinding::new(service)
    }
}

/// A service paired with persistent readiness.
///
/// Construct directly with [`Self::new`] or apply [`ServiceBindingMiddleware`]
/// to a factory to wrap each service it creates.
///
/// [`Ctx::ready`] and [`Ctx::call`] reuse the binding's successful readiness
/// until a subscribed [`crate::ReadyNotifier`] invalidates it. Calls do not
/// consume readiness. Services whose readiness can change must subscribe through
/// [`BindCtx::subscribe`] in [`Service::bind`] and notify when it changes.
/// Direct [`Service::call`] also checks this binding's cache through `Ctx`.
///
/// A binding owns its service, not pipeline state. Its scope is allocated lazily
/// in the first pipeline that binds it. Rebinding within that pipeline reuses
/// its scope and forwards binding to the wrapped service; binding the same
/// instance to another pipeline panics.
///
/// [`Self::with_data`] assigns user data passed to every [`crate::ReadyCallback`]
/// registered in this binding's scope. The default value is `0`.
/// Cloning a binding preserves its data and clones the service into a fresh,
/// unbound scope.
/// Use [`std::rc::Rc<ServiceBinding<S>>`] to share readiness within one pipeline.
/// To share a gate across pipelines, use [`crate::Pipeline`] or
/// [`crate::PipelineState`] and [`crate::ReadyLink`] instead.
///
/// Every enclosing service must forward [`Service::bind`] to this binding.
/// Runtime operations panic if the calling scope has not bound it, rather than
/// silently adding a dependency or losing readiness notifications.
#[derive(Debug)]
pub struct ServiceBinding<S> {
    service: S,
    scope: ReadyScope,
    data: usize,
}

impl<S> ServiceBinding<S> {
    /// Pairs a service with a fresh readiness scope and callback data `0`.
    ///
    /// Its dependencies are wired when a pipeline or enclosing service binds it.
    pub fn new(service: S) -> Self {
        Self::with_data(service, 0)
    }

    /// Pairs a service with a fresh readiness scope and user-defined callback data.
    ///
    /// Every callback registered in this scope receives `data`, including when
    /// notified by a dependency. The value is installed before the inner
    /// service's [`Service::bind`] hook and is preserved when the binding is
    /// cloned or rebound. Construction does not allocate readiness storage.
    pub fn with_data(service: S, data: usize) -> Self {
        Self {
            service,
            scope: ReadyScope::new(),
            data,
        }
    }

    /// Enters this binding's scope from a scope that has bound it.
    fn enter<'a, T, St>(&'a self, ctx: Ctx<'a, Self, St>) -> Ctx<'a, T, St> {
        ctx.scope().assert_dependency(&self.scope);
        ctx.with_scope(&self.scope)
    }

    #[cfg(test)]
    pub(crate) fn is_ready(&self) -> bool {
        self.scope.is_ready()
    }
}

impl<S: Clone> Clone for ServiceBinding<S> {
    fn clone(&self) -> Self {
        Self::with_data(self.service.clone(), self.data)
    }
}

impl<S, St, Req> Service<St, Req> for ServiceBinding<S>
where
    S: Service<St, Req>,
{
    type Res = S::Res;
    type Error = S::Error;

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.depend_on(&self.scope);
        self.scope.set_data(self.data);
        self.service.bind(BindCtx::new(&self.scope));
    }

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        let ctx = self.enter(ctx);
        self.scope.check(|| self.service.ready(ctx)).await
    }

    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        let ctx = self.enter::<Self, _>(ctx);
        if !ctx.is_call_nowait() {
            ctx.ready_cached(self).await?;
        }
        self.service.call(req, ctx.cast()).await
    }

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        self.service.shutdown(self.enter(ctx)).await;
    }
}

#[cfg(test)]
#[path = "../tests/unit/service_binding.rs"]
mod tests;
