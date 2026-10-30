use super::{Ctx, Service, ServiceFactory, util};

#[derive(Debug, Clone)]
/// Passes a service's complete call result to a second service.
///
/// Created by [`crate::ServiceChain::then`]. The second service receives
/// `Result<A::Res, A::Error>` and can transform a successful response, recover
/// from an error, or return an error of its own. Its result becomes the final
/// result. Both services use the same state and error types.
///
/// Unlike [`crate::dev::AndThen`], this invokes the second service even when
/// the first call fails.
///
/// # Readiness and lifecycle
///
/// Binding is forwarded to both children. Readiness succeeds only when both
/// children are ready. Errors from the chain's readiness check are returned
/// directly, not passed to the second service as a request. Shutdown drives
/// both children's shutdown futures to completion.
///
/// Raw children share the enclosing readiness scope, and calls forward
/// directly without an extra readiness check. Wrap a child with
/// [`crate::ready()`] when it needs an independent cache and a call-time check,
/// particularly when the second child's readiness can change while the first
/// call is awaiting.
///
/// Cloning clones both children; shared state and readiness depend on their
/// clone implementations.
///
/// # Example
///
/// ```
/// use ntex_service::{Service, fn_service, service};
///
/// # async fn example() -> Result<(), &'static str> {
/// let pipeline = service(fn_service(async |req: u32| {
///     if req == 0 { Err("missing value") } else { Ok(req) }
/// }))
/// .then(fn_service(async |result: Result<u32, &'static str>| {
///     Ok::<_, &'static str>(result.unwrap_or(10) * 2)
/// }))
/// .pipeline(());
///
/// assert_eq!(pipeline.call(3).await?, 6);
/// assert_eq!(pipeline.call(0).await?, 20);
/// # Ok(())
/// # }
/// ```
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
/// Creates a [`Then`] service from two service factories.
///
/// Created by [`crate::ServiceChainFactory::then`]. Both factories use the
/// same state, service-error, and initialization-error types. The second
/// factory creates services whose request is the first service's complete
/// `Result`, rather than just its successful response.
///
/// Creation is sequential with the same state reference. The second factory
/// is invoked only after the first succeeds. An initialization error from
/// either factory is returned unchanged; it is not passed to the second
/// factory or handled as a service-call result.
///
/// Created services use the call and lifecycle behavior described in [`Then`].
/// Wrap a child factory with [`crate::ready_factory()`] to give each of its
/// created services an independent readiness cache. Cloning clones both
/// factories without creating services.
///
/// # Example
///
/// ```
/// use ntex_service::{factory, fn_factory, fn_service};
///
/// # async fn example() -> Result<(), &'static str> {
/// let factory = factory(fn_factory(async |_: &()| {
///     Ok::<_, &'static str>(fn_service(async |_: u32| Err::<u32, _>("unavailable")))
/// }))
/// .then(fn_factory(async |_: &()| {
///     Ok::<_, &'static str>(fn_service(async |result: Result<u32, &'static str>| {
///         Ok::<_, &'static str>(result.unwrap_or(42))
///     }))
/// }));
///
/// let pipeline = factory.pipeline(()).await?;
/// assert_eq!(pipeline.call(1).await?, 42);
/// # Ok(())
/// # }
/// ```
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
