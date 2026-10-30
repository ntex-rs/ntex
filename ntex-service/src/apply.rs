use std::{fmt, marker};

use crate::{Ctx, IntoService, IntoServiceFactory, Service, ServiceFactory};
use crate::{ServiceCaller, ServiceChain, ServiceChainFactory};

/// Applies an asynchronous middleware function to a service.
///
/// The function receives an input request and an [`ApplyCtx`] with access to
/// the pipeline state and inner service. It can adapt the request, transform
/// the response or error, and decide whether and how often to call the inner
/// service. The state type stays the same; the request, response, and error
/// types can differ from those of the inner service.
///
/// Returns a [`ServiceChain`] containing an [`Apply`] service. Binding and
/// shutdown are forwarded to the inner service. Readiness is also forwarded,
/// with inner readiness errors converted into the middleware's error type
/// through [`From`]. Errors from [`ApplyCtx::call`] retain the inner error
/// type, so the middleware can handle or convert them.
///
/// # Readiness
///
/// The enclosing pipeline checks readiness before invoking the middleware.
/// [`ApplyCtx::call`] forwards directly to the inner service; it does not add
/// another readiness check for a raw service. Wrap the inner service with
/// [`crate::ready()`] when calls need their own readiness check, including
/// after the middleware awaits other work or makes an earlier call.
///
/// # Example
///
/// ```
/// use ntex_service::{Service, apply_fn, fn_service};
///
/// # async fn example() -> Result<(), ()> {
/// let service = apply_fn(
///     fn_service(async |req: u32| Ok::<_, ()>(req + 1)),
///     async |req: &str, ctx| {
///         let value = req.parse::<u32>().map_err(|_| ())?;
///         let response = ctx.call(value).await?;
///         Ok::<_, ()>(response.to_string())
///     },
/// );
/// let pipeline = service.pipeline(());
/// assert_eq!(pipeline.call("41").await?, "42");
/// # Ok(())
/// # }
/// ```
///
/// See the framework guide's [Service Pipelines](https://docs.ntex.rs/2-pipeline.html)
/// chapter for composition and readiness semantics.
pub fn apply_fn<S, St, Req, F, In, Out, Err>(
    service: impl IntoService<S, St, Req>,
    f: F,
) -> ServiceChain<Apply<S, St, Req, F, In, Out, Err>, St, In>
where
    S: Service<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, S, St, Req>) -> Result<Out, Err>,
    Err: From<S::Error>,
{
    crate::service(Apply::new(service.into_service(), f))
}

/// Applies an asynchronous middleware function to every service from a factory.
///
/// Returns a [`ServiceChainFactory`] containing an [`ApplyFactory`]. Each
/// successful creation wraps the new inner service with a clone of the
/// middleware function. The inner factory receives the same state and its
/// initialization errors are returned unchanged.
///
/// The resulting services use the call, binding, readiness, and shutdown
/// behavior described in [`apply_fn`]. To give each created inner service its
/// own readiness cache, wrap the inner factory with [`crate::ready_factory()`].
///
/// # Example
///
/// ```
/// use ntex_service::{ServiceFactory, apply_fn_factory, fn_factory, fn_service};
///
/// # async fn example() -> Result<(), ()> {
/// let factory = apply_fn_factory(
///     fn_factory(async |offset: &u32| {
///         let offset = *offset;
///         Ok::<_, ()>(fn_service(async move |req: u32| Ok::<_, ()>(req + offset)))
///     }),
///     async |req: u8, ctx| ctx.call(u32::from(req)).await,
/// );
/// let pipeline = factory.pipeline(10).await?;
/// assert_eq!(pipeline.call(2).await?, 12);
/// # Ok(())
/// # }
/// ```
pub fn apply_fn_factory<Sf, St, Req, F, In, Out, Err>(
    service: impl IntoServiceFactory<Sf, St, Req>,
    f: F,
) -> ServiceChainFactory<ApplyFactory<F, Sf, St, Req, In, Out, Err>, St, In>
where
    Sf: ServiceFactory<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St, Req>) -> Result<Out, Err> + Clone,
    Err: From<Sf::Error>,
{
    crate::factory(ApplyFactory::new(service.into_factory(), f))
}

#[derive(Debug)]
/// State and inner-service access for [`apply_fn`] and [`apply_fn_factory`] middleware.
///
/// [`Self::st`] borrows the current pipeline state, and [`Self::call`] invokes
/// the inner service with that state. The context also implements
/// [`ServiceCaller`], with the same call behavior.
///
/// Calls forward directly to the inner service. Wrap it with [`crate::ready()`]
/// to check its independent readiness on each call, including after middleware
/// awaits or makes another call. The context borrows both the state and the
/// inner service for the middleware invocation.
pub struct ApplyCtx<'a, S, St, Req> {
    ctx: Ctx<'a, S, St>,
    service: &'a S,
    r: marker::PhantomData<Req>,
}

impl<S: Service<St, Req>, St, Req> ApplyCtx<'_, S, St, Req> {
    /// Returns the pipeline state.
    ///
    /// Inner calls made through this context receive the same state reference.
    #[inline]
    pub fn st(&self) -> &St {
        self.ctx.st()
    }

    /// Calls the inner service, preserving any explicit readiness wrapper.
    ///
    /// This does not independently check readiness for a raw inner service.
    /// An inner [`crate::ready()`] wrapper still checks its own readiness
    /// cache, even when the outer middleware was invoked through a legacy
    /// readiness bypass.
    ///
    /// Returns the inner response and error types without applying the outer
    /// middleware's conversions.
    #[inline]
    pub async fn call(&self, req: Req) -> Result<S::Res, S::Error> {
        self.service
            .call(req, self.ctx.without_bypass().cast())
            .await
    }
}

impl<S: Service<St, Req>, St, Req> ServiceCaller<Req, S::Res, S::Error>
    for ApplyCtx<'_, S, St, Req>
{
    #[inline]
    async fn call_service(&self, req: Req) -> Result<S::Res, S::Error> {
        self.call(req).await
    }
}

/// Middleware service constructed by [`apply_fn`] or [`ApplyFactory`].
///
/// Calls invoke the middleware function with an [`ApplyCtx`]. Binding and
/// shutdown are forwarded to the inner service, and readiness errors are
/// converted into the middleware's error type.
///
/// Shares the enclosing readiness scope with its child unless the child has
/// its own readiness cache, for example through [`crate::ready()`].
///
/// Cloning clones both the inner service and middleware function. Whether
/// those clones share state or readiness depends on the inner service's
/// clone behavior.
pub struct Apply<S, St, Req, F, In, Out, Err> {
    svc: S,
    f: F,
    r: marker::PhantomData<fn(St, Req) -> (In, Out, Err)>,
}

impl<S, St, Req, F, In, Out, Err> Apply<S, St, Req, F, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, S, St, Req>) -> Result<Out, Err>,
{
    pub(crate) fn new(svc: S, f: F) -> Self {
        Apply {
            f,
            svc,
            r: marker::PhantomData,
        }
    }
}

impl<S, St, Req, F, In, Out, Err> Clone for Apply<S, St, Req, F, In, Out, Err>
where
    S: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Apply {
            svc: self.svc.clone(),
            f: self.f.clone(),
            r: marker::PhantomData,
        }
    }
}

impl<S, St, Req, F, In, Out, Err> fmt::Debug for Apply<S, St, Req, F, In, Out, Err>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Apply")
            .field("svc", &self.svc)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<S, St, Req, F, In, Out, Err> Service<St, In> for Apply<S, St, Req, F, In, Out, Err>
where
    S: Service<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, S, St, Req>) -> Result<Out, Err>,
    Err: From<S::Error>,
{
    type Res = Out;
    type Error = Err;

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Err> {
        ctx.ready(&self.svc).await.map_err(From::from)
    }

    #[inline]
    async fn call(&self, req: In, ctx: Ctx<'_, Self, St>) -> Result<Out, Err> {
        let ctx = ApplyCtx {
            ctx: ctx.cast(),
            service: &self.svc,
            r: marker::PhantomData,
        };
        (self.f)(req, &ctx).await
    }

    crate::forward_bind!(St, self.svc);
    crate::forward_shutdown!(St, self.svc);
}

/// Service factory produced by [`apply_fn_factory`].
///
/// Implements [`ServiceFactory`] by creating an inner service with the supplied
/// state, then wrapping it in [`Apply`] with a clone of the middleware function.
/// Initialization errors are those of the inner factory and are not converted
/// into the middleware's call error type.
///
/// Cloning clones both the inner factory and middleware function; it does not
/// create a service.
pub struct ApplyFactory<F, Sf, St, Req, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St, Req>) -> Result<Out, Err> + Clone,
    Sf: ServiceFactory<St, Req>,
{
    f: F,
    sf: Sf,
    r: marker::PhantomData<fn(St, Req) -> (In, Out)>,
}

impl<F, Sf, St, Req, In, Out, Err> ApplyFactory<F, Sf, St, Req, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St, Req>) -> Result<Out, Err> + Clone,
    Sf: ServiceFactory<St, Req>,
{
    /// Creates a new `ApplyFactory`.
    pub(crate) fn new(sf: Sf, f: F) -> Self
    where
        Sf: ServiceFactory<St, Req>,
        Err: From<Sf::Error>,
    {
        Self {
            f,
            sf,
            r: marker::PhantomData,
        }
    }
}

impl<F, Sf, St, Req, In, Out, Err> Clone for ApplyFactory<F, Sf, St, Req, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St, Req>) -> Result<Out, Err> + Clone,
    Sf: ServiceFactory<St, Req> + Clone,
{
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            sf: self.sf.clone(),
            r: marker::PhantomData,
        }
    }
}

impl<F, Sf, St, Req, In, Out, Err> fmt::Debug for ApplyFactory<F, Sf, St, Req, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St, Req>) -> Result<Out, Err> + Clone,
    Sf: ServiceFactory<St, Req> + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplyFactory")
            .field("factory", &self.sf)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, Sf, St, Req, In, Out, Err> ServiceFactory<St, In>
    for ApplyFactory<F, Sf, St, Req, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St, Req>) -> Result<Out, Err> + Clone,
    Sf: ServiceFactory<St, Req>,
    Err: From<Sf::Error>,
{
    type Res = Out;
    type Error = Err;

    type Service = Apply<Sf::Service, St, Req, F, In, Out, Err>;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        self.sf
            .create(st)
            .await
            .map(|svc| Apply::new(svc, self.f.clone()))
    }
}

#[cfg(test)]
#[path = "../tests/unit/apply.rs"]
mod tests;
