use std::{fmt, marker::PhantomData};

use super::{Ctx, Service, ServiceFactory};

/// Service produced by the `map_err` combinator.
///
/// This is created by the `Service::map_err()` and `ServiceChain::map_err()` methods.
/// Mapping errors preserves the wrapped service's readiness identity.
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

/// Service factory produced by the `map_err` combinator.
///
/// This is created by the `ServiceChainFactory::map_err()` method.
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
