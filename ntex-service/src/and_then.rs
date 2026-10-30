use super::{BindCtx, Ctx, Service, ServiceFactory, util};

#[derive(Debug, Clone)]
/// Calls two services in sequence, passing the first response to the second.
///
/// Created by [`Service::and_then`] and [`crate::ServiceChain::and_then`].
/// Both services use the same state and error types. The first service accepts
/// the input request, and the second service produces the final response.
/// If the first call fails, its error is returned without calling the second
/// service. Errors from the second call are also returned unchanged.
///
/// To pass both successful responses and errors to the next service, use
/// [`crate::ServiceChain::then`] instead.
///
/// # Readiness and lifecycle
///
/// Binding is forwarded to both children. Readiness succeeds only when both
/// children are ready; a readiness error from either child is returned
/// unchanged. Shutdown drives both children's shutdown futures to completion.
///
/// Children share the enclosing readiness scope unless explicitly wrapped
/// with [`crate::ready()`]. Calls forward directly to each child without
/// another readiness check for raw services. Wrap a child when it needs an
/// independent readiness cache and a check at call time, especially when the
/// second child's readiness can change while the first call is awaiting.
///
/// Cloning clones both children; any shared state or readiness depends on
/// their clone implementations.
///
/// # Example
///
/// ```
/// use ntex_service::{Service, fn_service};
///
/// # async fn example() -> Result<(), ()> {
/// let pipeline = fn_service(async |req: u32| Ok::<_, ()>(req + 1))
///     .and_then(fn_service(async |value: u32| Ok::<_, ()>(value.to_string())))
///     .pipeline(());
///
/// assert_eq!(pipeline.call(41).await?, "42");
/// # Ok(())
/// # }
/// ```
///
/// See the framework guide's [Service Pipelines](https://docs.ntex.rs/2-pipeline.html)
/// chapter for composition and readiness semantics.
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
/// Creates an [`AndThen`] service from two service factories.
///
/// Created by [`ServiceFactory::and_then`],
/// [`crate::ServiceChainFactory::and_then`], or [`Self::new`].
/// Both factories use the same state, service-error, and initialization-error
/// types. The first factory's response type is the second factory's request
/// type.
///
/// Creation is sequential: the first factory completes before the second
/// starts, and both receive the same state reference. If the first factory
/// fails, the second is not invoked. Initialization errors from either
/// factory are returned unchanged.
///
/// Created services use the call, binding, readiness, and shutdown behavior
/// described in [`AndThen`]. Wrap a child factory with [`crate::ready_factory()`]
/// to give each of its created services an independent readiness cache.
///
/// Cloning clones both factories without creating any services.
pub struct AndThenFactory<A, B> {
    svc1: A,
    svc2: B,
}

impl<A, B> AndThenFactory<A, B> {
    /// Combines two factories without creating their services yet.
    ///
    /// [`ServiceFactory::create`] creates the children in argument order and
    /// joins them into an [`AndThen`] service.
    ///
    /// # Example
    ///
    /// ```
    /// use ntex_service::{ServiceFactory, dev::AndThenFactory, fn_factory, fn_service};
    ///
    /// # async fn example() -> Result<(), ()> {
    /// let first = fn_factory(async |offset: &u32| {
    ///     let offset = *offset;
    ///     Ok::<_, ()>(fn_service(async move |req: u32| Ok::<_, ()>(req + offset)))
    /// });
    /// let second = fn_factory(async |_: &u32| {
    ///     Ok::<_, ()>(fn_service(async |value: u32| Ok::<_, ()>(value.to_string())))
    /// });
    ///
    /// let pipeline = AndThenFactory::new(first, second).pipeline(10).await?;
    /// assert_eq!(pipeline.call(2).await?, "12");
    /// # Ok(())
    /// # }
    /// ```
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
