use std::{fmt, marker::PhantomData};

use super::{IntoServiceFactory, ServiceFactory};

/// Adapt external config argument to a config for provided service factory
///
/// Note that this function consumes the receiving service factory and returns
/// a wrapped version of it.
pub fn map_config<T, S, U, F, C>(factory: U, f: F) -> MapConfig<T, F, C>
where
    T: ServiceFactory<S>,
    U: IntoServiceFactory<T, S>,
    F: Fn(&C) -> T::InitCfg,
{
    MapConfig::new(factory.into_factory(), f)
}

/// Replace config with unit
pub fn unit_config<T, C, S, U>(factory: U) -> UnitConfig<T, C>
where
    T: ServiceFactory<S>,
    U: IntoServiceFactory<T, S>,
{
    UnitConfig::new(factory.into_factory())
}

/// `map_config()` adapter service factory
pub struct MapConfig<A, F, C> {
    a: A,
    f: F,
    c: PhantomData<C>,
}

impl<A, F, C> MapConfig<A, F, C> {
    /// Create new `MapConfig` combinator
    pub(crate) fn new(a: A, f: F) -> Self {
        Self {
            a,
            f,
            c: PhantomData,
        }
    }
}

impl<A, F, C> Clone for MapConfig<A, F, C>
where
    A: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            f: self.f.clone(),
            c: PhantomData,
        }
    }
}

impl<A, F, C> fmt::Debug for MapConfig<A, F, C>
where
    A: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapConfig")
            .field("factory", &self.a)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<A, F, C, S> ServiceFactory<S> for MapConfig<A, F, C>
where
    A: ServiceFactory<S>,
    F: Fn(&C) -> A::InitCfg,
{
    type Req = A::Req;
    type Res = A::Res;
    type Error = A::Error;

    type Service = A::Service;
    type InitCfg = C;
    type InitError = A::InitError;

    async fn create(&self, cfg: &C) -> Result<Self::Service, Self::InitError> {
        self.a.create(&(self.f)(cfg)).await
    }
}

#[derive(Clone, Debug)]
/// `unit_config()` config combinator
pub struct UnitConfig<A, C> {
    factory: A,
    c: PhantomData<C>,
}

impl<A, C> UnitConfig<A, C> {
    /// Create new `UnitConfig` combinator
    pub(crate) fn new(factory: A) -> Self {
        Self {
            factory,
            c: PhantomData,
        }
    }
}

impl<A, C, S> ServiceFactory<S> for UnitConfig<A, C>
where
    A: ServiceFactory<S, InitCfg = ()>,
{
    type Req = A::Req;
    type Res = A::Res;
    type Error = A::Error;

    type Service = A::Service;
    type InitCfg = C;
    type InitError = A::InitError;

    async fn create(&self, _: &C) -> Result<Self::Service, Self::InitError> {
        self.factory.create(&()).await
    }
}

#[cfg(test)]
#[allow(clippy::redundant_closure)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::fn_service;

    #[ntex::test]
    async fn test_map_config() {
        let item = Rc::new(Cell::new(1usize));

        let factory = map_config(
            fn_service(|item: usize| async move { Ok::<_, ()>(item) }),
            |t: &usize| {
                item.set(item.get() + *t);
            },
        )
        .clone();

        let svc = factory.pipeline(&10).await.unwrap();
        assert_eq!(item.get(), 11);
        let _ = format!("{factory:?}");

        assert_eq!(svc.call(1, &()).await.unwrap(), 1);
    }

    #[ntex::test]
    async fn test_unit_config() {
        let svc = unit_config(fn_service(async move |item: usize| Ok::<_, ()>(item)))
            .clone()
            .pipeline(&10)
            .await
            .unwrap();
        assert_eq!(svc.call(1, &()).await.unwrap(), 1);
    }
}
