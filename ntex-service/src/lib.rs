//! See [`Service`] docs for information on this crate's foundational trait.
#![deny(clippy::pedantic)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::missing_fields_in_debug,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::type_complexity,
    clippy::unused_async,
    clippy::unused_async_trait_impl
)]
use std::rc::Rc;

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
pub mod state;
mod then;
mod util;

pub mod pipeline;
mod pl_factory;
mod pl_nost;

pub use crate::apply::{apply_fn, apply_fn_factory};
pub use crate::chain::{ServiceChain, ServiceChainFactory, factory, factory_with_st, svc};
pub use crate::ctx::Ctx;
pub use crate::fn_service::{fn_factory, fn_factory_with_config, fn_service, fn_service_st};
pub use crate::fn_shutdown::fn_shutdown;
pub use crate::map_config::{map_config, unit_config};
pub use crate::middleware::{Identity, Middleware, Stack, apply, fn_layer};
pub use crate::pipeline::Pipeline;
pub use crate::state::StateMapping;

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
/// # use ntex_service::{Service, Ctx};
///
/// struct MyService;
///
/// impl Service<(), u8> for MyService {
///     type Res = u64;
///     type Error = Infallible;
///
///     async fn call(&self, req: u8, ctx: Ctx<'_, Self>) -> Result<Self::Res, Self::Error> {
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
pub trait Service<St, Req> {
    /// Responses that the service could provide.
    type Res;

    /// Errors produced by the service while checking readiness or executing a call.
    type Error;

    /// Processes a request and asynchronously returns the response.
    ///
    /// The `call` method can only be invoked within a pipeline, which ensures
    /// that all services in the pipeline are ready. Implementations of `call`
    /// must not call `ready`; the `ctx` argument ensures that the service is
    /// ready before it is invoked.
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error>;

    #[inline]
    /// Returns when the service is ready to process requests.
    ///
    /// If the service is at capacity, `ready` will not return immediately. The current
    /// task is notified when the service becomes ready again. This function should
    /// be called while executing on a task.
    ///
    /// **Note:** Pipeline readiness is maintained across all services in the pipeline.
    /// The pipeline can process requests only if every service in the pipeline is ready.
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        Ok(())
    }

    #[inline]
    /// Shuts down the service.
    ///
    /// Returns when the service has been properly shut down.
    async fn shutdown(&self, cfg: Ctx<'_, Self, St>) {}

    #[inline]
    /// Maps this service's output to a different type, returning a new service.
    ///
    /// This is similar to `Option::map` or `Iterator::map`, changing the
    /// output type of the underlying service.
    ///
    /// This function consumes the original service and returns a wrapped version,
    /// following the pattern of standard library `map` methods.
    fn map<F, Res>(self, f: F) -> ServiceChain<dev::Map<F, Self, Res>, St, Req>
    where
        Self: Sized,
        F: Fn(Self::Res) -> Res,
    {
        svc(dev::Map::new(f, self))
    }

    #[inline]
    /// Maps this service's error to a different type, returning a new service.
    ///
    /// This is similar to `Result::map_err`, changing the error type of the
    /// underlying service. It is useful, for example, to ensure multiple
    /// services have the same error type.
    ///
    /// This function consumes the original service and returns a wrapped version.
    fn map_err<F, E>(self, f: F) -> ServiceChain<dev::MapErr<F, Self, E>, St, Req>
    where
        Self: Sized,
        F: Fn(Self::Error) -> E,
    {
        svc(dev::MapErr::new(f, self))
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
pub trait ServiceFactory<St, Req, Cfg = ()> {
    /// Responses given by the created services.
    type Res;

    /// Errors produced by the created services.
    type Error;

    /// The type of `Service` produced by this factory.
    type Service: Service<St, Req, Res = Self::Res, Error = Self::Error>;

    /// Possible errors encountered during service construction.
    type InitError;

    /// Creates a new service asynchronously and returns it.
    async fn create(&self, cfg: &Cfg) -> Result<Self::Service, Self::InitError>;

    #[inline]
    /// Asynchronously creates a new service and wraps it in a container.
    async fn pipeline(
        &self,
        cfg: &Cfg,
    ) -> Result<Pipeline<Req, Self::Res, Self::Error>, Self::InitError>
    where
        Self: 'static,
        St: Default + 'static,
        Req: 'static,
        Cfg: 'static,
    {
        Ok(Pipeline::new(self.create(cfg).await?))
    }

    #[inline]
    /// Returns a new service that maps this service's output to a different type.
    fn map<F, Res>(self, f: F) -> ServiceChainFactory<dev::MapFactory<F, Self, Res>, St, Req, Cfg>
    where
        Self: Sized,
        F: Fn(Self::Res) -> Res + Clone,
    {
        factory_with_st(dev::MapFactory::new(f, self))
    }

    #[inline]
    /// Transforms this service's error into another error,
    /// producing a new service.
    fn map_err<F, E>(
        self,
        f: F,
    ) -> ServiceChainFactory<dev::MapErrFactory<F, Self, E>, St, Req, Cfg>
    where
        Self: Sized,
        F: Fn(Self::Error) -> E + Clone,
    {
        factory_with_st(dev::MapErrFactory::new(f, self))
    }

    #[inline]
    /// Maps this factory's initialization error to a different error,
    /// returning a new service factory.
    fn map_init_err<F, E>(
        self,
        f: F,
    ) -> ServiceChainFactory<dev::MapInitErr<F, Self, E>, St, Req, Cfg>
    where
        Self: Sized,
        F: Fn(Self::InitError) -> E + Clone,
    {
        factory_with_st(dev::MapInitErr::new(f, self))
    }

    /// Creates a boxed service factory.
    fn boxed(
        self,
    ) -> boxed::BoxServiceFactory<St, Req, Self::Res, Self::Error, Cfg, Self::InitError>
    where
        St: 'static,
        Req: 'static,
        Cfg: 'static,
        Self: Sized + 'static,
    {
        boxed::factory(self)
    }
}

impl<S, St, Req> Service<St, Req> for &S
where
    S: Service<St, Req>,
{
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), S::Error> {
        ctx.ready(&**self).await
    }

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, S::Error> {
        ctx.call_nowait(&**self, req).await
    }

    #[inline]
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        ctx.shutdown(&**self).await;
    }
}

impl<S, St, Req> Service<St, Req> for Box<S>
where
    S: Service<St, Req>,
{
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), S::Error> {
        ctx.ready(&**self).await
    }

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, S::Error> {
        ctx.call_nowait(&**self, req).await
    }

    #[inline]
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        ctx.shutdown(&**self).await;
    }
}

impl<Sf, St, Req, Cfg> ServiceFactory<St, Req, Cfg> for Rc<Sf>
where
    Sf: ServiceFactory<St, Req, Cfg>,
{
    type Res = Sf::Res;
    type Error = Sf::Error;
    type Service = Sf::Service;
    type InitError = Sf::InitError;

    async fn create(&self, cfg: &Cfg) -> Result<Self::Service, Self::InitError> {
        self.as_ref().create(cfg).await
    }
}

/// Trait for types that can be converted to a `Service`
pub trait IntoService<S, St, Req>
where
    S: Service<St, Req>,
{
    /// Convert to a `Service`
    fn into_service(self) -> S;
}

/// Trait for types that can be converted to a `ServiceFactory`
pub trait IntoServiceFactory<Sf, St, Req, Cfg = ()>
where
    Sf: ServiceFactory<St, Req, Cfg>,
{
    /// Convert `Self` to a `ServiceFactory`
    fn into_factory(self) -> Sf;
}

impl<S, St, Req> IntoService<S, St, Req> for S
where
    S: Service<St, Req>,
{
    #[inline]
    fn into_service(self) -> S {
        self
    }
}

impl<Sf, St, Req, Cfg> IntoServiceFactory<Sf, St, Req, Cfg> for Sf
where
    Sf: ServiceFactory<St, Req, Cfg>,
{
    #[inline]
    fn into_factory(self) -> Sf {
        self
    }
}

/// Check `Service` type
#[inline(always)]
#[allow(clippy::inline_always)]
pub fn __assert_svc<S, St, Req, Res, Err>(s: S) -> S
where
    S: Service<St, Req, Res = Res, Error = Err>,
{
    s
}

/// Check `ServiceFactory` type
#[inline(always)]
#[allow(clippy::inline_always)]
pub fn __assert_factory<Sf, St, Req, Res, Err, InitCfg, InitErr>(f: Sf) -> Sf
where
    Sf: ServiceFactory<St, Req, InitCfg, Res = Res, Error = Err, InitError = InitErr>,
{
    f
}

pub mod dev {
    pub use crate::and_then::{AndThen, AndThenFactory};
    pub use crate::apply::{Apply, ApplyCtx, ApplyFactory};
    pub use crate::chain::{ServiceChain, ServiceChainFactory};
    pub use crate::fn_service::{FnService, FnServiceConfig, FnServiceFactory, FnServiceNoConfig};
    pub use crate::fn_shutdown::FnShutdown;
    pub use crate::map::{Map, MapFactory};
    pub use crate::map_config::{MapConfig, UnitConfig};
    pub use crate::map_err::{MapErr, MapErrFactory};
    pub use crate::map_init_err::MapInitErr;
    pub use crate::middleware::{ApplyMiddleware, FnMiddleware};
    pub use crate::then::{Then, ThenFactory};
}
