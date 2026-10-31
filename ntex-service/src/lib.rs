//! Asynchronous services, factories, middleware, and execution pipelines.
//!
//! The [`Service`] trait is the crate's central abstraction. A service
//! asynchronously transforms a request into a response, while
//! [`ServiceFactory`] constructs services and [`Pipeline`] manages readiness,
//! calls, and shutdown.
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
mod bind;
pub mod boxed;
pub mod cfg;
mod chain;
mod ctx;
mod fn_ready;
mod fn_service;
mod fn_shutdown;
mod macros;
mod map;
mod map_err;
mod map_init_err;
mod map_state;
mod middleware;
mod readiness;
mod ready;
mod service_binding;
pub mod state;
mod then;
mod util;

pub mod pipeline;
mod pl_factory;
mod pl_inner;
mod pl_state;

pub use crate::apply::{apply_fn, apply_fn_factory};
pub use crate::bind::BindCtx;
pub use crate::chain::{ServiceChain, ServiceChainFactory, factory, service};
pub use crate::ctx::Ctx;
pub use crate::fn_service::{fn_factory, fn_service, fn_service_st};
pub use crate::map_state::{map_state, map_state_factory};
pub use crate::middleware::{Identity, Middleware, Stack, apply, fn_layer};
pub use crate::pipeline::Pipeline;
pub use crate::readiness::{ReadyCallback, ReadyLink, ReadyNotifier};
pub use crate::ready::{ready, ready_factory};
pub use crate::service_binding::{ServiceBinding, ServiceBindingMiddleware};
pub use crate::state::{RequestState, State};

#[allow(unused_variables)]
/// An asynchronous operation from a request to a response.
///
/// A service receives requests and asynchronously produces responses.
/// Conceptually, it is similar to:
///
/// ```rust,ignore
/// async fn(Request) -> Result<Response, Error>
/// ```
///
/// The request and pipeline-state types are generic parameters. The response
/// and error types are associated types, allowing one service type to implement
/// `Service` for multiple request types.
///
/// Methods take `&self`, so implementations that mutate internal state must use
/// interior mutability such as `Cell`, `RefCell`, or a synchronization
/// primitive when appropriate.
///
/// The same abstraction can represent client- and server-side operations.
/// Services focus on transformation, making them straightforward to test and
/// compose.
///
/// A service call requires a [`Ctx`] and therefore runs through a [`Pipeline`]
/// or from another service. The pipeline coordinates readiness across a
/// composed service chain before dispatching a request.
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
/// Simple services do not need a manual trait implementation. The example
/// above can be expressed with [`fn_service`]:
///
/// ```rust
/// # use std::convert::Infallible;
/// # use ntex_service::{Pipeline, fn_service};
/// #
/// # async fn run() -> Result<(), Infallible> {
/// let service = fn_service(|req: u8| async move {
///     Ok::<_, Infallible>(u64::from(req))
/// });
/// let pipeline = Pipeline::new((), service);
///
/// assert_eq!(pipeline.call(10).await?, 10);
/// # Ok(())
/// # }
/// ```
///
/// # Framework guide
///
/// - [Service Model](https://docs.ntex.rs/1-service.html)
/// - [Service Pipelines](https://docs.ntex.rs/2-pipeline.html)
/// - [Service State](https://docs.ntex.rs/3-state.html)
pub trait Service<St, Req> {
    /// Response produced by the service.
    type Res;

    /// Error produced while checking readiness or processing a request.
    type Error;

    /// Processes a request and asynchronously returns the response.
    ///
    /// The enclosing pipeline checks readiness before invoking this method.
    /// Implementations should not call their own `ready` method. A composed
    /// service can use [`Ctx::call`] to call an inner service using the closest
    /// upstream readiness cache. A service wrapped with [`ready()`] can be
    /// called directly to check its own cache instead.
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error>;

    #[inline]
    /// Initializes the service within a [`Pipeline`].
    ///
    /// Called during pipeline construction or [`Ctx::create`], before runtime
    /// operations on the new service.
    ///
    /// The default is suitable for leaves without notification sources.
    /// Composites must bind every existing child, and services with changing
    /// readiness must subscribe to all their sources here. Lazily created
    /// children must be initialized through [`Ctx::create`], which binds them
    /// explicitly before use.
    /// Register reusable [`ReadyCallback`] handles with [`BindCtx::on_notify`]
    /// to observe notifications reaching the service's scope.
    ///
    /// Binding is synchronous. Shared services can be bound to multiple parents;
    /// implementations must support repeated binding.
    /// [`forward_bind!`] and [`forward_pl_bind!`] handle simple forwarding.
    /// Ordinary readiness checks, calls, and shutdown do not add dependencies.
    fn bind(&self, ctx: BindCtx<'_>) {}

    #[inline]
    /// Waits until the service is ready to process a request.
    ///
    /// If the service is at capacity, the returned future remains pending until
    /// capacity becomes available.
    ///
    /// Pipeline readiness is coordinated across all services in a composed
    /// chain. A request is dispatched only when the chain is ready.
    ///
    /// A [`Pipeline`] or [`ServiceBinding`] caches success until a subscribed
    /// [`ReadyNotifier`] invalidates it; calls do not consume that success.
    /// Subscribe in [`Self::bind`] and notify on every readiness-changing
    /// transition, including consuming or releasing capacity and external
    /// failures, before another caller can use stale cached readiness.
    /// A task wake schedules a pending check but does not invalidate a cached
    /// success. Services must not rely on this method being polled on every
    /// call to drive background progress.
    ///
    /// Readiness checks and their wake sources run on the pipeline's thread.
    /// This local-only requirement does not apply to task wakers supplied to
    /// [`Self::call`] or [`Self::shutdown`].
    ///
    /// Readiness may inspect [`Ctx::st`], but it describes the shared gate, not
    /// a reservation for one request or state value. State-taking pipeline
    /// handles share the same cache; independent gates need separate pipelines
    /// or service bindings.
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        Ok(())
    }

    #[inline]
    /// Shuts down the service.
    ///
    /// Returns when the service has been properly shut down.
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {}

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
        service(dev::Map::new(f, self))
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
        service(dev::MapErr::new(f, self))
    }

    #[inline]
    /// Calls another service after this service completes successfully.
    ///
    /// The first service's response becomes the second service's request. If
    /// the first service returns an error, the second service is not called.
    fn and_then<Next, F>(self, f: F) -> ServiceChain<dev::AndThen<Self, Next>, St, Req>
    where
        Self: Sized,
        Next: Service<St, Self::Res, Error = Self::Error>,
        F: IntoService<Next, St, Self::Res>,
    {
        service(dev::AndThen::new(self, f.into_service()))
    }

    #[inline]
    /// Wraps this service and its state in a [`Pipeline`].
    fn pipeline(self, st: St) -> Pipeline<Req, Self::Res, Self::Error>
    where
        Self: Sized + 'static,
        St: 'static,
        Req: 'static,
    {
        Pipeline::new(st, self)
    }
}

/// A factory for asynchronously creating [`Service`] values.
///
/// This is useful when new `Service`s must be produced dynamically. For example,
/// a TCP server listener accepts new connections, constructs a new `Service` for
/// each connection using the `ServiceFactory` trait, and uses that service to
/// handle inbound requests.
///
/// `St` is the state type shared by the factory and its services.
///
/// Simple factories can often use [`fn_factory`] to reduce boilerplate.
pub trait ServiceFactory<St, Req> {
    /// Response produced by the created services.
    type Res;

    /// Error produced by the created services.
    type Error;

    /// The type of `Service` produced by this factory.
    type Service: Service<St, Req, Res = Self::Res, Error = Self::Error>;

    /// Error that can occur while constructing a service.
    type InitError;

    /// Asynchronously creates a service using the supplied state.
    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError>;

    #[inline]
    /// Creates a service and wraps it with its state in a [`Pipeline`].
    async fn pipeline(
        &self,
        st: St,
    ) -> Result<Pipeline<Req, Self::Res, Self::Error>, Self::InitError>
    where
        Self: 'static,
        St: 'static,
        Req: 'static,
    {
        let svc = self.create(&st).await?;
        Ok(Pipeline::new(st, svc))
    }

    #[inline]
    /// Returns a factory whose services map responses to a different type.
    fn map<F, Res>(self, f: F) -> ServiceChainFactory<dev::MapFactory<F, Self, Res>, St, Req>
    where
        Self: Sized,
        F: Fn(Self::Res) -> Res + Clone,
    {
        factory(dev::MapFactory::new(f, self))
    }

    #[inline]
    /// Returns a factory whose services map errors to a different type.
    fn map_err<F, E>(self, f: F) -> ServiceChainFactory<dev::MapErrFactory<F, Self, E>, St, Req>
    where
        Self: Sized,
        F: Fn(Self::Error) -> E + Clone,
    {
        factory(dev::MapErrFactory::new(f, self))
    }

    #[inline]
    /// Maps this factory's initialization error to a different error,
    /// returning a new service factory.
    fn map_init_err<F, E>(self, f: F) -> ServiceChainFactory<dev::MapInitErr<F, Self, E>, St, Req>
    where
        Self: Sized,
        F: Fn(Self::InitError) -> E + Clone,
    {
        factory(dev::MapInitErr::new(f, self))
    }

    /// Chains another factory after this factory's services.
    ///
    /// Each response from the first service becomes a request to the second
    /// service. The second service is not called when the first returns an
    /// error.
    fn and_then<U, F>(self, f: F) -> ServiceChainFactory<dev::AndThenFactory<Self, U>, St, Req>
    where
        Self: Sized,
        U: ServiceFactory<St, Self::Res, Error = Self::Error, InitError = Self::InitError>,
        F: IntoServiceFactory<U, St, Self::Res>,
    {
        factory(dev::AndThenFactory::new(self, f.into_factory()))
    }

    /// Creates a boxed service factory.
    fn boxed(self) -> boxed::BoxServiceFactory<St, Req, Self::Res, Self::Error, Self::InitError>
    where
        St: 'static,
        Req: 'static,
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

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.bind::<_, St, Req>(&**self);
    }

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), S::Error> {
        Service::ready(&**self, ctx.cast()).await
    }

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, S::Error> {
        Service::call(&**self, req, ctx.cast()).await
    }

    #[inline]
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        Service::shutdown(&**self, ctx.cast()).await;
    }
}

impl<S, St, Req> Service<St, Req> for Box<S>
where
    S: Service<St, Req>,
{
    type Res = S::Res;
    type Error = S::Error;

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.bind::<_, St, Req>(&**self);
    }

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), S::Error> {
        Service::ready(&**self, ctx.cast()).await
    }

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, S::Error> {
        Service::call(&**self, req, ctx.cast()).await
    }

    #[inline]
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        Service::shutdown(&**self, ctx.cast()).await;
    }
}

impl<S, St, Req> Service<St, Req> for Rc<S>
where
    S: Service<St, Req>,
{
    type Res = S::Res;
    type Error = S::Error;

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.bind::<_, St, Req>(&**self);
    }

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), S::Error> {
        Service::ready(&**self, ctx.cast()).await
    }

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, S::Error> {
        Service::call(&**self, req, ctx.cast()).await
    }

    #[inline]
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        Service::shutdown(&**self, ctx.cast()).await;
    }
}

impl<Sf, St, Req> ServiceFactory<St, Req> for Rc<Sf>
where
    Sf: ServiceFactory<St, Req>,
{
    type Res = Sf::Res;
    type Error = Sf::Error;
    type Service = Sf::Service;
    type InitError = Sf::InitError;

    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        self.as_ref().create(st).await
    }
}

/// A common interface for values that can call a service.
pub trait ServiceCaller<Req, Res, Err> {
    /// Waits for readiness, then calls the service.
    async fn call_service(&self, req: Req) -> Result<Res, Err>;
}

/// Conversion into a [`Service`].
pub trait IntoService<S, St, Req>
where
    S: Service<St, Req>,
{
    /// Converts this value into a service.
    fn into_service(self) -> S;
}

/// Conversion into a [`ServiceFactory`].
pub trait IntoServiceFactory<Sf, St, Req>
where
    Sf: ServiceFactory<St, Req>,
{
    /// Converts this value into a service factory.
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

impl<Sf, St, Req> IntoServiceFactory<Sf, St, Req> for Sf
where
    Sf: ServiceFactory<St, Req>,
{
    #[inline]
    fn into_factory(self) -> Sf {
        self
    }
}

/// Combinator and helper types used by the public API.
///
/// Most services can be built through the crate-level functions and the
/// [`Service`] and [`ServiceFactory`] methods without naming these types.
/// This module exposes their concrete return types for type annotations,
/// reusable wrappers, and custom composition.
///
/// - [`ServiceChain`] and [`ServiceChainFactory`] are builders returned by
///   [`service()`] and [`factory()`].
/// - [`dev::FnService`], [`dev::FnServiceSt`], and [`dev::FnFactory`] adapt
///   functions into services and factories.
/// - [`dev::AndThen`], [`dev::Then`], [`dev::Map`], and [`dev::MapErr`] compose
///   or transform service results, with corresponding factory adapters.
/// - [`dev::ApplyCtx`] provides state and inner-service calls to
///   [`apply_fn`] middleware; [`dev::MapState`] substitutes the inner state.
/// - [`dev::Ready`] and [`dev::ReadyFactory`] are the wrappers produced by
///   [`ready()`] and [`ready_factory()`] for independent cached readiness.
///
/// # Framework guide
///
/// - [Service Pipelines](https://docs.ntex.rs/2-pipeline.html)
/// - [Service State](https://docs.ntex.rs/3-state.html)
pub mod dev {
    pub use crate::and_then::{AndThen, AndThenFactory};
    pub use crate::apply::{Apply, ApplyCtx, ApplyFactory};
    pub use crate::chain::{ServiceChain, ServiceChainFactory};
    pub use crate::fn_ready::FnReadiness;
    pub use crate::fn_service::{FnFactory, FnService, FnServiceSt, FnServiceStFactory};
    pub use crate::fn_shutdown::FnShutdown;
    pub use crate::map::{Map, MapFactory};
    pub use crate::map_err::{MapErr, MapErrFactory};
    pub use crate::map_init_err::MapInitErr;
    pub use crate::map_state::{MapState, MapStateFactory};
    pub use crate::middleware::{ApplyMiddleware, FnMiddleware};
    pub use crate::ready::{Ready, ReadyFactory};
    pub use crate::then::{Then, ThenFactory};
}

#[cfg(test)]
#[path = "../tests/unit/lib.rs"]
mod tests;
