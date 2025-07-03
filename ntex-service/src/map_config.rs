use std::{fmt, marker::PhantomData};

use super::{IntoServiceFactory, ServiceFactory};

/// Adapt external config argument to a config for provided service factory
///
/// Note that this function consumes the receiving service factory and returns
/// a wrapped version of it.
pub fn map_config<T, R, U, F, C, C2>(factory: U, f: F) -> MapConfig<T, F, C, C2>
where
    T: ServiceFactory<R, C2>,
    U: IntoServiceFactory<T, R, C2>,
    F: Fn(C) -> C2,
{
    MapConfig::new(factory.into_factory(), f)
}

/// Replace config with unit
pub fn unit_config<T, R, U>(factory: U) -> UnitConfig<T>
where
    T: ServiceFactory<R>,
    U: IntoServiceFactory<T, R>,
{
    UnitConfig::new(factory.into_factory())
}

/// `map_config()` adapter service factory
pub struct MapConfig<A, F, C, C2> {
    a: A,
    f: F,
    e: PhantomData<(C, C2)>,
}

impl<A, F, C, C2> MapConfig<A, F, C, C2> {
    /// Create new `MapConfig` combinator
    pub(crate) fn new(a: A, f: F) -> Self {
        Self {
            a,
            f,
            e: PhantomData,
        }
    }
}

impl<A, F, C, C2> Clone for MapConfig<A, F, C, C2>
where
    A: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            f: self.f.clone(),
            e: PhantomData,
        }
    }
}

impl<A, F, C, C2> fmt::Debug for MapConfig<A, F, C, C2>
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

impl<A, F, R, C, C2> ServiceFactory<R, C> for MapConfig<A, F, C, C2>
where
    A: ServiceFactory<R, C2>,
    F: Fn(C) -> C2,
{
    type Response = A::Response;
    type Error = A::Error;

    type Service = A::Service;
    type InitError = A::InitError;

    async fn create(&self, cfg: C) -> Result<Self::Service, Self::InitError> {
        self.a.create((self.f)(cfg)).await
    }
}

#[derive(Clone, Debug)]
/// `unit_config()` config combinator
pub struct UnitConfig<A> {
    factory: A,
}

impl<A> UnitConfig<A> {
    /// Create new `UnitConfig` combinator
    pub(crate) fn new(factory: A) -> Self {
        Self { factory }
    }
}

impl<A, R, C> ServiceFactory<R, C> for UnitConfig<A>
where
    A: ServiceFactory<R>,
{
    type Response = A::Response;
    type Error = A::Error;

    type Service = A::Service;
    type InitError = A::InitError;

    async fn create(&self, _: C) -> Result<Self::Service, Self::InitError> {
        self.factory.create(()).await
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

        let _ = factory.create(&10).await;
        assert_eq!(item.get(), 11);
        let _ = format!("{factory:?}");
    }

    #[ntex::test]
    async fn test_unit_config() {
        let _ = unit_config(fn_service(|item: usize| async move { Ok::<_, ()>(item) }))
            .clone()
            .create(&10)
            .await;
    }
}
