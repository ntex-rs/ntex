use std::{fmt, marker::PhantomData};

use super::{IntoServiceFactory, ServiceFactory};

/// Adapt external config argument to a config for provided service factory
///
/// Note that this function consumes the receiving service factory and returns
/// a wrapped version of it.
pub fn map_config<Sf, St, Req, Cfg, F, C>(
    sf: impl IntoServiceFactory<Sf, St, Req, Cfg>,
    f: F,
) -> MapConfig<Sf, F, C>
where
    Sf: ServiceFactory<St, Req, Cfg>,
    F: Fn(&C) -> Cfg,
{
    MapConfig::new(sf.into_factory(), f)
}

/// Replace config with unit
pub fn unit_config<Sf, St, Req, Cfg>(
    factory: impl IntoServiceFactory<Sf, St, Req, ()>,
) -> UnitConfig<Sf, Cfg>
where
    Sf: ServiceFactory<St, Req>,
{
    UnitConfig::new(factory.into_factory())
}

/// `map_config()` adapter service factory
pub struct MapConfig<Sf, F, Cfg> {
    sf: Sf,
    f: F,
    c: PhantomData<Cfg>,
}

impl<Sf, F, Cfg> MapConfig<Sf, F, Cfg> {
    /// Create new `MapConfig` combinator
    pub(crate) fn new(sf: Sf, f: F) -> Self {
        Self {
            sf,
            f,
            c: PhantomData,
        }
    }
}

impl<Sf, F, Cfg> Clone for MapConfig<Sf, F, Cfg>
where
    Sf: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            sf: self.sf.clone(),
            f: self.f.clone(),
            c: PhantomData,
        }
    }
}

impl<Sf, F, Cfg> fmt::Debug for MapConfig<Sf, F, Cfg>
where
    Sf: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapConfig")
            .field("factory", &self.sf)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<Sf, St, Req, Cfg, F, C> ServiceFactory<St, Req, C> for MapConfig<Sf, F, C>
where
    Sf: ServiceFactory<St, Req, Cfg>,
    F: Fn(&C) -> Cfg,
{
    type Res = Sf::Res;
    type Error = Sf::Error;

    type Service = Sf::Service;
    type InitError = Sf::InitError;

    async fn create(&self, cfg: &C) -> Result<Self::Service, Self::InitError> {
        self.sf.create(&(self.f)(cfg)).await
    }
}

#[derive(Clone, Debug)]
/// `unit_config()` config combinator
pub struct UnitConfig<Sf, Cfg> {
    sf: Sf,
    c: PhantomData<Cfg>,
}

impl<Sf, Cfg> UnitConfig<Sf, Cfg> {
    /// Create new `UnitConfig` combinator
    pub(crate) fn new(sf: Sf) -> Self {
        Self { sf, c: PhantomData }
    }
}

impl<Sf, St, Req, Cfg> ServiceFactory<St, Req, Cfg> for UnitConfig<Sf, Cfg>
where
    Sf: ServiceFactory<St, Req, ()>,
{
    type Res = Sf::Res;
    type Error = Sf::Error;

    type Service = Sf::Service;
    type InitError = Sf::InitError;

    async fn create(&self, _: &Cfg) -> Result<Self::Service, Self::InitError> {
        self.sf.create(&()).await
    }
}

#[cfg(test)]
#[allow(clippy::redundant_closure)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::{Pipeline, factory_no_st};

    #[ntex::test]
    async fn test_map_config() {
        let item = Rc::new(Cell::new(1usize));

        let factory = map_config(
            factory_no_st(async move |item: usize| Ok::<_, ()>(item)),
            |t: &usize| {
                item.set(item.get() + *t);
            },
        )
        .clone();

        let svc = Pipeline::with((), factory.create(&10).await.unwrap());
        assert_eq!(item.get(), 11);
        let _ = format!("{factory:?}");

        assert_eq!(svc.call(1).await.unwrap(), 1);
    }

    #[ntex::test]
    async fn test_unit_config() {
        let svc = Pipeline::with(
            (),
            unit_config(factory_no_st(async move |item: usize| Ok::<_, ()>(item)).clone())
                .create(&10)
                .await
                .unwrap(),
        );
        assert_eq!(svc.call(1).await.unwrap(), 1);
    }
}
