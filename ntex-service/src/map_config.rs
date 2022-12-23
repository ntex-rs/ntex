use std::{future::Future, marker::PhantomData, pin::Pin, task::Context, task::Poll};

use super::{IntoServiceFactory, ServiceFactory};

/// Adapt external config argument to a config for provided service factory
///
/// Note that this function consumes the receiving service factory and returns
/// a wrapped version of it.
pub fn map_config<T, R, U, F, C, C2>(factory: U, f: F) -> MapConfig<T, F, C, C2>
where
    T: ServiceFactory<R, C2>,
    U: IntoServiceFactory<T, R, C2>,
    F: Fn(&C) -> C2,
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

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

impl<A, F, R, C, C2> ServiceFactory<R, C> for MapConfig<A, F, C, C2>
where
    A: ServiceFactory<R, C2>,
    F: Fn(&C) -> C2,
{
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
pub struct UnitConfig<A> {
    a: A,
}

impl<A> UnitConfig<A> {
    /// Create new `UnitConfig` combinator
    pub(crate) fn new(a: A) -> Self {
        Self { a }
    }
}

impl<A> Clone for UnitConfig<A>
where
    A: Clone,
{
    fn clone(&self) -> Self {
        Self { a: self.a.clone() }
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
    type Future<'f> = UnitConfigFuture<'f, A, R, C> where Self: 'f, A: 'f, C: 'f;

    fn create(&self, _: &C) -> Self::Future<'_> {
        UnitConfigFuture {
            fut: self.a.create(&()),
            _t: PhantomData,
        }
    }
}

pin_project_lite::pin_project! {
    pub struct UnitConfigFuture<'f, A, R, C>
    where A: ServiceFactory<R>,
          A: 'f,
          C: 'f,
    {
        #[pin]
        fut: A::Future<'f>,
        _t: PhantomData<C>,
    }
}

impl<'f, A, R, C> Future for UnitConfigFuture<'f, A, R, C>
where
    A: ServiceFactory<R>,
    C: 'f,
{
    type Output = Result<A::Service, A::InitError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().fut.poll(cx)
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
