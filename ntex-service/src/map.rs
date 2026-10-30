use std::{fmt, marker::PhantomData};

use super::{Ctx, Service, ServiceFactory};

/// Transforms successful service responses with a synchronous function.
///
/// Created by [`Service::map`] and [`crate::ServiceChain::map`]. Each successful
/// inner call passes its response to the mapping function, whose return value
/// becomes the new response. On failure, the error is returned unchanged and
/// the mapping function is not invoked.
///
/// The request, state, and error types are unchanged. For asynchronous response
/// processing, use [`Service::and_then`] or [`crate::apply_fn`] instead.
///
/// Binding, readiness, and shutdown are forwarded to the inner service.
/// Mapping preserves the inner service's readiness identity without adding an
/// independent cache or an extra readiness check at call time. An explicit
/// [`crate::ready()`] wrapper on the inner service retains its own readiness
/// behavior.
///
/// Cloning clones both the inner service and mapping function. Whether their
/// state is shared depends on their clone implementations.
///
/// # Example
///
/// ```
/// use ntex_service::{Service, fn_service};
///
/// # async fn example() -> Result<(), ()> {
/// let pipeline = fn_service(async |req: u32| Ok::<_, ()>(req + 1))
///     .map(|value| value.to_string())
///     .pipeline(());
///
/// assert_eq!(pipeline.call(41).await?, "42");
/// # Ok(())
/// # }
/// ```
pub struct Map<F, S, Res> {
    f: F,
    svc: S,
    _t: PhantomData<fn() -> Res>,
}

impl<F, S, Res> Map<F, S, Res> {
    /// Creates a new `Map` service.
    pub(crate) fn new<St, Req>(f: F, svc: S) -> Self
    where
        F: Fn(S::Res) -> Res,
        S: Service<St, Req>,
    {
        Self {
            f,
            svc,
            _t: PhantomData,
        }
    }
}

impl<F, S, Res> Clone for Map<F, S, Res>
where
    F: Clone,
    S: Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        Map {
            f: self.f.clone(),
            svc: self.svc.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, S, Res> fmt::Debug for Map<F, S, Res>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Map")
            .field("svc", &self.svc)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, S, St, Req, Res> Service<St, Req> for Map<F, S, Res>
where
    S: Service<St, Req>,
    F: Fn(S::Res) -> Res,
{
    type Res = Res;
    type Error = S::Error;

    crate::forward_bind!(St, svc);

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Res, S::Error> {
        self.svc.call(req, ctx.cast()).await.map(|r| (self.f)(r))
    }

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), S::Error> {
        self.svc.ready(ctx.cast()).await
    }

    #[inline]
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        self.svc.shutdown(ctx.cast()).await;
    }
}

/// Maps successful responses from each service created by an inner factory.
///
/// Created by [`ServiceFactory::map`] and [`crate::ServiceChainFactory::map`].
/// Creation forwards the supplied state to the inner factory. On success,
/// the new service is wrapped in [`Map`] with a clone of the mapping function.
/// The function runs when that service produces a successful response, not
/// during initialization.
///
/// The request, state, service-error, and initialization-error types are
/// unchanged. Inner initialization errors are returned without cloning or
/// invoking the mapping function. Created services retain the binding,
/// readiness, and shutdown behavior described in [`Map`].
///
/// Cloning clones the inner factory and mapping function without creating a
/// service.
///
/// # Example
///
/// ```
/// use ntex_service::{ServiceFactory, fn_factory, fn_service};
///
/// # async fn example() -> Result<(), ()> {
/// let factory = fn_factory(async |offset: &u32| {
///     let offset = *offset;
///     Ok::<_, ()>(fn_service(async move |req: u32| Ok::<_, ()>(req + offset)))
/// })
/// .map(|value| value.to_string());
///
/// let pipeline = factory.pipeline(10).await?;
/// assert_eq!(pipeline.call(2).await?, "12");
/// # Ok(())
/// # }
/// ```
pub struct MapFactory<F, Sf, Res> {
    f: F,
    sf: Sf,
    r: PhantomData<fn() -> Res>,
}

impl<F, Sf, Res> MapFactory<F, Sf, Res> {
    /// Creates a new `MapFactory`.
    pub(crate) fn new<St, Req>(f: F, sf: Sf) -> Self
    where
        F: Fn(Sf::Res) -> Res,
        Sf: ServiceFactory<St, Req>,
    {
        Self {
            f,
            sf,
            r: PhantomData,
        }
    }
}

impl<F, Sf, Res> Clone for MapFactory<F, Sf, Res>
where
    F: Clone,
    Sf: Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        Self {
            sf: self.sf.clone(),
            f: self.f.clone(),
            r: PhantomData,
        }
    }
}

impl<F, Sf, Res> fmt::Debug for MapFactory<F, Sf, Res>
where
    Sf: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapFactory")
            .field("factory", &self.sf)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, Sf, St, Req, Res> ServiceFactory<St, Req> for MapFactory<F, Sf, Res>
where
    F: Fn(Sf::Res) -> Res + Clone,
    Sf: ServiceFactory<St, Req>,
{
    type Res = Res;
    type Error = Sf::Error;

    type Service = Map<F, Sf::Service, Res>;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        Ok(Map {
            svc: self.sf.create(st).await?,
            f: self.f.clone(),
            _t: PhantomData,
        })
    }
}

#[cfg(test)]
#[path = "../tests/unit/map.rs"]
mod tests;
