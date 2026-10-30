use std::{fmt, marker::PhantomData};

use super::{Ctx, Service, ServiceFactory};

/// Service produced by the `map` combinator.
///
/// This is created by the `Service::map()` and `ServiceChain::map()` methods.
/// Mapping responses preserves the wrapped service's readiness identity.
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

/// Service factory produced by the `map` combinator.
///
/// This is created by the `ServiceChainFactory::map()` method.
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
