use std::marker::PhantomData;

use super::{IntoServiceFactory, ServiceFactory};

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
