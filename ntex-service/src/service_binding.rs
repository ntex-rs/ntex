use crate::{BindCtx, Ctx, Service, readiness::ReadyScope};

/// A service paired with persistent readiness.
///
/// [`Ctx::ready`] and [`Ctx::call`] reuse the binding's successful readiness
/// until a subscribed [`crate::ReadyNotifier`] invalidates it. Calls do not
/// consume readiness. Services whose readiness can change must subscribe through
/// [`BindCtx::subscribe`] in [`Service::bind`] and notify when it changes.
/// Direct [`Service::call`] also checks this binding's cache through `Ctx`.
///
/// A binding owns its service, not pipeline state. Its readiness describes one
/// shared gate regardless of the state passed to a check or call. Independent
/// gates require separate bindings.
///
/// Cloning a binding clones the service into a fresh, initially invalid scope.
/// Use [`std::rc::Rc<ServiceBinding<S>>`] to share a service and its readiness.
///
/// Every enclosing service must forward [`Service::bind`] to this binding.
/// Runtime operations panic if the calling scope has not bound it, rather than
/// silently adding a dependency or losing readiness notifications.
#[derive(Debug)]
pub struct ServiceBinding<S> {
    service: S,
    scope: ReadyScope,
}

impl<S> ServiceBinding<S> {
    /// Pairs a service with a fresh readiness scope.
    ///
    /// Its dependencies are wired when a pipeline or enclosing service binds it.
    pub fn new(service: S) -> Self {
        Self {
            service,
            scope: ReadyScope::new(),
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
        Self::new(self.service.clone())
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
