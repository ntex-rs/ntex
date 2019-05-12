use std::marker::PhantomData;

use super::NewService;

pub enum MappedConfig<'a, T> {
    Ref(&'a T),
    Owned(T),
}

/// `MapInitErr` service combinator
pub struct MapConfig<A, F, C> {
    a: A,
    f: F,
    e: PhantomData<C>,
}

impl<A, F, C> MapConfig<A, F, C> {
    /// Create new `MapConfig` combinator
    pub fn new(a: A, f: F) -> Self
    where
        A: NewService,
        F: Fn(&C) -> MappedConfig<A::Config>,
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

impl<A, F, C> NewService for MapConfig<A, F, C>
where
    A: NewService,
    F: Fn(&C) -> MappedConfig<A::Config>,
{
    type Request = A::Request;
    type Response = A::Response;
    type Error = A::Error;

    type Config = C;
    type Service = A::Service;
    type InitError = A::InitError;
    type Future = A::Future;

    fn new_service(&self, cfg: &C) -> Self::Future {
        match (self.f)(cfg) {
            MappedConfig::Ref(cfg) => self.a.new_service(cfg),
            MappedConfig::Owned(cfg) => self.a.new_service(&cfg),
        }
    }
}

/// `MapInitErr` service combinator
pub struct UnitConfig<A, C> {
    a: A,
    e: PhantomData<C>,
}

impl<A, C> UnitConfig<A, C> {
    /// Create new `UnitConfig` combinator
    pub fn new(a: A) -> Self
    where
        A: NewService<Config = ()>,
    {
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

impl<A, C> NewService for UnitConfig<A, C>
where
    A: NewService<Config = ()>,
{
    type Request = A::Request;
    type Response = A::Response;
    type Error = A::Error;

    type Config = C;
    type Service = A::Service;
    type InitError = A::InitError;
    type Future = A::Future;

    fn new_service(&self, _: &C) -> Self::Future {
        self.a.new_service(&())
    }
}
