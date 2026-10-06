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
pub mod state;
mod then;
mod util;

pub mod pipeline;
mod pl_factory;
mod pl_inner;
mod pl_state;

pub use crate::apply::{apply_fn, apply_fn_factory};
pub use crate::chain::{ServiceChain, ServiceChainFactory, factory, service};
pub use crate::ctx::Ctx;
pub use crate::fn_service::{fn_factory, fn_service, fn_service_st};
pub use crate::map_state::{map_state, map_state_factory};
pub use crate::middleware::{Identity, Middleware, Stack, apply, fn_layer};
pub use crate::pipeline::Pipeline;
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
pub trait Service<St, Req> {
    /// Response produced by the service.
    type Res;

    /// Error produced while checking readiness or processing a request.
    type Error;

    /// Processes a request and asynchronously returns the response.
    ///
    /// The enclosing pipeline checks readiness before invoking this method.
    /// Implementations should not call their own `ready` method. A composed
    /// service can use `ctx` to call an inner service.
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error>;

    #[inline]
    /// Waits until the service is ready to process a request.
    ///
    /// If the service is at capacity, the returned future remains pending until
    /// capacity becomes available.
    ///
    /// Pipeline readiness is coordinated across all services in a composed
    /// chain. A request is dispatched only when the chain is ready.
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

impl<S, St, Req> Service<St, Req> for Rc<S>
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
    pub use crate::then::{Then, ThenFactory};
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, task::Poll};

    use ntex::util::lazy;

    use super::*;
    use crate::dev::FnServiceSt;
    use crate::pipeline::PipelineFactory;

    #[derive(Clone, Debug, Default)]
    struct Srv(Rc<Cell<usize>>);

    impl Service<usize, usize> for Srv {
        type Res = usize;
        type Error = &'static str;

        async fn ready(&self, ctx: Ctx<'_, Self, usize>) -> Result<(), Self::Error> {
            // waker of the current readiness owner is available
            assert!(ctx.poll_once(|_| true));
            self.0.set(self.0.get() + 1);
            Ok(())
        }

        async fn call(&self, req: usize, ctx: Ctx<'_, Self, usize>) -> Result<usize, Self::Error> {
            let one = ctx.poll_once(|_| 1);
            let two = ctx.poll_fn(|_| Poll::Ready(2)).await;
            assert_eq!(one + two, 3);

            if req == 0 { Err("zero") } else { Ok(req + *ctx) }
        }

        async fn shutdown(&self, _: Ctx<'_, Self, usize>) {
            self.0.set(self.0.get() + 100);
        }
    }

    struct Fwd {
        inner: Srv,
        pl: Pipeline<usize, usize, &'static str>,
    }

    impl Service<usize, usize> for Fwd {
        type Res = usize;
        type Error = usize;

        crate::forward_ready!(usize, inner, str::len);

        async fn call(&self, req: usize, ctx: Ctx<'_, Self, usize>) -> Result<usize, usize> {
            let res = ctx.call(&self.inner, req).await.map_err(str::len)?;
            self.pl.call(res).await.map_err(str::len)
        }

        crate::forward_shutdown!(usize, inner);
    }

    struct FwdPl {
        pl: Pipeline<usize, usize, &'static str>,
    }

    impl Service<usize, usize> for FwdPl {
        type Res = usize;
        type Error = &'static str;

        crate::forward_pl_ready!(usize, pl);
        crate::forward_pl_shutdown!(usize, pl);

        async fn call(&self, req: usize, _: Ctx<'_, Self, usize>) -> Result<usize, &'static str> {
            self.pl.call(req).await
        }
    }

    struct FwdPlErr {
        pl: Pipeline<usize, usize, &'static str>,
    }

    impl Service<usize, usize> for FwdPlErr {
        type Res = usize;
        type Error = usize;

        crate::forward_pl_ready!(usize, pl, str::len);

        async fn call(&self, req: usize, _: Ctx<'_, Self, usize>) -> Result<usize, usize> {
            self.pl.call(req).await.map_err(str::len)
        }
    }

    #[ntex::test]
    async fn service_wrappers() {
        let cnt = Rc::new(Cell::new(0));

        let srv: &'static Srv = Box::leak(Box::new(Srv(cnt.clone())));
        let pl = Pipeline::new(1, srv);
        assert_eq!(pl.call(1).await, Ok(2));
        assert_eq!(pl.call(0).await, Err("zero"));
        pl.shutdown().await;
        assert_eq!(cnt.get(), 102);

        let pl = Pipeline::new(2, Box::new(Srv(cnt.clone())));
        assert_eq!(pl.call(1).await, Ok(3));
        pl.shutdown().await;
        assert_eq!(cnt.get(), 203);

        let pl = Pipeline::new(3, Rc::new(Srv(cnt.clone())));
        assert_eq!(pl.call(1).await, Ok(4));
        pl.shutdown().await;
        assert_eq!(cnt.get(), 304);

        let pl = Pipeline::new(
            1,
            Fwd {
                inner: Srv(cnt.clone()),
                pl: Pipeline::new(10, Srv(cnt.clone())),
            },
        );
        assert_eq!(pl.call(1).await, Ok(12));
        assert_eq!(pl.call(0).await, Err(4));
        pl.shutdown().await;
        assert_eq!(cnt.get(), 409);

        let pl = Pipeline::new(
            1,
            FwdPl {
                pl: Pipeline::new(10, Srv(cnt.clone())),
            },
        );
        assert_eq!(pl.call(1).await, Ok(11));
        pl.shutdown().await;
        // inner pipeline readiness is checked once, by `forward_pl_ready!`
        assert_eq!(cnt.get(), 510);

        let pl = Pipeline::new(
            1,
            FwdPlErr {
                pl: Pipeline::new(10, Srv(cnt.clone())),
            },
        );
        assert_eq!(pl.call(0).await, Err(4));
        pl.shutdown().await;
        assert_eq!(cnt.get(), 511);
    }

    #[ntex::test]
    async fn pipeline_calls() {
        let cnt = Rc::new(Cell::new(0));
        let pl = Srv(cnt.clone()).pipeline(1);

        assert_eq!(pl.call_static(1).await, Ok(2));
        assert_eq!(cnt.get(), 1);

        // a successful readiness check is consumed by the next call only
        assert_eq!(pl.ready().await, Ok(()));
        assert_eq!(cnt.get(), 2);
        assert_eq!(pl.call(2).await, Ok(3));
        assert_eq!(cnt.get(), 2);
        assert_eq!(ServiceCaller::call_service(&pl, 3).await, Ok(4));
        assert_eq!(cnt.get(), 3);

        assert_eq!(lazy(|cx| pl.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert_eq!(cnt.get(), 4);
        assert_eq!(pl.call_static(3).await, Ok(4));
        assert_eq!(pl.call_static(3).await, Ok(4));
        assert_eq!(cnt.get(), 5);

        let b = pl.bind();
        assert!(format!("{b:?}").contains("PipelineBinding"));
        assert_eq!(b.call_static(4).await, Ok(5));
        assert_eq!(cnt.get(), 6);
        assert_eq!(b.ready().await, Ok(()));
        assert_eq!(cnt.get(), 7);
        assert_eq!(pl.call(4).await, Ok(5));
        assert_eq!(b.call(4).await, Ok(5));
        assert_eq!(cnt.get(), 8);

        // the flag is decided on the first poll, not on creation
        assert_eq!(pl.ready().await, Ok(()));
        let fut1 = pl.call_static(1);
        let fut2 = pl.call_static(2);
        assert_eq!(fut2.await, Ok(3));
        assert_eq!(cnt.get(), 9);
        assert_eq!(fut1.await, Ok(2));
        assert_eq!(cnt.get(), 10);

        // shutdown resets the flag
        assert_eq!(pl.ready().await, Ok(()));
        assert_eq!(cnt.get(), 11);
        pl.shutdown().await;
        assert_eq!(pl.call(1).await, Ok(2));
        assert_eq!(cnt.get(), 112);

        let svc = apply_fn(
            Srv(cnt.clone()),
            async |req: usize, svc: &dev::ApplyCtx<'_, Srv, usize, usize>| {
                svc.call_service(req * 2).await
            },
        );
        let pl = Pipeline::new(1, svc);
        assert_eq!(pl.call(2).await, Ok(5));
        assert_eq!(cnt.get(), 115);
    }

    #[ntex::test]
    async fn fn_conversions() {
        let pl = Pipeline::new(5, async |st: &usize, req: usize| Ok::<_, ()>(st + req));
        assert_eq!(pl.call(1).await, Ok(6));

        let _: FnServiceSt<_, usize, usize, usize, ()> =
            IntoService::into_service(async |st: &usize, req: usize| Ok::<_, ()>(st + req));

        let f = factory(async |st: &usize| Ok::<_, ()>(Srv(Rc::new(Cell::new(*st)))));
        let pl = f.pipeline(1).await.unwrap();
        assert_eq!(pl.call(1).await, Ok(2));
    }

    #[ntex::test]
    async fn factory_combinators() {
        let cnt = Rc::new(Cell::new(0));
        let c = cnt.clone();
        let f = fn_factory(async move |st: &usize| {
            if *st == 0 {
                Err(())
            } else {
                Ok::<_, ()>(Srv(c.clone()))
            }
        });

        let rc = Rc::new(f.clone());
        let pl = rc.pipeline(1).await.unwrap();
        assert_eq!(pl.call(1).await, Ok(2));
        assert!(rc.pipeline(0).await.is_err());

        let f2 = f.clone().map_init_err(|()| "init");
        assert_eq!(f2.create(&0).await.err(), Some("init"));

        let f2 = f.clone().and_then(f.clone());
        let pl = f2.pipeline(1).await.unwrap();
        assert_eq!(pl.call(1).await, Ok(3));

        let f2 = factory(f.clone()).map(|r| r * 10).map_err(|_| ());
        let pl = f2.pipeline(1).await.unwrap();
        assert_eq!(pl.call(1).await, Ok(20));
        assert_eq!(pl.call(0).await, Err(()));

        let pf = PipelineFactory::new(f.clone());
        let pf2 = pf.clone();
        assert!(format!("{pf2:?}").contains("PipelineFactory"));
        let pl = pf2.create(2).await.unwrap();
        assert_eq!(pl.call(1).await, Ok(3));
        assert!(pf.create(0).await.is_err());
    }

    #[ntex::test]
    async fn boxed_and_debug() {
        let cnt = Rc::new(Cell::new(0));

        let svc = boxed::service(Srv(cnt.clone()));
        let pl = Pipeline::new(1, svc.clone());
        assert_eq!(pl.call(1).await, Ok(2));

        let f = boxed::factory(fn_factory(async |_: &usize| Ok::<_, ()>(Srv::default())));
        let pl = f.clone().pipeline(1).await.unwrap();
        assert_eq!(pl.call(1).await, Ok(2));

        let s = format!("{:?}", service(Srv::default()).map(|r| r).map_err(|e| e));
        assert!(s.contains("Map") && s.contains("MapErr"));
    }

    #[test]
    fn request_state() {
        let st = State {
            req: 1,
            state: "st",
        };
        assert_eq!(st.unpack(), ("st", 1));
        assert_eq!(("st", 2).unpack(), ("st", 2));
    }
}
