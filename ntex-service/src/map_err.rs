use std::{fmt, marker::PhantomData};

use super::{Ctx, Service, ServiceFactory};

/// Transforms call and readiness errors with a synchronous function.
///
/// Created by [`Service::map_err`] and [`crate::ServiceChain::map_err`].
/// The function receives an inner error and returns the new error value.
/// Successful responses are returned unchanged without invoking the function.
/// Request and state types are also unchanged.
///
/// This is useful for giving composed services a common error type. It does
/// not recover errors into responses; use [`crate::ServiceChain::then`] for
/// call-result recovery.
///
/// Binding and shutdown are forwarded to the inner service. Readiness is
/// forwarded with the same error conversion as calls. Mapping preserves the
/// inner service's readiness identity without adding an independent cache or
/// an extra readiness check at call time. An explicit [`crate::ready()`]
/// wrapper retains its own readiness behavior.
///
/// Cloning clones both the inner service and mapping function. Whether their
/// state is shared depends on their clone implementations.
///
/// # Example
///
/// ```
/// use ntex_service::{Service, fn_service};
///
/// # async fn example() {
/// let pipeline = fn_service(async |req: u32| {
///     if req == 0 { Err("zero is not allowed") } else { Ok(req) }
/// })
/// .map_err(|error| format!("request failed: {error}"))
/// .pipeline(());
///
/// assert_eq!(pipeline.call(1).await, Ok(1));
/// assert_eq!(
///     pipeline.call(0).await,
///     Err(String::from("request failed: zero is not allowed")),
/// );
/// # }
/// ```
pub struct MapErr<F, S, E> {
    f: F,
    svc: S,
    e: PhantomData<E>,
}

impl<F, S, E> MapErr<F, S, E> {
    /// Creates a new `MapErr` service.
    pub(crate) fn new<St, Req>(f: F, svc: S) -> Self
    where
        S: Service<St, Req>,
        F: Fn(S::Error) -> E,
    {
        Self {
            f,
            svc,
            e: PhantomData,
        }
    }
}

impl<F, S, E> Clone for MapErr<F, S, E>
where
    F: Clone,
    S: Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        MapErr {
            f: self.f.clone(),
            svc: self.svc.clone(),
            e: PhantomData,
        }
    }
}

impl<F, S, E> fmt::Debug for MapErr<F, S, E>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapErr")
            .field("svc", &self.svc)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, S, St, Req, E> Service<St, Req> for MapErr<F, S, E>
where
    S: Service<St, Req>,
    F: Fn(S::Error) -> E,
{
    type Res = S::Res;
    type Error = E;

    crate::forward_bind!(St, svc);

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, E> {
        self.svc
            .call(req, ctx.cast())
            .await
            .map_err(|e| (self.f)(e))
    }

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), E> {
        self.svc.ready(ctx.cast()).await.map_err(&self.f)
    }

    #[inline]
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        self.svc.shutdown(ctx.cast()).await;
    }
}

/// Maps call and readiness errors from each service created by an inner factory.
///
/// Created by [`ServiceFactory::map_err`] and
/// [`crate::ServiceChainFactory::map_err`]. Creation forwards the supplied state
/// to the inner factory. On success, the new service is wrapped in [`MapErr`]
/// with a clone of the mapping function.
///
/// Initialization errors are returned unchanged, without cloning or invoking
/// the function. Use [`ServiceFactory::map_init_err`] to transform those
/// errors. Request, response, and state types remain unchanged.
///
/// Created services use the lifecycle and readiness behavior described in
/// [`MapErr`]. Cloning clones the inner factory and mapping function without
/// creating a service.
///
/// # Example
///
/// ```
/// use ntex_service::{ServiceFactory, fn_factory, fn_service};
///
/// # async fn example() -> Result<(), ()> {
/// let factory = fn_factory(async |_: &()| {
///     Ok::<_, ()>(fn_service(async |_: u32| Err::<u32, _>("unavailable")))
/// })
/// .map_err(String::from);
///
/// let pipeline = factory.pipeline(()).await?;
/// assert_eq!(pipeline.call(1).await, Err(String::from("unavailable")));
/// # Ok(())
/// # }
/// ```
pub struct MapErrFactory<F, Sf, E> {
    f: F,
    sf: Sf,
    e: PhantomData<fn(Sf) -> E>,
}

impl<F, Sf, E> MapErrFactory<F, Sf, E> {
    /// Creates a new `MapErrFactory`.
    pub(crate) fn new<St, Req>(f: F, sf: Sf) -> Self
    where
        Sf: ServiceFactory<St, Req>,
        F: Fn(Sf::Error) -> E + Clone,
    {
        Self {
            f,
            sf,
            e: PhantomData,
        }
    }
}

impl<F: Clone, Sf: Clone, E> Clone for MapErrFactory<F, Sf, E> {
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            sf: self.sf.clone(),
            e: PhantomData,
        }
    }
}

impl<F, Sf, E> fmt::Debug for MapErrFactory<F, Sf, E>
where
    Sf: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapErrFactory")
            .field("sf", &self.sf)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, Sf, St, Req, E> ServiceFactory<St, Req> for MapErrFactory<F, Sf, E>
where
    Sf: ServiceFactory<St, Req>,
    F: Fn(Sf::Error) -> E + Clone,
{
    type Res = Sf::Res;
    type Error = E;

    type Service = MapErr<F, Sf::Service, E>;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        self.sf.create(st).await.map(|svc| MapErr {
            svc,
            f: self.f.clone(),
            e: PhantomData,
        })
    }
}

#[cfg(test)]
#[path = "../tests/unit/map_err.rs"]
mod tests;
