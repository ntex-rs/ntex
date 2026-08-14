use std::{fmt, marker::PhantomData};

use super::{IntoServiceFactory, Service, ServiceCtx, ServiceFactory};

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

impl<A, F, R, C, C2> Service<C> for MapConfig<A, F, C, C2, R>
where
    A: ServiceFactory<R, C2>,
    A::Response: Service<R, Data = A::Data>,
    F: Fn(C) -> C2,
{
    type Response = A::Response;
    type Error = A::Error;
    type Data = A::Data;

    async fn call(
        &self,
        cfg: C,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        ctx.call(&self.a, (self.f)(cfg), data).await
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

impl<A, R, C> Service<C> for UnitConfig<A, R>
where
    A: ServiceFactory<R, ()>,
    A::Response: Service<R, Data = A::Data>,
{
    type Response = A::Response;
    type Error = A::Error;
    type Data = A::Data;

    async fn call(
        &self,
        _: C,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        ctx.call(&self.factory, (), data).await
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
