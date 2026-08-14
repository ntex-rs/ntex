//! See [`Service`] docs for information on this crate's foundational trait.
#![deny(clippy::pedantic)]
#![allow(
    clippy::unused_async,
    clippy::missing_fields_in_debug,
    clippy::must_use_candidate,
    clippy::missing_errors_doc
)]
use std::task::Context;

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
mod svc_fct;
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
pub use self::svc_fct::ServiceFactory;

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
///     type Data = ();
///
///     async fn call(&self, req: u8, _: &Self::Data, _: ServiceCtx<'_, Self>) -> Result<Self::Response, Self::Error> {
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

    /// Errors produced by the service when checking readiness or executing call.
    type Error;

    /// Data stored by the pipeline and passed to every service operation.
    type Data;

    /// Processes a request and returns the response asynchronously.
    ///
    /// The `call` method can only be invoked within a pipeline, which enforces
    /// readiness for all services in the pipeline. Implementations of `call`
    /// must not call `ready`; the `ctx` argument ensures the service is ready
    /// before it is invoked.
    async fn call(
        &self,
        req: Req,
        data: &Self::Data,
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
    async fn ready(
        &self,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    #[inline]
    /// Shuts down the service.
    ///
    /// Returns when the service has been properly shut down.
    async fn shutdown(&self, data: &Self::Data) {}

    #[inline]
    /// Polls the service from the current async task.
    ///
    /// The service may perform asynchronous computations or
    /// maintain asynchronous state during polling.
    fn poll(&self, data: &Self::Data, cx: &mut Context<'_>) -> Result<(), Self::Error> {
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

impl<S, Req> Service<Req> for &S
where
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Data = S::Data;

    #[inline]
    async fn ready(
        &self,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<(), S::Error> {
        ctx.ready(&**self, data).await
    }

    #[inline]
    fn poll(&self, data: &Self::Data, cx: &mut Context<'_>) -> Result<(), S::Error> {
        (**self).poll(data, cx)
    }

    #[inline]
    async fn shutdown(&self, data: &Self::Data) {
        (**self).shutdown(data).await;
    }

    #[inline]
    async fn call(
        &self,
        request: Req,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        ctx.call_nowait(&**self, request, data).await
    }
}

impl<S, Req> Service<Req> for Box<S>
where
    S: Service<Req>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Data = S::Data;

    #[inline]
    async fn ready(
        &self,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<(), S::Error> {
        ctx.ready(&**self, data).await
    }

    #[inline]
    async fn shutdown(&self, data: &Self::Data) {
        (**self).shutdown(data).await;
    }

    #[inline]
    async fn call(
        &self,
        request: Req,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        ctx.call_nowait(&**self, request, data).await
    }

    #[inline]
    fn poll(&self, data: &Self::Data, cx: &mut Context<'_>) -> Result<(), S::Error> {
        (**self).poll(data, cx)
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
    pub use crate::apply::{Apply, ApplyCtx, ApplyFactory};
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
