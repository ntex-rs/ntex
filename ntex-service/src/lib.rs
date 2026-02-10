//! See [`Service`] docs for information on this crate's foundational trait.
#![deny(clippy::pedantic)]
#![allow(
    clippy::missing_fields_in_debug,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::cast_possible_truncation
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
mod inspect;
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
pub use self::middleware::{Identity, Middleware, Stack, apply, fn_layer};
pub use self::pipeline::{Pipeline, PipelineBinding, PipelineCall, PipelineSvc};

#[allow(unused_variables)]
/// An asynchronous function from a `Request` to a `Response`.
///
/// The `Service` trait represents a request/response interaction, receiving
/// requests and returning replies. Conceptually, a service is like a function
/// with one argument that returns a result asynchronously:
///
/// ```rust,ignore
/// async fn(Request) -> Result<Response, Error>
/// ```
///
/// The `Service` trait generalizes this form. Requests are defined as a generic
/// type parameter, while responses and other details are defined as associated
/// types on the trait implementation. This design allows services to accept
/// many request types and produce a single response type.
///
/// Services can also have internal mutable state that influences computation
/// using `Cell`, `RefCell`, or `Mutex`. Services intentionally do not take
/// `&mut self` to reduce overhead in common use cases.
///
/// `Service` provides a uniform API; the same abstractions can represent both
/// clients and servers. Services describe only _transformation_ operations,
/// which encourages simple API surfaces, easier testing, and straightforward
/// composition.
///
/// Services can only be called within a pipeline. The `Pipeline` enforces
/// shared readiness for all services in the pipeline. To process requests from
/// one service to another, all services must be ready; otherwise, processing
/// is paused until that state is achieved.
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
/// Service cannot be called directly, it must be wrapped to an instance of [`Pipeline`] or
/// by using `ctx` argument of the call method in case of chanined services.
pub trait Service<Req> {
    /// Responses given by the service.
    type Response;

    /// Errors produced by the service when polling readiness or executing call.
    type Error;

    /// Processes a request and returns the response asynchronously.
    ///
    /// The `call` method can only be invoked within a pipeline, which enforces
    /// readiness for all services in the pipeline. Implementations of `call`
    /// must not call `ready`; the `ctx` argument ensures the service is ready
    /// before it is invoked.
    async fn call(
        &self,
        req: Req,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error>;

    #[inline]
    /// Returns when the service is ready to process requests.
    ///
    /// If the service is at capacity, `ready` will not return immediately. The current
    /// task is notified when the service becomes ready again. This function should
    /// be called while executing on a task.
    ///
    /// **Note:** Pipeline readiness is maintained across all services in the pipeline.
    /// The pipeline can process requests only if every service in the pipeline is ready.
    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        Ok(())
    }

    #[inline]
    /// Shuts down the service.
    ///
    /// Returns when the service has been properly shut down.
    async fn shutdown(&self) {}

    #[inline]
    /// Polls the service from the current async task.
    ///
    /// The service may perform asynchronous computations or
    /// maintain asynchronous state during polling.
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        Ok(())
    }

    #[inline]
    /// Maps this service's output to a different type, returning a new service.
    ///
    /// This is similar to `Option::map` or `Iterator::map`, changing the
    /// output type of the underlying service.
    ///
    /// This function consumes the original service and returns a wrapped version,
    /// following the pattern of standard library `map` methods.
    fn map<F, Res>(self, f: F) -> dev::ServiceChain<dev::Map<Self, F, Req, Res>, Req>
    where
        Self: Sized,
        F: Fn(Self::Response) -> Res,
    {
        chain(dev::Map::new(self, f))
    }

    #[inline]
    /// Maps this service's error to a different type, returning a new service.
    ///
    /// This is similar to `Result::map_err`, changing the error type of the
    /// underlying service. It is useful, for example, to ensure multiple
    /// services have the same error type.
    ///
    /// This function consumes the original service and returns a wrapped version.
    fn map_err<F, E>(self, f: F) -> dev::ServiceChain<dev::MapErr<Self, F, E>, Req>
    where
        Self: Sized,
        F: Fn(Self::Error) -> E,
    {
        chain(dev::MapErr::new(self, f))
    }
}

/// A factory for creating `Service`s.
///
/// This is useful when new `Service`s must be produced dynamically. For example,
/// a TCP server listener accepts new connections, constructs a new `Service` for
/// each connection using the `ServiceFactory` trait, and uses that service to
/// handle inbound requests.
///
/// `Config` represents the configuration type for the service factory.
///
/// Simple factories can often use [`fn_factory`] or [`fn_factory_with_config`]
/// to reduce boilerplate.
pub trait ServiceFactory<Req, Cfg = ()> {
    /// Responses given by the created services.
    type Response;

    /// Errors produced by the created services.
    type Error;

    /// The type of `Service` produced by this factory.
    type Service: Service<Req, Response = Self::Response, Error = Self::Error>;

    /// Possible errors encountered during service construction.
    type InitError;

    /// Creates a new service asynchronously and returns it.
    async fn create(&self, cfg: Cfg) -> Result<Self::Service, Self::InitError>;

    #[inline]
    /// Asynchronously creates a new service and wraps it in a container.
    async fn pipeline(&self, cfg: Cfg) -> Result<Pipeline<Self::Service>, Self::InitError>
    where
        Self: Sized,
    {
        Ok(Pipeline::new(self.create(cfg).await?))
    }

    #[inline]
    /// Returns a new service that maps this service's output to a different type.
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
    /// Transforms this service's error into another error,
    /// producing a new service.
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
    /// Maps this factory's initialization error to a different error,
    /// returning a new service factory.
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

    /// Creates a boxed service factory.
    fn boxed(
        self,
    ) -> boxed::BoxServiceFactory<Cfg, Req, Self::Response, Self::Error, Self::InitError>
    where
        Cfg: 'static,
        Req: 'static,
        Self: 'static + Sized,
    {
        boxed::factory(self)
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
        (**self).shutdown().await;
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
        (**self).shutdown().await;
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
    pub use crate::inspect::{InspectErr, InspectErrFactory};
    pub use crate::map::{Map, MapFactory};
    pub use crate::map_config::{MapConfig, UnitConfig};
    pub use crate::map_err::{MapErr, MapErrFactory};
    pub use crate::map_init_err::MapInitErr;
    pub use crate::middleware::{ApplyMiddleware, FnMiddleware};
    pub use crate::then::{Then, ThenFactory};
}
