use std::rc::Rc;
use std::sync::Arc;

use futures::{Future, IntoFuture, Poll};

pub use void::Void;

mod and_then;
mod and_then_apply;
mod and_then_apply_fn;
mod apply;
pub mod blank;
pub mod boxed;
mod cell;
mod fn_service;
mod fn_transform;
mod from_err;
mod map;
mod map_err;
mod map_init_err;
mod then;
mod transform;
mod transform_map_init_err;

pub use self::and_then::{AndThen, AndThenNewService};
use self::and_then_apply::AndThenTransform;
use self::and_then_apply_fn::{AndThenApply, AndThenApplyNewService};
pub use self::apply::{Apply, ApplyNewService};
pub use self::fn_service::{fn_cfg_factory, fn_factory, fn_service, FnService};
pub use self::fn_transform::FnTransform;
pub use self::from_err::{FromErr, FromErrNewService};
pub use self::map::{Map, MapNewService};
pub use self::map_err::{MapErr, MapErrNewService};
pub use self::map_init_err::MapInitErr;
pub use self::then::{Then, ThenNewService};
pub use self::transform::{ApplyTransform, IntoTransform, Transform};

/// An asynchronous function from `Request` to a `Response`.
///
/// `Request` - requests handled by the service.
pub trait Service<Request> {
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
    fn call(&mut self, req: Request) -> Self::Future;
}

/// An extension trait for `Service`s that provides a variety of convenient
/// adapters
pub trait ServiceExt<Request>: Service<Request> {
    /// Apply function to specified service and use it as a next service in
    /// chain.
    fn apply_fn<F, B, B1, Out, Req>(
        self,
        service: B1,
        f: F,
    ) -> AndThenApply<Self, B, F, Out, Request, Req>
    where
        Self: Sized,
        F: FnMut(Self::Response, &mut B) -> Out,
        Out: IntoFuture,
        Out::Error: Into<Self::Error>,
        B: Service<Req, Error = Self::Error>,
        B1: IntoService<B, Req>,
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
        F: IntoService<B, Self::Response>,
        B: Service<Self::Response, Error = Self::Error>,
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
        B: Service<Result<Self::Response, Self::Error>, Error = Self::Error>,
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

impl<T: ?Sized, R> ServiceExt<R> for T where T: Service<R> {}

/// Creates new `Service` values.
///
/// Acts as a service factory. This is useful for cases where new `Service`
/// values must be produced. One case is a TCP servier listener. The listner
/// accepts new TCP streams, obtains a new `Service` value using the
/// `NewService` trait, and uses that new `Service` value to process inbound
/// requests on that new TCP stream.
///
/// * `Request` - requests handled by the service.
/// * `Config` - is a service factory configuration type.
pub trait NewService<Request, Config = ()> {
    /// Responses given by the service
    type Response;

    /// Errors produced by the service
    type Error;

    /// The `Service` value created by this factory
    type Service: Service<Request, Response = Self::Response, Error = Self::Error>;

    /// Errors produced while building a service.
    type InitError;

    /// The future of the `Service` instance.
    type Future: Future<Item = Self::Service, Error = Self::InitError>;

    /// Create and return a new service value asynchronously.
    fn new_service(&self, cfg: &Config) -> Self::Future;

    /// Apply function to specified service and use it as a next service in
    /// chain.
    fn apply<T, T1, B, B1, Req>(
        self,
        transform: T1,
        service: B1,
    ) -> AndThenTransform<T, Self, B, Req>
    where
        Self: Sized,
        T: Transform<Self::Response, B::Service, InitError = Self::InitError>,
        T::Error: From<Self::Error>,
        T1: IntoTransform<T, Self::Response, B::Service>,
        B: NewService<Req, Config, InitError = Self::InitError>,
        B1: IntoNewService<B, Req, Config>,
    {
        AndThenTransform::new(transform.into_transform(), self, service.into_new_service())
    }

    /// Apply function to specified service and use it as a next service in
    /// chain.
    fn apply_fn<B, I, F, Out, Req>(
        self,
        service: I,
        f: F,
    ) -> AndThenApplyNewService<Self, B, F, Out, Request, Req, Config>
    where
        Self: Sized,
        B: NewService<Req, Config, Error = Self::Error, InitError = Self::InitError>,
        I: IntoNewService<B, Req, Config>,
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
        F: IntoNewService<B, Self::Response, Config>,
        B: NewService<Self::Response, Config, Error = Self::Error, InitError = Self::InitError>,
    {
        AndThenNewService::new(self, new_service)
    }

    /// `NewService` that create service to map this service's error
    /// and new service's init error to any error
    /// implementing `From` for this service`s `Error`.
    ///
    /// Note that this function consumes the receiving new service and returns a
    /// wrapped version of it.
    fn from_err<E>(self) -> FromErrNewService<Self, E, Config>
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
    fn then<F, B>(self, new_service: F) -> ThenNewService<Self, B, Config>
    where
        Self: Sized,
        F: IntoNewService<B, Result<Self::Response, Self::Error>, Config>,
        B: NewService<
            Result<Self::Response, Self::Error>,
            Config,
            Error = Self::Error,
            InitError = Self::InitError,
        >,
    {
        ThenNewService::new(self, new_service.into_new_service())
    }

    /// Map this service's output to a different type, returning a new service
    /// of the resulting type.
    fn map<F, R>(self, f: F) -> MapNewService<Self, F, R, Config>
    where
        Self: Sized,
        F: FnMut(Self::Response) -> R,
    {
        MapNewService::new(self, f)
    }

    /// Map this service's error to a different error, returning a new service.
    fn map_err<F, E>(self, f: F) -> MapErrNewService<Self, F, E, Config>
    where
        Self: Sized,
        F: Fn(Self::Error) -> E,
    {
        MapErrNewService::new(self, f)
    }

    /// Map this service's init error to a different error, returning a new service.
    fn map_init_err<F, E>(self, f: F) -> MapInitErr<Self, F, E, Config>
    where
        Self: Sized,
        F: Fn(Self::InitError) -> E,
    {
        MapInitErr::new(self, f)
    }
}

impl<'a, S, R> Service<R> for &'a mut S
where
    S: Service<R> + 'a,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), S::Error> {
        (**self).poll_ready()
    }

    fn call(&mut self, request: R) -> S::Future {
        (**self).call(request)
    }
}

impl<S, R> Service<R> for Box<S>
where
    S: Service<R> + ?Sized,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), S::Error> {
        (**self).poll_ready()
    }

    fn call(&mut self, request: R) -> S::Future {
        (**self).call(request)
    }
}

impl<S, R, C> NewService<R, C> for Rc<S>
where
    S: NewService<R, C>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Service = S::Service;
    type InitError = S::InitError;
    type Future = S::Future;

    fn new_service(&self, cfg: &C) -> S::Future {
        self.as_ref().new_service(cfg)
    }
}

impl<S, R, C> NewService<R, C> for Arc<S>
where
    S: NewService<R, C>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Service = S::Service;
    type InitError = S::InitError;
    type Future = S::Future;

    fn new_service(&self, cfg: &C) -> S::Future {
        self.as_ref().new_service(cfg)
    }
}

/// Trait for types that can be converted to a `Service`
pub trait IntoService<T, R>
where
    T: Service<R>,
{
    /// Convert to a `Service`
    fn into_service(self) -> T;
}

/// Trait for types that can be converted to a `NewService`
pub trait IntoNewService<T, R, C = ()>
where
    T: NewService<R, C>,
{
    /// Convert to an `NewService`
    fn into_new_service(self) -> T;
}

impl<T, R> IntoService<T, R> for T
where
    T: Service<R>,
{
    fn into_service(self) -> T {
        self
    }
}

impl<T, R, C> IntoNewService<T, R, C> for T
where
    T: NewService<R, C>,
{
    fn into_new_service(self) -> T {
        self
    }
}

/// Trait for types that can be converted to a configurable `NewService`
pub trait IntoConfigurableNewService<T, R, C>
where
    T: NewService<R, C>,
{
    /// Convert to an `NewService`
    fn into_new_service(self) -> T;
}

impl<T, R, C> IntoConfigurableNewService<T, R, C> for T
where
    T: NewService<R, C>,
{
    fn into_new_service(self) -> T {
        self
    }
}
