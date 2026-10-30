use std::{fmt, marker::PhantomData, rc::Rc};

use crate::dev::{Apply, ApplyCtx};
use crate::{IntoServiceFactory, Service, ServiceChainFactory, ServiceFactory};

/// Applies middleware to every service produced by a factory.
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
/// Middleware can run before and after the inner service, and can modify
/// requests, responses, or errors.
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
/// A middleware factory for `Timeout` could look like this:
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
    /// Service created by this middleware.
    type Service;

    /// Creates and returns a new middleware service.
    fn create(&self, st: &St, service: S) -> Self::Service;

    /// Creates a service factory that instantiates a service and applies
    /// the current middleware to it.
    ///
    /// This is equivalent to `apply(self, factory)`.
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
/// The inner middleware is applied first, then the outer middleware wraps its
/// service.
#[derive(Debug, Clone)]
pub struct Stack<Inner, Outer> {
    inner: Inner,
    outer: Outer,
}

impl<Inner, Outer> Stack<Inner, Outer> {
    /// Creates a middleware stack.
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
/// wrapped service.
pub fn fn_layer<F, S, St, Req, In, Out, Err>(f: F) -> FnMiddleware<F, S, St, Req, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, S, St, Req>) -> Result<Out, Err> + Clone,
    S: Service<St, Req>,
{
    FnMiddleware { f, r: PhantomData }
}

/// Middleware backed by an asynchronous function.
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
