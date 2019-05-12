use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use futures::{Future, IntoFuture, Poll};

mod and_then;
mod and_then_apply;
mod and_then_apply_fn;
mod apply;
mod apply_cfg;
pub mod blank;
pub mod boxed;
mod cell;
mod fn_service;
mod fn_transform;
mod from_err;
mod map;
mod map_config;
mod map_err;
mod map_init_err;
mod then;
mod transform;
mod transform_err;

pub use self::and_then::{AndThen, AndThenNewService};
pub use self::apply::{apply_fn, new_apply_fn, Apply, ApplyNewService};
pub use self::apply_cfg::apply_cfg;
pub use self::fn_service::{new_service_cfg, new_service_fn, service_fn, ServiceFn};
pub use self::fn_transform::transform_fn;
pub use self::from_err::{FromErr, FromErrNewService};
pub use self::map::{Map, MapNewService};
pub use self::map_config::{MapConfig, MappedConfig, UnitConfig};
pub use self::map_err::{MapErr, MapErrNewService};
pub use self::map_init_err::MapInitErr;
pub use self::then::{Then, ThenNewService};
pub use self::transform::{apply_transform, IntoTransform, Transform};

use self::and_then_apply::AndThenTransform;
use self::and_then_apply_fn::{AndThenApply, AndThenApplyNewService};

/// An asynchronous function from `Request` to a `Response`.
pub trait Service {
    /// Requests handled by the service.
    type Request;

    /// Responses given by the service.
    type Response;

    /// Errors produced by the service.
    type Error;

    /// The future response value.
    type Future: Future<Item = Self::Response, Error = Self::Error>;

    /// Returns `Ready` when the service is able to process requests.
    ///
    /// If the service is at capacity, then `NotReady` is returned and the task
    /// is notified when the service becomes ready again. This function is
    /// expected to be called while on a task.
    ///
    /// This is a **best effort** implementation. False positives are permitted.
    /// It is permitted for the service to return `Ready` from a `poll_ready`
    /// call and the next invocation of `call` results in an error.
    fn poll_ready(&mut self) -> Poll<(), Self::Error>;

    /// Process the request and return the response asynchronously.
    ///
    /// This function is expected to be callable off task. As such,
    /// implementations should take care to not call `poll_ready`. If the
    /// service is at capacity and the request is unable to be handled, the
    /// returned `Future` should resolve to an error.
    ///
    /// Calling `call` without calling `poll_ready` is permitted. The
    /// implementation must be resilient to this fact.
    fn call(&mut self, req: Self::Request) -> Self::Future;
}

/// An extension trait for `Service`s that provides a variety of convenient
/// adapters
pub trait ServiceExt: Service {
    /// Apply function to specified service and use it as a next service in
    /// chain.
    fn apply_fn<F, B, B1, Out>(self, service: B1, f: F) -> AndThenApply<Self, B, F, Out>
    where
        Self: Sized,
        F: FnMut(Self::Response, &mut B) -> Out,
        Out: IntoFuture,
        Out::Error: Into<Self::Error>,
        B: Service<Error = Self::Error>,
        B1: IntoService<B>,
    {
        AndThenApply::new(self, service, f)
    }

    /// Call another service after call to this one has resolved successfully.
    ///
    /// This function can be used to chain two services together and ensure that
    /// the second service isn't called until call to the fist service have
    /// finished. Result of the call to the first service is used as an
    /// input parameter for the second service's call.
    ///
    /// Note that this function consumes the receiving service and returns a
    /// wrapped version of it.
    fn and_then<F, B>(self, service: F) -> AndThen<Self, B>
    where
        Self: Sized,
        F: IntoService<B>,
        B: Service<Request = Self::Response, Error = Self::Error>,
    {
        AndThen::new(self, service.into_service())
    }

    /// Map this service's error to any error implementing `From` for
    /// this service`s `Error`.
    ///
    /// Note that this function consumes the receiving service and returns a
    /// wrapped version of it.
    fn from_err<E>(self) -> FromErr<Self, E>
    where
        Self: Sized,
        E: From<Self::Error>,
    {
        FromErr::new(self)
    }

    /// Chain on a computation for when a call to the service finished,
    /// passing the result of the call to the next service `B`.
    ///
    /// Note that this function consumes the receiving service and returns a
    /// wrapped version of it.
    fn then<B>(self, service: B) -> Then<Self, B>
    where
        Self: Sized,
        B: Service<Request = Result<Self::Response, Self::Error>, Error = Self::Error>,
    {
        Then::new(self, service)
    }

    /// Map this service's output to a different type, returning a new service
    /// of the resulting type.
    ///
    /// This function is similar to the `Option::map` or `Iterator::map` where
    /// it will change the type of the underlying service.
    ///
    /// Note that this function consumes the receiving service and returns a
    /// wrapped version of it, similar to the existing `map` methods in the
    /// standard library.
    fn map<F, R>(self, f: F) -> Map<Self, F, R>
    where
        Self: Sized,
        F: FnMut(Self::Response) -> R,
    {
        Map::new(self, f)
    }

    /// Map this service's error to a different error, returning a new service.
    ///
    /// This function is similar to the `Result::map_err` where it will change
    /// the error type of the underlying service. This is useful for example to
    /// ensure that services have the same error type.
    ///
    /// Note that this function consumes the receiving service and returns a
    /// wrapped version of it.
    fn map_err<F, E>(self, f: F) -> MapErr<Self, F, E>
    where
        Self: Sized,
        F: Fn(Self::Error) -> E,
    {
        MapErr::new(self, f)
    }
}

impl<T: ?Sized> ServiceExt for T where T: Service {}

/// Creates new `Service` values.
///
/// Acts as a service factory. This is useful for cases where new `Service`
/// values must be produced. One case is a TCP servier listener. The listner
/// accepts new TCP streams, obtains a new `Service` value using the
/// `NewService` trait, and uses that new `Service` value to process inbound
/// requests on that new TCP stream.
///
/// `Config` is a service factory configuration type.
pub trait NewService {
    /// Requests handled by the service.
    type Request;

    /// Responses given by the service
    type Response;

    /// Errors produced by the service
    type Error;

    /// Service factory configuration
    type Config;

    /// The `Service` value created by this factory
    type Service: Service<
        Request = Self::Request,
        Response = Self::Response,
        Error = Self::Error,
    >;

    /// Errors produced while building a service.
    type InitError;

    /// The future of the `Service` instance.
    type Future: Future<Item = Self::Service, Error = Self::InitError>;

    /// Create and return a new service value asynchronously.
    fn new_service(&self, cfg: &Self::Config) -> Self::Future;

    /// Apply transform service to specified service and use it as a next service in
    /// chain.
    fn apply<T, T1, B, B1>(self, transform: T1, service: B1) -> AndThenTransform<T, Self, B>
    where
        Self: Sized,
        T: Transform<B::Service, Request = Self::Response, InitError = Self::InitError>,
        T::Error: From<Self::Error>,
        T1: IntoTransform<T, B::Service>,
        B: NewService<Config = Self::Config, InitError = Self::InitError>,
        B1: IntoNewService<B>,
    {
        AndThenTransform::new(transform.into_transform(), self, service.into_new_service())
    }

    /// Apply function to specified service and use it as a next service in
    /// chain.
    fn apply_fn<B, I, F, Out>(self, service: I, f: F) -> AndThenApplyNewService<Self, B, F, Out>
    where
        Self: Sized,
        B: NewService<Config = Self::Config, Error = Self::Error, InitError = Self::InitError>,
        I: IntoNewService<B>,
        F: FnMut(Self::Response, &mut B::Service) -> Out,
        Out: IntoFuture,
        Out::Error: Into<Self::Error>,
    {
        AndThenApplyNewService::new(self, service, f)
    }

    /// Call another service after call to this one has resolved successfully.
    fn and_then<F, B>(self, new_service: F) -> AndThenNewService<Self, B>
    where
        Self: Sized,
        F: IntoNewService<B>,
        B: NewService<
            Config = Self::Config,
            Request = Self::Response,
            Error = Self::Error,
            InitError = Self::InitError,
        >,
    {
        AndThenNewService::new(self, new_service)
    }

    /// `NewService` that create service to map this service's error
    /// and new service's init error to any error
    /// implementing `From` for this service`s `Error`.
    ///
    /// Note that this function consumes the receiving new service and returns a
    /// wrapped version of it.
    fn from_err<E>(self) -> FromErrNewService<Self, E>
    where
        Self: Sized,
        E: From<Self::Error>,
    {
        FromErrNewService::new(self)
    }

    /// Create `NewService` to chain on a computation for when a call to the
    /// service finished, passing the result of the call to the next
    /// service `B`.
    ///
    /// Note that this function consumes the receiving future and returns a
    /// wrapped version of it.
    fn then<F, B>(self, new_service: F) -> ThenNewService<Self, B>
    where
        Self: Sized,
        F: IntoNewService<B>,
        B: NewService<
            Config = Self::Config,
            Request = Result<Self::Response, Self::Error>,
            Error = Self::Error,
            InitError = Self::InitError,
        >,
    {
        ThenNewService::new(self, new_service)
    }

    /// Map this service's output to a different type, returning a new service
    /// of the resulting type.
    fn map<F, R>(self, f: F) -> MapNewService<Self, F, R>
    where
        Self: Sized,
        F: FnMut(Self::Response) -> R,
    {
        MapNewService::new(self, f)
    }

    /// Map this service's error to a different error, returning a new service.
    fn map_err<F, E>(self, f: F) -> MapErrNewService<Self, F, E>
    where
        Self: Sized,
        F: Fn(Self::Error) -> E + Clone,
    {
        MapErrNewService::new(self, f)
    }

    /// Map this factory's init error to a different error, returning a new service.
    fn map_init_err<F, E>(self, f: F) -> MapInitErr<Self, F, E>
    where
        Self: Sized,
        F: Fn(Self::InitError) -> E,
    {
        MapInitErr::new(self, f)
    }

    /// Map config to a different error, returning a new service.
    fn map_config<F, C>(self, f: F) -> MapConfig<Self, F, C>
    where
        Self: Sized,
        F: Fn(&C) -> MappedConfig<Self::Config>,
    {
        MapConfig::new(self, f)
    }

    /// Replace config with unit
    fn unit_config<C>(self) -> UnitConfig<Self, C>
    where
        Self: NewService<Config = ()> + Sized,
    {
        UnitConfig::new(self)
    }
}

impl<'a, S> Service for &'a mut S
where
    S: Service + 'a,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), S::Error> {
        (**self).poll_ready()
    }

    fn call(&mut self, request: Self::Request) -> S::Future {
        (**self).call(request)
    }
}

impl<S> Service for Box<S>
where
    S: Service + ?Sized,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), S::Error> {
        (**self).poll_ready()
    }

    fn call(&mut self, request: Self::Request) -> S::Future {
        (**self).call(request)
    }
}

impl<S> Service for Rc<RefCell<S>>
where
    S: Service,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), S::Error> {
        self.borrow_mut().poll_ready()
    }

    fn call(&mut self, request: Self::Request) -> S::Future {
        self.borrow_mut().call(request)
    }
}

impl<S> NewService for Rc<S>
where
    S: NewService,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Config = S::Config;
    type Service = S::Service;
    type InitError = S::InitError;
    type Future = S::Future;

    fn new_service(&self, cfg: &S::Config) -> S::Future {
        self.as_ref().new_service(cfg)
    }
}

impl<S> NewService for Arc<S>
where
    S: NewService,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Config = S::Config;
    type Service = S::Service;
    type InitError = S::InitError;
    type Future = S::Future;

    fn new_service(&self, cfg: &S::Config) -> S::Future {
        self.as_ref().new_service(cfg)
    }
}

/// Trait for types that can be converted to a `Service`
pub trait IntoService<T>
where
    T: Service,
{
    /// Convert to a `Service`
    fn into_service(self) -> T;
}

/// Trait for types that can be converted to a `NewService`
pub trait IntoNewService<T>
where
    T: NewService,
{
    /// Convert to an `NewService`
    fn into_new_service(self) -> T;
}

impl<T> IntoService<T> for T
where
    T: Service,
{
    fn into_service(self) -> T {
        self
    }
}

impl<T> IntoNewService<T> for T
where
    T: NewService,
{
    fn into_new_service(self) -> T {
        self
    }
}
