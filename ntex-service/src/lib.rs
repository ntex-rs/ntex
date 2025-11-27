//! See [`Service`] docs for information on this crate's foundational trait.
#![allow(async_fn_in_trait)]
#![deny(
    rust_2018_idioms,
    warnings,
    unreachable_pub,
    missing_debug_implementations
)]
use std::{rc::Rc, task::Context};

mod and_then;
mod apply;
pub mod boxed;
pub mod cfg;
mod chain;
mod ctx;
mod fn_service;
mod fn_shutdown;
mod macros;
mod map;
mod map_config;
mod map_err;
mod map_init_err;
mod middleware;
mod pipeline;
mod then;
mod util;

pub use self::apply::{apply_fn, apply_fn_factory};
pub use self::chain::{chain, chain_factory};
pub use self::ctx::ServiceCtx;
pub use self::fn_service::{fn_factory, fn_factory_with_config, fn_service};
pub use self::fn_shutdown::fn_shutdown;
pub use self::map_config::{map_config, unit_config};
pub use self::middleware::{Identity, Middleware, Stack, apply};
pub use self::pipeline::{Pipeline, PipelineBinding, PipelineCall};

#[allow(unused_variables)]
/// An asynchronous function of `Request` to a `Response`.
///
/// The `Service` trait represents a request/response interaction, receiving requests and returning
/// replies. You can think about service as a function with one argument that returns some result
/// asynchronously. Conceptually, the operation looks like this:
///
/// ```rust,ignore
/// async fn(Request) -> Result<Response, Error>
/// ```
///
/// The `Service` trait just generalizes this form. Requests are defined as a generic type parameter
/// and responses and other details are defined as associated types on the trait impl. Notice that
/// this design means that services can receive many request types and converge them to a single
/// response type.
///
/// Services can also have mutable state that influence computation by using a `Cell`, `RefCell`
/// or `Mutex`. Services intentionally do not take `&mut self` to reduce overhead in the
/// common cases.
///
/// `Service` provides a symmetric and uniform API; the same abstractions can be used to represent
/// both clients and servers. Services describe only _transformation_ operations which encourage
/// simple API surfaces. This leads to simpler design of each service, improves test-ability and
/// makes composition easier.
///
/// ```rust
/// # use std::convert::Infallible;
/// #
/// # use ntex_service::{Service, ServiceCtx};
///
/// struct MyService;
///
/// impl Service<u8> for MyService {
///     type Response = u64;
///     type Error = Infallible;
///
///     async fn call(&self, req: u8, ctx: ServiceCtx<'_, Self>) -> Result<Self::Response, Self::Error> {
///         Ok(req as u64)
///     }
/// }
/// ```
///
/// Sometimes it is not necessary to implement the Service trait. For example, the above service
/// could be rewritten as a simple function and passed to [`fn_service`](fn_service()).
///
/// ```rust,ignore
/// async fn my_service(req: u8) -> Result<u64, Infallible>;
/// ```
///
/// Service cannot be called directly, it must be wrapped to an instance of [`Pipeline``] or
/// by using `ctx` argument of the call method in case of chanined services.
///
pub trait Service<Req> {
    /// Responses given by the service.
    type Response;

    /// Errors produced by the service when polling readiness or executing call.
    type Error;

    /// Process the request and return the response asynchronously.
    ///
    /// This function is expected to be callable off-task. As such, implementations of `call`
    /// should take care to not call `poll_ready`. Caller of the service verifies readiness,
    /// Only way to make a `call` is to use `ctx` argument, it enforces readiness before calling
    /// service.
    async fn call(
        &self,
        req: Req,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error>;

    #[inline]
    /// Returns when the service is able to process requests.
    ///
    /// If the service is at capacity, then `ready` does not returns and the task is notified when
    /// the service becomes ready again. This function is expected to be called while on a task.
    ///
    /// This is a **best effort** implementation. False positives are permitted. It is permitted for
    /// the service to returns from a `ready` call and the next invocation of `call`
    /// results in an error.
    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        Ok(())
    }

    #[inline]
    /// Shutdown service.
    ///
    /// Returns when the service is properly shutdowns.
    async fn shutdown(&self) {}

    #[inline]
    /// Polls service from the current task.
    ///
    /// Service may require to execute asynchronous computation or
    /// maintain asynchronous state.
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        Ok(())
    }

    #[inline]
    /// Map this service's output to a different type, returning a new service of the resulting type.
    ///
    /// This function is similar to the `Option::map` or `Iterator::map` where it will change
    /// the type of the underlying service.
    ///
    /// Note that this function consumes the receiving service and returns a wrapped version of it,
    /// similar to the existing `map` methods in the standard library.
    fn map<F, Res>(self, f: F) -> dev::ServiceChain<dev::Map<Self, F, Req, Res>, Req>
    where
        Self: Sized,
        F: Fn(Self::Response) -> Res,
    {
        chain(dev::Map::new(self, f))
    }

    #[inline]
    /// Map this service's error to a different error, returning a new service.
    ///
    /// This function is similar to the `Result::map_err` where it will change the error type of
    /// the underlying service. This is useful for example to ensure that services have the same
    /// error type.
    ///
    /// Note that this function consumes the receiving service and returns a wrapped version of it.
    fn map_err<F, E>(self, f: F) -> dev::ServiceChain<dev::MapErr<Self, F, E>, Req>
    where
        Self: Sized,
        F: Fn(Self::Error) -> E,
    {
        chain(dev::MapErr::new(self, f))
    }

    #[inline]
    /// Create pipeline for this service
    fn pipeline(self) -> Pipeline<Self> where Self: Sized {
        Pipeline::new(self)
    }
}

/// Factory for creating `Service`s.
///
/// This is useful for cases where new `Service`s must be produced. One case is a TCP server
/// listener: a listener accepts new connections, constructs a new `Service` for each using
/// the `ServiceFactory` trait, and uses the new `Service` to process inbound requests on that
/// new connection.
///
/// `Config` is a service factory configuration type.
///
/// Simple factories may be able to use [`fn_factory`] or [`fn_factory_with_config`] to
/// reduce boilerplate.
pub trait ServiceFactory<Req, Cfg = ()> {
    /// Responses given by the created services.
    type Response;

    /// Errors produced by the created services.
    type Error;

    /// The kind of `Service` created by this factory.
    type Service: Service<Req, Response = Self::Response, Error = Self::Error>;

    /// Errors potentially raised while building a service.
    type InitError;

    /// Create and return a new service value asynchronously.
    async fn create(&self, cfg: Cfg) -> Result<Self::Service, Self::InitError>;

    #[inline]
    /// Create and return a new service value asynchronously and wrap into a container
    async fn pipeline(&self, cfg: Cfg) -> Result<Pipeline<Self::Service>, Self::InitError>
    where
        Self: Sized,
    {
        Ok(Pipeline::new(self.create(cfg).await?))
    }

    #[inline]
    /// Map this service's output to a different type, returning a new service
    /// of the resulting type.
    fn map<F, Res>(
        self,
        f: F,
    ) -> dev::ServiceChainFactory<dev::MapFactory<Self, F, Req, Res, Cfg>, Req, Cfg>
    where
        Self: Sized,
        F: Fn(Self::Response) -> Res + Clone,
    {
        chain_factory(dev::MapFactory::new(self, f))
    }

    #[inline]
    /// Map this service's error to a different error, returning a new service.
    fn map_err<F, E>(
        self,
        f: F,
    ) -> dev::ServiceChainFactory<dev::MapErrFactory<Self, Req, Cfg, F, E>, Req, Cfg>
    where
        Self: Sized,
        F: Fn(Self::Error) -> E + Clone,
    {
        chain_factory(dev::MapErrFactory::new(self, f))
    }

    #[inline]
    /// Map this factory's init error to a different error, returning a new service.
    fn map_init_err<F, E>(
        self,
        f: F,
    ) -> dev::ServiceChainFactory<dev::MapInitErr<Self, Req, Cfg, F, E>, Req, Cfg>
    where
        Self: Sized,
        F: Fn(Self::InitError) -> E + Clone,
    {
        chain_factory(dev::MapInitErr::new(self, f))
    }
}

impl<S, Req> Service<Req> for &S
where
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;

    #[inline]
    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), S::Error> {
        ctx.ready(&**self).await
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), S::Error> {
        (**self).poll(cx)
    }

    #[inline]
    async fn shutdown(&self) {
        (**self).shutdown().await
    }

    #[inline]
    async fn call(
        &self,
        request: Req,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        ctx.call_nowait(&**self, request).await
    }
}

impl<S, Req> Service<Req> for Box<S>
where
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;

    #[inline]
    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), S::Error> {
        ctx.ready(&**self).await
    }

    #[inline]
    async fn shutdown(&self) {
        (**self).shutdown().await
    }

    #[inline]
    async fn call(
        &self,
        request: Req,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        ctx.call_nowait(&**self, request).await
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), S::Error> {
        (**self).poll(cx)
    }
}

impl<S, Req, Cfg> ServiceFactory<Req, Cfg> for Rc<S>
where
    S: ServiceFactory<Req, Cfg>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Service = S::Service;
    type InitError = S::InitError;

    async fn create(&self, cfg: Cfg) -> Result<Self::Service, Self::InitError> {
        self.as_ref().create(cfg).await
    }
}

/// Trait for types that can be converted to a `Service`
pub trait IntoService<Svc, Req>
where
    Svc: Service<Req>,
{
    /// Convert to a `Service`
    fn into_service(self) -> Svc;
}

/// Trait for types that can be converted to a `ServiceFactory`
pub trait IntoServiceFactory<T, Req, Cfg = ()>
where
    T: ServiceFactory<Req, Cfg>,
{
    /// Convert `Self` to a `ServiceFactory`
    fn into_factory(self) -> T;
}

impl<Svc, Req> IntoService<Svc, Req> for Svc
where
    Svc: Service<Req>,
{
    #[inline]
    fn into_service(self) -> Svc {
        self
    }
}

impl<T, Req, Cfg> IntoServiceFactory<T, Req, Cfg> for T
where
    T: ServiceFactory<Req, Cfg>,
{
    #[inline]
    fn into_factory(self) -> T {
        self
    }
}

pub mod dev {
    pub use crate::and_then::{AndThen, AndThenFactory};
    pub use crate::apply::{Apply, ApplyFactory};
    pub use crate::chain::{ServiceChain, ServiceChainFactory};
    pub use crate::fn_service::{
        FnService, FnServiceConfig, FnServiceFactory, FnServiceNoConfig,
    };
    pub use crate::fn_shutdown::FnShutdown;
    pub use crate::map::{Map, MapFactory};
    pub use crate::map_config::{MapConfig, UnitConfig};
    pub use crate::map_err::{MapErr, MapErrFactory};
    pub use crate::map_init_err::MapInitErr;
    pub use crate::middleware::ApplyMiddleware;
    pub use crate::then::{Then, ThenFactory};
}
