use std::{fmt, marker::PhantomData, rc::Rc};

use crate::dev::{Apply, ApplyCtx};
use crate::{IntoServiceFactory, Service, ServiceChainFactory, ServiceFactory};

/// Applies middleware to every service produced by a factory.
///
/// Returns a [`ServiceChainFactory`] containing an [`ApplyMiddleware`].
/// Creation first awaits the inner factory, then calls [`Middleware::create`]
/// synchronously with the new service and the same state reference. If the
/// factory fails, its initialization error is returned without invoking the
/// middleware.
///
/// To use the result as a [`ServiceFactory`], the middleware's service must
/// accept the same state and request types as the inner service. Response and
/// service-error types may change. The wrapper is responsible for forwarding
/// calls, binding, readiness, and shutdown as appropriate.
///
/// Cloning the resulting factory shares the middleware and inner factory
/// through [`Rc`]; neither needs to implement [`Clone`].
///
/// # Example
///
/// ```
/// use ntex_service::{Identity, apply, fn_factory, fn_service};
///
/// # async fn example() -> Result<(), ()> {
/// let factory = apply(
///     Identity,
///     fn_factory(async |offset: &u32| {
///         let offset = *offset;
///         Ok::<_, ()>(fn_service(async move |req: u32| Ok::<_, ()>(req + offset)))
///     }),
/// );
///
/// let pipeline = factory.pipeline(10).await?;
/// assert_eq!(pipeline.call(2).await?, 12);
/// # Ok(())
/// # }
/// ```
pub fn apply<Sf, St, Req, M>(
    mw: M,
    factory: impl IntoServiceFactory<Sf, St, Req>,
) -> ServiceChainFactory<ApplyMiddleware<M, Sf>, St, Req>
where
    Sf: ServiceFactory<St, Req>,
    M: Middleware<Sf::Service, St>,
{
    ServiceChainFactory {
        factory: ApplyMiddleware::new(mw, factory.into_factory()),
        _t: PhantomData,
    }
}

/// Wraps an inner service during service construction.
///
/// [`Self::create`] takes ownership of an already-created service and returns
/// a wrapper synchronously and infallibly. The wrapper can run work before
/// and after inner calls, transform requests, responses, or errors, or avoid
/// calling the inner service.
///
/// Use [`apply()`], [`Self::apply_to`], or [`ServiceChainFactory::apply`] to
/// install middleware on a factory. These require the wrapper to implement
/// [`Service`] for the factory's state and request types. Direct use of
/// [`Self::create`] can also construct wrappers with a different request type.
///
/// The state reference is available during construction. A service wrapper
/// can access the current state on each call through [`crate::Ctx::st`].
/// [`Rc`] middleware shares one middleware value across its clones, and
/// [`Stack`] composes multiple wrappers.
///
/// # Lifecycle forwarding
///
/// Middleware must arrange the resulting service's lifecycle behavior.
/// Wrappers retaining an inner service should forward [`Service::bind`] so
/// its readiness notifications reach the enclosing pipeline, and forward or
/// deliberately replace its readiness and shutdown behavior. These methods
/// are not forwarded automatically by implementing this trait.
///
/// # Example
///
/// For example, timeout middleware:
///
/// ```rust
/// use ntex_service::{Ctx, Service};
/// use ntex::{time::sleep, util::Either, util::select};
///
/// pub struct Timeout<S> {
///     service: S,
///     timeout: std::time::Duration,
/// }
///
/// pub enum TimeoutError<E> {
///    Service(E),
///    Timeout,
/// }
///
/// impl<S, R> Service<(), R> for Timeout<S>
/// where
///     S: Service<(), R>,
/// {
///     type Res = S::Res;
///     type Error = TimeoutError<S::Error>;
///
///     ntex_service::forward_bind!((), service);
///     ntex_service::forward_shutdown!((), service);
///
///     async fn ready(&self, ctx: Ctx<'_, Self>) -> Result<(), Self::Error> {
///         ctx.ready(&self.service).await.map_err(TimeoutError::Service)
///     }
///
///     async fn call(&self, req: R, ctx: Ctx<'_, Self>) -> Result<Self::Res, Self::Error> {
///         match select(sleep(self.timeout), ctx.call(&self.service, req)).await {
///             Either::Left(_) => Err(TimeoutError::Timeout),
///             Either::Right(res) => res.map_err(TimeoutError::Service),
///         }
///     }
/// }
/// ```
///
/// The timeout service is independent of the wrapped service implementation and
/// can be applied to any compatible service.
///
/// Middleware that constructs `Timeout` could look like this:
///
/// ```rust
/// # use ntex_service::Middleware;
/// # pub struct Timeout<S> {
/// #     service: S,
/// #     timeout: std::time::Duration,
/// # }
/// pub struct TimeoutMiddleware {
///     timeout: std::time::Duration,
/// }
///
/// impl<S> Middleware<S, ()> for TimeoutMiddleware
/// {
///     type Service = Timeout<S>;
///
///     fn create(&self, _: &(), service: S) -> Self::Service {
///         Timeout {
///             service,
///             timeout: self.timeout,
///         }
///     }
/// }
/// ```
pub trait Middleware<S, St> {
    /// Wrapper returned by [`Self::create`].
    ///
    /// Factory application requires this type to implement [`Service`] for
    /// the factory's state and request types. Its response and error types
    /// may differ from those of the inner service.
    type Service;

    /// Wraps `service` using the supplied construction state.
    ///
    /// This is synchronous and infallible. It does not bind or check readiness
    /// of the returned service; those steps belong to the enclosing pipeline.
    fn create(&self, st: &St, service: S) -> Self::Service;

    /// Creates a service factory that instantiates a service and applies
    /// the current middleware to it.
    ///
    /// This is equivalent to [`apply()`] with this middleware and `factory`.
    /// The factory is awaited before invoking [`Self::create`] with the same
    /// state reference. Initialization errors bypass the middleware.
    ///
    /// The middleware and factory are shared when the returned builder is
    /// cloned; they do not need to implement [`Clone`].
    fn apply_to<Sf, Req>(
        self,
        factory: Sf,
    ) -> ServiceChainFactory<ApplyMiddleware<Self, Sf>, St, Req>
    where
        Sf: ServiceFactory<St, Req, Service = S>,
        Self: Sized,
        Self::Service: Service<St, Req>,
    {
        crate::factory(ApplyMiddleware::new(self, factory))
    }
}

impl<M, S, St> Middleware<S, St> for Rc<M>
where
    M: Middleware<S, St>,
{
    type Service = M::Service;

    fn create(&self, st: &St, service: S) -> M::Service {
        self.as_ref().create(st, service)
    }
}

/// A service factory with middleware applied.
///
/// Created by [`apply()`], [`Middleware::apply_to`], or
/// [`ServiceChainFactory::apply`]. Each creation first awaits the inner
/// factory, then wraps its service through [`Middleware::create`] with the
/// same state reference.
///
/// The wrapper determines response and service-error types and is responsible
/// for lifecycle forwarding. Inner initialization errors are returned
/// unchanged without invoking the middleware.
///
/// Cloning shares both middleware and factory through [`Rc`] rather than
/// cloning them or creating a service.
pub struct ApplyMiddleware<M, Sf>(Rc<(M, Sf)>);

impl<M, Sf> ApplyMiddleware<M, Sf> {
    /// Creates a new `ApplyMiddleware` factory.
    pub(crate) fn new(mw: M, sf: Sf) -> Self {
        Self(Rc::new((mw, sf)))
    }
}

impl<M, Sf> Clone for ApplyMiddleware<M, Sf> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<M, Sf> fmt::Debug for ApplyMiddleware<M, Sf>
where
    M: fmt::Debug,
    Sf: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplyMiddleware")
            .field("factory", &self.0.1)
            .field("middleware", &self.0.0)
            .finish()
    }
}

impl<M, Sf, St, Req> ServiceFactory<St, Req> for ApplyMiddleware<M, Sf>
where
    Sf: ServiceFactory<St, Req>,
    M: Middleware<Sf::Service, St>,
    M::Service: Service<St, Req>,
{
    type Res = <M::Service as Service<St, Req>>::Res;
    type Error = <M::Service as Service<St, Req>>::Error;

    type Service = M::Service;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        Ok(self.0.0.create(st, self.0.1.create(st).await?))
    }
}

/// Middleware that returns the wrapped service unchanged.
///
/// Construction ignores state and returns the exact input service without
/// adding a wrapper, readiness cache, or lifecycle behavior. This is useful as
/// a default middleware or a no-op component of a [`Stack`].
#[derive(Debug, Clone, Copy)]
pub struct Identity;

impl<S, St> Middleware<S, St> for Identity {
    type Service = S;

    #[inline]
    fn create(&self, _: &St, service: S) -> Self::Service {
        service
    }
}

/// Two middleware values applied in sequence.
///
/// Construction applies `inner` first, then `outer`, with the same state
/// reference: the result is `outer(inner(service))`. Requests enter the outer
/// wrapper first; if forwarded, they then pass through the inner wrapper to
/// the service. Responses return through the inner wrapper before the outer.
///
/// The stack only composes construction. Lifecycle forwarding and whether
/// calls reach the inner service are determined by each wrapper. Cloning
/// clones both middleware values without constructing a service.
///
/// # Example
///
/// ```
/// use ntex_service::{Middleware, Service, Stack, fn_layer, fn_service};
///
/// # async fn example() -> Result<(), ()> {
/// let inner = fn_layer(async |req: u32, ctx| ctx.call(req + 1).await);
/// let outer = fn_layer(async |req: u32, ctx| ctx.call(req * 2).await);
/// let service = Stack::new(inner, outer)
///     .create(&(), fn_service(async |req: u32| Ok::<_, ()>(req)));
///
/// let pipeline = service.pipeline(());
/// assert_eq!(pipeline.call(2).await?, 5);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Stack<Inner, Outer> {
    inner: Inner,
    outer: Outer,
}

impl<Inner, Outer> Stack<Inner, Outer> {
    /// Stores two middleware values in inner-to-outer construction order.
    ///
    /// No service is created until [`Middleware::create`] is called. The
    /// resulting outer wrapper receives requests before the inner wrapper.
    pub fn new(inner: Inner, outer: Outer) -> Self {
        Stack { inner, outer }
    }
}

impl<S, St, Inner, Outer> Middleware<S, St> for Stack<Inner, Outer>
where
    Inner: Middleware<S, St>,
    Outer: Middleware<Inner::Service, St>,
{
    type Service = Outer::Service;

    fn create(&self, st: &St, service: S) -> Self::Service {
        self.outer.create(st, self.inner.create(st, service))
    }
}

#[doc(hidden)]
/// Creates middleware from an asynchronous function.
///
/// The function receives an input request and an [`ApplyCtx`] that can call the
/// wrapped service or access pipeline state. Each [`Middleware::create`] call
/// clones the function into an [`Apply`] wrapper; it does not invoke it.
///
/// Request, response, and error types may change, while the state type stays
/// the same. The middleware implementation requires `Err: From<S::Error>` to
/// convert inner readiness errors. See [`crate::apply_fn`] for lifecycle and
/// call-time readiness semantics.
pub fn fn_layer<F, S, St, Req, In, Out, Err>(f: F) -> FnMiddleware<F, S, St, Req, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, S, St, Req>) -> Result<Out, Err> + Clone,
    S: Service<St, Req>,
{
    FnMiddleware { f, r: PhantomData }
}

/// Middleware backed by an asynchronous function.
///
/// Created by [`fn_layer`]. Each [`Middleware::create`] call wraps the supplied
/// service in [`Apply`] with a clone of the function. The construction state
/// is not captured automatically; the function accesses current pipeline state
/// through [`ApplyCtx::st`].
///
/// The function controls whether and how often the inner service is called.
/// Binding, readiness, and shutdown follow [`Apply`], including conversion of
/// inner readiness errors through [`From`]. Cloning this middleware clones only
/// the function, not an inner service.
pub struct FnMiddleware<F, S, St, Req, In, Out, Err> {
    f: F,
    r: PhantomData<fn(S, St, Req) -> (In, Out, Err)>,
}

impl<F, S, St, Req, In, Out, Err> Clone for FnMiddleware<F, S, St, Req, In, Out, Err>
where
    F: Clone,
{
    fn clone(&self) -> Self {
        FnMiddleware {
            f: self.f.clone(),
            r: PhantomData,
        }
    }
}

impl<F, S, St, Req, In, Out, Err> fmt::Debug for FnMiddleware<F, S, St, Req, In, Out, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnMiddleware")
            .field("layer", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, S, St, Req, In, Out, Err> Middleware<S, St> for FnMiddleware<F, S, St, Req, In, Out, Err>
where
    S: Service<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, S, St, Req>) -> Result<Out, Err> + Clone,
    Err: From<S::Error>,
{
    type Service = Apply<S, St, Req, F, In, Out, Err>;

    fn create(&self, _: &St, service: S) -> Self::Service {
        Apply::new(service, self.f.clone())
    }
}

#[cfg(test)]
#[allow(clippy::redundant_clone)]
#[path = "../tests/unit/middleware.rs"]
mod tests;
