use std::{future::Future, marker::PhantomData, pin::Pin};

use super::{IntoServiceFactory, ServiceFactory};

/// Adapt external config argument to a config for provided service factory
///
/// Note that this function consumes the receiving service factory and returns
/// a wrapped version of it.
pub fn map_config<T, R, U, F, C, C2>(factory: U, f: F) -> MapConfig<T, R, F, C, C2>
where
    T: ServiceFactory<R, C2>,
    U: IntoServiceFactory<T, R, C2>,
    F: Fn(&C) -> C2,
{
    MapConfig::new(factory.into_factory(), f)
}

/// Replace config with unit
pub fn unit_config<T, R, U, C>(factory: U) -> UnitConfig<T, R, C>
where
    T: ServiceFactory<R, ()>,
    U: IntoServiceFactory<T, R, ()>,
{
    UnitConfig::new(factory.into_factory())
}

/// `map_config()` adapter service factory
pub struct MapConfig<A, R, F, C, C2> {
    a: A,
    f: F,
    e: PhantomData<(R, C, C2)>,
}

impl<A, R, F, C, C2> MapConfig<A, R, F, C, C2> {
    /// Create new `MapConfig` combinator
    pub(crate) fn new(a: A, f: F) -> Self
    where
        A: ServiceFactory<R, C2>,
        F: Fn(&C) -> C2,
    {
        Self {
            a,
            f,
            e: PhantomData,
        }
    }
}

impl<A, R, F, C, C2> Clone for MapConfig<A, R, F, C, C2>
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

impl<A, R, F, C, C2> ServiceFactory<R, C> for MapConfig<A, R, F, C, C2>
where
    A: ServiceFactory<R, C2>,
    F: Fn(&C) -> C2,
{
    type Response = A::Response;
    type Error = A::Error;

    type Service = A::Service;
    type InitError = A::InitError;
    type Future<'f> = BoxFuture<'f, Result<Self::Service, Self::InitError>> where Self: 'f, C2: 'f;

    fn create<'a>(&'a self, cfg: &'a C) -> Self::Future<'a> {
        Box::pin(async move {
            let cfg = (self.f)(cfg);
            self.a.create(&cfg).await
        })
    }
}

/// `unit_config()` config combinator
pub struct UnitConfig<A, R, C> {
    a: A,
    e: PhantomData<(C, R)>,
}

impl<A, R, C> UnitConfig<A, R, C>
where
    A: ServiceFactory<R, ()>,
{
    /// Create new `UnitConfig` combinator
    pub(crate) fn new(a: A) -> Self {
        Self { a, e: PhantomData }
    }
}

impl<A, R, C> Clone for UnitConfig<A, R, C>
where
    A: Clone,
{
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            e: PhantomData,
        }
    }
}

impl<A, R, C> ServiceFactory<R, C> for UnitConfig<A, R, C>
where
    A: ServiceFactory<R, ()>,
{
    type Response = A::Response;
    type Error = A::Error;

    type Service = A::Service;
    type InitError = A::InitError;
    type Future<'f> = A::Future<'f> where A: 'f, R: 'f, C: 'f;

    fn create<'a>(&'a self, _: &'a C) -> Self::Future<'a> {
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
                item.set(item.get() + t);
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
