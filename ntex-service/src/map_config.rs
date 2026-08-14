use std::{fmt, marker::PhantomData};

use super::{IntoServiceFactory, Pipeline, Service, ServiceFactory};

/// Adapt external config argument to a config for provided service factory
///
/// Note that this function consumes the receiving service factory and returns
/// a wrapped version of it.
pub fn map_config<T, R, U, F, C, C2>(factory: U, f: F) -> MapConfig<T, F, C, C2, R>
where
    T: ServiceFactory<R, C2>,
    U: IntoServiceFactory<T, R, C2>,
    F: Fn(C) -> C2,
{
    MapConfig::new(factory.into_factory(), f)
}

/// Replace config with unit
pub fn unit_config<T, R, U>(factory: U) -> UnitConfig<T, R>
where
    T: ServiceFactory<R, ()>,
    U: IntoServiceFactory<T, R, ()>,
{
    UnitConfig::new(factory.into_factory())
}

/// `map_config()` adapter service factory
pub struct MapConfig<A, F, C, C2, R> {
    a: A,
    f: F,
    e: PhantomData<fn(C, C2, R)>,
}

impl<A, F, C, C2, R> MapConfig<A, F, C, C2, R> {
    /// Create new `MapConfig` combinator
    pub(crate) fn new(a: A, f: F) -> Self {
        Self {
            a,
            f,
            e: PhantomData,
        }
    }
}

impl<A, F, C, C2, R> Clone for MapConfig<A, F, C, C2, R>
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

impl<A, F, C, C2, R> fmt::Debug for MapConfig<A, F, C, C2, R>
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

impl<A, F, R, C, C2> ServiceFactory<R, C> for MapConfig<A, F, C, C2, R>
where
    A: ServiceFactory<R, C2>,
    F: Fn(C) -> C2,
    C: Clone,
{
    type Response = A::Response;
    type Error = A::Error;
    type Service = A::Service;
    type InitError = A::InitError;
    type Data = A::Data;

    async fn create(&self, cfg: C) -> Result<Self::Service, Self::InitError> {
        self.a.create((self.f)(cfg)).await
    }

    async fn map_data(
        &self,
        cfg: &C,
        data: &Self::Data,
    ) -> Result<<Self::Service as Service<R>>::Data, Self::InitError> {
        self.a.map_data(&(self.f)(cfg.clone()), data).await
    }

    async fn pipeline(
        &self,
        cfg: C,
        data: &Self::Data,
    ) -> Result<Pipeline<Self::Service, <Self::Service as Service<R>>::Data>, Self::InitError>
    {
        let cfg = (self.f)(cfg);
        let svc_data = self.a.map_data(&cfg, data).await?;
        Ok(Pipeline::new(self.a.create(cfg).await?, svc_data))
    }
}

#[derive(Clone, Debug)]
/// `unit_config()` config combinator
pub struct UnitConfig<A, R> {
    factory: A,
    _t: PhantomData<fn(R)>,
}

impl<A, R> UnitConfig<A, R> {
    /// Create new `UnitConfig` combinator
    pub(crate) fn new(factory: A) -> Self {
        Self {
            factory,
            _t: PhantomData,
        }
    }
}

impl<A, R, C> ServiceFactory<R, C> for UnitConfig<A, R>
where
    A: ServiceFactory<R, ()>,
{
    type Response = A::Response;
    type Error = A::Error;
    type Service = A::Service;
    type InitError = A::InitError;
    type Data = A::Data;

    async fn create(&self, _: C) -> Result<Self::Service, Self::InitError> {
        self.factory.create(()).await
    }

    async fn map_data(
        &self,
        _: &C,
        data: &Self::Data,
    ) -> Result<<Self::Service as Service<R>>::Data, Self::InitError> {
        self.factory.map_data(&(), data).await
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

        let svc = factory.pipeline(&10, &()).await.unwrap();
        assert_eq!(item.get(), 11);
        let _ = format!("{factory:?}");

        assert_eq!(svc.call(1).await.unwrap(), 1);
    }

    #[ntex::test]
    async fn test_unit_config() {
        let svc = unit_config(fn_service(|item: usize| async move { Ok::<_, ()>(item) }))
            .clone()
            .pipeline(&10, &())
            .await
            .unwrap();
        assert_eq!(svc.call(1).await.unwrap(), 1);
    }
}
