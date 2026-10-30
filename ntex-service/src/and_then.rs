use super::{BindCtx, Ctx, Service, ServiceFactory, util};

#[derive(Debug, Clone)]
/// Service produced by the `and_then` combinator.
///
/// This is created by the `Service::and_then()` and `ServiceChain::and_then()` methods.
/// Children share the enclosing readiness scope unless explicitly wrapped with
/// [`crate::ready`]. Calls forward directly to each child; a wrapped child checks
/// its own readiness at call time, including after the first child's call awaits.
pub struct AndThen<A, B> {
    svc1: A,
    svc2: B,
}

impl<A, B> AndThen<A, B> {
    /// Creates a new `AndThen` service.
    pub(crate) fn new(svc1: A, svc2: B) -> Self {
        Self { svc1, svc2 }
    }
}

impl<A, B, St, Req> Service<St, Req> for AndThen<A, B>
where
    A: Service<St, Req>,
    B: Service<St, A::Res, Error = A::Error>,
{
    type Res = B::Res;
    type Error = A::Error;

    #[inline]
    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.bind::<_, St, Req>(&self.svc1);
        ctx.bind::<_, St, A::Res>(&self.svc2);
    }

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<B::Res, A::Error> {
        let result = self.svc1.call(req, ctx.cast()).await?;
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
/// Service factory produced by the `and_then` combinator.
///
/// This is created by the `ServiceChainFactory::and_then()` method.
pub struct AndThenFactory<A, B> {
    svc1: A,
    svc2: B,
}

impl<A, B> AndThenFactory<A, B> {
    /// Creates a new `AndThenFactory`.
    pub fn new(svc1: A, svc2: B) -> Self {
        Self { svc1, svc2 }
    }
}

impl<A, B, St, Req> ServiceFactory<St, Req> for AndThenFactory<A, B>
where
    A: ServiceFactory<St, Req>,
    B: ServiceFactory<St, A::Res, Error = A::Error, InitError = A::InitError>,
{
    type Res = B::Res;
    type Error = A::Error;

    type Service = AndThen<A::Service, B::Service>;
    type InitError = A::InitError;

    #[inline]
    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        Ok(AndThen::new(
            self.svc1.create(st).await?,
            self.svc2.create(st).await?,
        ))
    }
}

#[cfg(test)]
#[path = "../tests/unit/and_then.rs"]
mod tests;
