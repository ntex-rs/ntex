use super::{Ctx, Service, ServiceFactory, util};

#[derive(Debug, Clone)]
/// Service produced by the `then` combinator.
///
/// This is created by the `ServiceChain::then()` method.
/// Children share the enclosing readiness scope unless explicitly wrapped with
/// [`crate::ready`]. Calls forward directly to each child; a wrapped child checks
/// its own readiness at call time, including after the first child's call awaits.
pub struct Then<A, B> {
    svc1: A,
    svc2: B,
}

impl<A, B> Then<A, B> {
    /// Creates a new `Then` service.
    pub(crate) fn new(svc1: A, svc2: B) -> Then<A, B> {
        Self { svc1, svc2 }
    }
}

impl<A, B, St, Req> Service<St, Req> for Then<A, B>
where
    A: Service<St, Req>,
    B: Service<St, Result<A::Res, A::Error>, Error = A::Error>,
{
    type Res = B::Res;
    type Error = B::Error;

    crate::forward_bind!(St, svc1, svc2);

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<B::Res, B::Error> {
        let result = self.svc1.call(req, ctx.cast()).await;
        self.svc2.call(result, ctx.without_bypass().cast()).await
    }

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        util::ready(&self.svc1, &self.svc2, ctx).await
    }

    #[inline]
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        util::shutdown(&self.svc1, &self.svc2, ctx).await;
    }
}

#[derive(Debug, Clone)]
/// Service factory produced by the `then` combinator.
///
/// This is created by the `ServiceChainFactory::then()` method.
pub struct ThenFactory<A, B> {
    svc1: A,
    svc2: B,
}

impl<A, B> ThenFactory<A, B> {
    /// Creates a new `ThenFactory`.
    pub(crate) fn new(svc1: A, svc2: B) -> Self {
        Self { svc1, svc2 }
    }
}

impl<A, B, St, Req> ServiceFactory<St, Req> for ThenFactory<A, B>
where
    A: ServiceFactory<St, Req>,
    B: ServiceFactory<St, Result<A::Res, A::Error>, Error = A::Error, InitError = A::InitError>,
{
    type Res = B::Res;
    type Error = A::Error;

    type Service = Then<A::Service, B::Service>;
    type InitError = A::InitError;

    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        Ok(Then::new(
            self.svc1.create(st).await?,
            self.svc2.create(st).await?,
        ))
    }
}

#[cfg(test)]
#[path = "../tests/unit/then.rs"]
mod tests;
