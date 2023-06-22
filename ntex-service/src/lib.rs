//! See [`Service`] docs for information on this crate's foundational trait.

#![deny(rust_2018_idioms, warnings)]

use std::future::Future;
use std::rc::Rc;
use std::task::{self, Context, Poll};

mod and_then;
mod apply;
pub mod boxed;
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

pub use self::apply::{apply_fn, apply_fn_factory};
pub use self::chain::{svc, svc_factory};
pub use self::ctx::{ServiceCall, ServiceCtx};
pub use self::fn_service::{fn_factory, fn_factory_with_config, fn_service};
pub use self::fn_shutdown::fn_shutdown;
pub use self::map_config::{map_config, unit_config};
pub use self::middleware::{apply, Identity, Middleware, Stack};
pub use self::pipeline::{Pipeline, PipelineCall};

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
/// # use std::future::Future;
/// # use std::pin::Pin;
/// #
/// # use ntex_service::{Service, ServiceCtx};
///
/// struct MyService;
///
/// impl Service<u8> for MyService {
///     type Response = u64;
///     type Error = Infallible;
///     type Future<'f> = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;
///
///     fn call<'a>(&'a self, req: u8, _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
///         Box::pin(std::future::ready(Ok(req as u64)))
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
/// Service cannot be called directly, it must be wrapped to an instance of [`Container`] or
/// by using `ctx` argument of the call method in case of chanined services.
///
pub trait Service<Req> {
    /// Responses given by the service.
    type Response;

    /// Errors produced by the service when polling readiness or executing call.
    type Error;

    /// The future response value.
    type Future<'f>: Future<Output = Result<Self::Response, Self::Error>>
    where
        Req: 'f,
        Self: 'f;

    /// Process the request and return the response asynchronously.
    ///
    /// This function is expected to be callable off-task. As such, implementations of `call`
    /// should take care to not call `poll_ready`. Caller of the service verifies readiness,
    /// Only way to make a `call` is to use `ctx` argument, it enforces readiness before calling
    /// service.
    fn call<'a>(&'a self, req: Req, ctx: ServiceCtx<'a, Self>) -> Self::Future<'a>;

    #[inline]
    /// Returns `Ready` when the service is able to process requests.
    ///
    /// If the service is at capacity, then `Pending` is returned and the task is notified when
    /// the service becomes ready again. This function is expected to be called while on a task.
    ///
    /// This is a **best effort** implementation. False positives are permitted. It is permitted for
    /// the service to return `Ready` from a `poll_ready` call and the next invocation of `call`
    /// results in an error.
    ///
    /// # Notes
    ///
    /// 1. `.poll_ready()` might be called on different task from actual service call.
    /// 2. In case of chained services, `.poll_ready()` is called for all services at once.
    /// 3. Every `.call()` in chained services enforces readiness.
    fn poll_ready(&self, cx: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    /// Shutdown service.
    ///
    /// Returns `Ready` when the service is properly shutdowns. This method might be called
    /// after it returns `Ready`.
    fn poll_shutdown(&self, cx: &mut task::Context<'_>) -> Poll<()> {
        Poll::Ready(())
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

    /// The future of the `ServiceFactory` instance.
    type Future<'f>: Future<Output = Result<Self::Service, Self::InitError>>
    where
        Cfg: 'f,
        Self: 'f;

    /// Create and return a new service value asynchronously.
    fn create(&self, cfg: Cfg) -> Self::Future<'_>;

    /// Create and return a new service value asynchronously and wrap into a container
    fn pipeline(&self, cfg: Cfg) -> dev::CreatePipeline<'_, Self, Req, Cfg>
    where
        Self: Sized,
    {
        dev::CreatePipeline::new(self.create(cfg))
    }
}

impl<'a, S, Req> Service<Req> for &'a S
where
    S: Service<Req> + ?Sized,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future<'f> = S::Future<'f> where 'a: 'f, Req: 'f;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        (**self).poll_ready(cx)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        (**self).poll_shutdown(cx)
    }

    #[inline]
    fn call<'s>(&'s self, request: Req, ctx: ServiceCtx<'s, Self>) -> S::Future<'s> {
        ctx.call_nowait(&**self, request)
    }
}

impl<S, Req> Service<Req> for Box<S>
where
    S: Service<Req> + ?Sized,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future<'f> = S::Future<'f> where S: 'f, Req: 'f;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        (**self).poll_ready(cx)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        (**self).poll_shutdown(cx)
    }

    #[inline]
    fn call<'a>(&'a self, request: Req, ctx: ServiceCtx<'a, Self>) -> S::Future<'a> {
        ctx.call_nowait(&**self, request)
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
    type Future<'f> = S::Future<'f> where S: 'f, Cfg: 'f;

    fn create(&self, cfg: Cfg) -> S::Future<'_> {
        self.as_ref().create(cfg)
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
    fn into_service(self) -> Svc {
        self
    }
}

impl<T, Req, Cfg> IntoServiceFactory<T, Req, Cfg> for T
where
    T: ServiceFactory<Req, Cfg>,
{
    fn into_factory(self) -> T {
        self
    }
}

/// Convert object of type `T` to a service `S`
pub fn into_service<Svc, Req, F>(tp: F) -> Svc
where
    Svc: Service<Req>,
    F: IntoService<Svc, Req>,
{
    tp.into_service()
}

pub mod dev {
    pub use crate::and_then::{AndThen, AndThenFactory};
    pub use crate::apply::{Apply, ApplyFactory, ApplyService};
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
    pub use crate::pipeline::CreatePipeline;
    pub use crate::then::{Then, ThenFactory};
}
