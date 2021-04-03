use std::task::{Context, Poll};
use std::{cell::RefCell, future::Future, marker::PhantomData, pin::Pin, rc::Rc};

use super::{IntoServiceFactory, Service, ServiceFactory};

/// Adapt external config argument to a config for provided service factory
///
/// Note that this function consumes the receiving service factory and returns
/// a wrapped version of it.
pub fn map_config<T, U, F, C>(factory: U, f: F) -> MapConfig<T, F, C>
where
    T: ServiceFactory,
    U: IntoServiceFactory<T>,
    F: Fn(C) -> T::Config,
{
    MapConfig::new(factory.into_factory(), f)
}

/// Adapt external config argument to a config for provided service factory
///
/// This function uses service for converting config.
pub fn map_config_service<T, M, C, U1, U2>(
    factory: U1,
    mapper: U2,
) -> MapConfigService<T, M, C>
where
    T: ServiceFactory,
    M: ServiceFactory<
        Config = (),
        Request = C,
        Response = T::Config,
        Error = T::InitError,
        InitError = T::InitError,
    >,
    U1: IntoServiceFactory<T>,
    U2: IntoServiceFactory<M>,
{
    MapConfigService::new(factory.into_factory(), mapper.into_factory())
}

/// Replace config with unit
pub fn unit_config<T, U, C>(factory: U) -> UnitConfig<T, C>
where
    T: ServiceFactory<Config = ()>,
    U: IntoServiceFactory<T>,
{
    UnitConfig::new(factory.into_factory())
}

/// `map_config()` adapter service factory
pub struct MapConfig<A, F, C> {
    a: A,
    f: F,
    e: PhantomData<C>,
}

impl<A, F, C> MapConfig<A, F, C> {
    /// Create new `MapConfig` combinator
    pub(crate) fn new(a: A, f: F) -> Self
    where
        A: ServiceFactory,
        F: Fn(C) -> A::Config,
    {
        Self {
            a,
            f,
            e: PhantomData,
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
            e: PhantomData,
        }
    }
}

impl<A, F, C> ServiceFactory for MapConfig<A, F, C>
where
    A: ServiceFactory,
    F: Fn(C) -> A::Config,
{
    type Request = A::Request;
    type Response = A::Response;
    type Error = A::Error;

    type Config = C;
    type Service = A::Service;
    type InitError = A::InitError;
    type Future = A::Future;

    fn new_service(&self, cfg: C) -> Self::Future {
        self.a.new_service((self.f)(cfg))
    }
}

/// `unit_config()` config combinator
pub struct UnitConfig<A, C> {
    a: A,
    e: PhantomData<C>,
}

impl<A, C> UnitConfig<A, C>
where
    A: ServiceFactory<Config = ()>,
{
    /// Create new `UnitConfig` combinator
    pub(crate) fn new(a: A) -> Self {
        Self { a, e: PhantomData }
    }
}

impl<A, C> Clone for UnitConfig<A, C>
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

impl<A, C> ServiceFactory for UnitConfig<A, C>
where
    A: ServiceFactory<Config = ()>,
{
    type Request = A::Request;
    type Response = A::Response;
    type Error = A::Error;

    type Config = C;
    type Service = A::Service;
    type InitError = A::InitError;
    type Future = A::Future;

    fn new_service(&self, _: C) -> Self::Future {
        self.a.new_service(())
    }
}

/// `map_config_service()` adapter service factory
pub struct MapConfigService<A, M: ServiceFactory, C>(Rc<Inner<A, M, C>>);

struct Inner<A, M: ServiceFactory, C> {
    a: A,
    m: M,
    mapper: RefCell<Option<M::Service>>,
    e: PhantomData<C>,
}

impl<A, M: ServiceFactory, C> Clone for MapConfigService<A, M, C> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<A, M: ServiceFactory, C> MapConfigService<A, M, C> {
    /// Create new `MapConfigService` combinator
    pub(crate) fn new(a: A, m: M) -> Self
    where
        A: ServiceFactory,
        M: ServiceFactory<
            Config = (),
            Request = C,
            Response = A::Config,
            Error = A::InitError,
            InitError = A::InitError,
        >,
    {
        Self(Rc::new(Inner {
            a,
            m,
            mapper: RefCell::new(None),
            e: PhantomData,
        }))
    }
}

impl<A, M, C> ServiceFactory for MapConfigService<A, M, C>
where
    A: ServiceFactory,
    M: ServiceFactory<
        Config = (),
        Request = C,
        Response = A::Config,
        Error = A::InitError,
        InitError = A::InitError,
    >,
{
    type Request = A::Request;
    type Response = A::Response;
    type Error = A::Error;

    type Config = C;
    type Service = A::Service;
    type InitError = A::InitError;
    type Future = MapConfigServiceResponse<A, M, C>;

    fn new_service(&self, cfg: C) -> Self::Future {
        let inner = self.0.clone();
        if self.0.mapper.borrow().is_some() {
            MapConfigServiceResponse {
                inner,
                config: Some(cfg),
                state: ResponseState::MapReady,
            }
        } else {
            MapConfigServiceResponse {
                inner,
                config: Some(cfg),
                state: ResponseState::CreateMapper {
                    fut: self.0.m.new_service(()),
                },
            }
        }
    }
}

pin_project_lite::pin_project! {
    pub struct MapConfigServiceResponse<A, M: ServiceFactory, C>
    where
        A: ServiceFactory,
        M: ServiceFactory,
    {
        inner: Rc<Inner<A, M, C>>,
        config: Option<C>,
        #[pin]
        state: ResponseState<A, M>,
    }
}

pin_project_lite::pin_project! {
    #[project = ResponseStateProject]
    enum ResponseState<A: ServiceFactory, M: ServiceFactory> {
        CreateMapper { #[pin] fut: M::Future },
        MapReady,
        MapConfig { #[pin] fut: <M::Service as Service>::Future },
        CreateService { #[pin] fut: A::Future },
    }
}

impl<A, M, C> Future for MapConfigServiceResponse<A, M, C>
where
    A: ServiceFactory,
    M: ServiceFactory<
        Config = (),
        Request = C,
        Response = A::Config,
        Error = A::InitError,
        InitError = A::InitError,
    >,
{
    type Output = Result<A::Service, A::InitError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        match this.state.as_mut().project() {
            ResponseStateProject::CreateMapper { fut } => {
                let mapper = match fut.poll(cx) {
                    Poll::Ready(result) => result?,
                    Poll::Pending => return Poll::Pending,
                };
                *this.inner.mapper.borrow_mut() = Some(mapper);
                this.state.set(ResponseState::MapReady);
                self.poll(cx)
            }
            ResponseStateProject::MapReady => {
                let mapper = this.inner.mapper.borrow();
                match mapper.as_ref().unwrap().poll_ready(cx) {
                    Poll::Ready(result) => result?,
                    Poll::Pending => return Poll::Pending,
                };

                let fut = mapper.as_ref().unwrap().call(this.config.take().unwrap());
                this.state.set(ResponseState::MapConfig { fut });
                drop(mapper);
                self.poll(cx)
            }
            ResponseStateProject::MapConfig { fut } => {
                let config = match fut.poll(cx) {
                    Poll::Ready(result) => result?,
                    Poll::Pending => return Poll::Pending,
                };
                let fut = this.inner.a.new_service(config);
                this.state.set(ResponseState::CreateService { fut });
                self.poll(cx)
            }
            ResponseStateProject::CreateService { fut } => fut.poll(cx),
        }
    }
}

#[cfg(test)]
#[allow(clippy::redundant_closure)]
mod tests {
    use ntex_util::future::Ready;
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::{fn_factory_with_config, fn_service, ServiceFactory};

    #[ntex::test]
    async fn test_map_config() {
        let item = Rc::new(Cell::new(1usize));

        let factory = map_config(
            fn_service(|item: usize| Ready::<_, ()>::Ok(item)),
            |t: usize| {
                item.set(item.get() + t);
            },
        )
        .clone();

        let _ = factory.new_service(10).await;
        assert_eq!(item.get(), 11);
    }

    #[ntex::test]
    async fn test_unit_config() {
        let _ = unit_config(fn_service(|item: usize| Ready::<_, ()>::Ok(item)))
            .clone()
            .new_service(10)
            .await;
    }

    #[ntex::test]
    async fn test_map_config_service() {
        let item = Rc::new(Cell::new(10usize));
        let item2 = item.clone();

        let srv = map_config_service(
            fn_factory_with_config(move |next: usize| {
                let item = item2.clone();
                async move {
                    item.set(next);
                    Ok::<_, ()>(fn_service(|id: usize| Ready::<_, ()>::Ok(id * 2)))
                }
            }),
            fn_service(move |item: usize| Ready::<_, ()>::Ok(item + 1)),
        )
        .clone()
        .new_service(10)
        .await
        .unwrap();

        assert_eq!(srv.call(10usize).await.unwrap(), 20);
        assert_eq!(item.get(), 11);
    }
}
