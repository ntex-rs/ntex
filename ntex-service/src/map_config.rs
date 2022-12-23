use std::{future::Future, marker::PhantomData, pin::Pin};

use super::{IntoServiceFactory, ServiceFactory};

/// Adapt external config argument to a config for provided service factory
///
/// Note that this function consumes the receiving service factory and returns
/// a wrapped version of it.
pub fn map_config<T, U, F, C, C2>(factory: U, f: F) -> MapConfig<T, F, C, C2>
where
    T: ServiceFactory<C2>,
    U: IntoServiceFactory<T, C2>,
    F: Fn(&C) -> C2,
{
    MapConfig::new(factory.into_factory(), f)
}

/// Replace config with unit
pub fn unit_config<T, U, C>(factory: U) -> UnitConfig<T, C>
where
    T: ServiceFactory<()>,
    U: IntoServiceFactory<T, ()>,
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
    pub(crate) fn new(a: A, f: F) -> Self
    where
        A: ServiceFactory<C2>,
        F: Fn(&C) -> C2,
    {
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

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

impl<A, F, C, C2> ServiceFactory<C> for MapConfig<A, F, C, C2>
where
    A: ServiceFactory<C2>,
    F: Fn(&C) -> C2,
{
    type Request = A::Request;
    type Response = A::Response;
    type Error = A::Error;

    type Service = A::Service;
    type InitError = A::InitError;
    type Future<'f> = BoxFuture<'f, Result<Self::Service, Self::InitError>> where Self: 'f, C2: 'f;

    fn create(&self, cfg: &C) -> Self::Future<'_> {
        let cfg = (self.f)(cfg);
        Box::pin(async move { self.a.create(&cfg).await })
    }
}

/// `unit_config()` config combinator
pub struct UnitConfig<A, C> {
    a: A,
    _t: PhantomData<C>,
}

impl<A, C> UnitConfig<A, C>
where
    A: ServiceFactory<()>,
{
    /// Create new `UnitConfig` combinator
    pub(crate) fn new(a: A) -> Self {
        Self { a, _t: PhantomData }
    }
}

impl<A, C> Clone for UnitConfig<A, C>
where
    A: Clone,
{
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            _t: PhantomData,
        }
    }
}

impl<A, C> ServiceFactory<C> for UnitConfig<A, C>
where
    A: ServiceFactory<()>,
{
    type Request = A::Request;
    type Response = A::Response;
    type Error = A::Error;

    type Service = A::Service;
    type InitError = A::InitError;
    type Future<'f> = A::Future<'f> where Self: 'f, C: 'f;

    fn create(&self, _: &C) -> Self::Future<'_> {
        self.a.create(&())
    }
}

#[cfg(test)]
#[allow(clippy::redundant_closure)]
mod tests {
    use ntex_util::future::Ready;
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::{fn_service, ServiceFactory};

    #[ntex::test]
    async fn test_map_config() {
        let item = Rc::new(Cell::new(1usize));

        let factory = map_config(
            fn_service(|item: usize| Ready::<_, ()>::Ok(item)),
            |t: &usize| {
                item.set(item.get() + *t);
            },
        )
        .clone();

        let _ = factory.create(&10).await;
        assert_eq!(item.get(), 11);
    }

    #[ntex::test]
    async fn test_unit_config() {
        let _ = unit_config(fn_service(|item: usize| Ready::<_, ()>::Ok(item)))
            .clone()
            .create(&10)
            .await;
    }

    // #[ntex::test]
    // async fn test_map_config_service() {
    //     let item = Rc::new(Cell::new(10usize));
    //     let item2 = item.clone();

    //     let srv = map_config_service(
    //         fn_factory_with_config(move |next: usize| {
    //             let item = item2.clone();
    //             async move {
    //                 item.set(next);
    //                 Ok::<_, ()>(fn_service(|id: usize| Ready::<_, ()>::Ok(id * 2)))
    //             }
    //         }),
    //         fn_service(move |item: usize| Ready::<_, ()>::Ok(item + 1)),
    //     )
    //     .clone()
    //     .create(10)
    //     .await
    //     .unwrap();

    //     assert_eq!(srv.call(10usize).await.unwrap(), 20);
    //     assert_eq!(item.get(), 11);
    // }
}
