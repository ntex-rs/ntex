use std::{fmt, marker::PhantomData};

use crate::and_then::{AndThen, AndThenFactory};
use crate::apply::{Apply, ApplyCtx, ApplyFactory};
use crate::ctx::Ctx;
use crate::fn_ready::FnReadiness;
use crate::fn_shutdown::FnShutdown;
use crate::map::{Map, MapFactory};
use crate::map_err::{MapErr, MapErrFactory};
use crate::map_init_err::MapInitErr;
use crate::middleware::{ApplyMiddleware, Middleware};
use crate::pipeline::Pipeline;
use crate::then::{Then, ThenFactory};
use crate::{IntoService, IntoServiceFactory, Service, ServiceFactory};

/// Starts a [`ServiceChain`] with one service.
///
/// Converts the input through [`IntoService`] and returns a builder for
/// composing calls, mapping results, and adding middleware. This does not
/// create a pipeline or check readiness; use [`Service::pipeline`] to supply
/// state and obtain a callable pipeline.
///
/// # Example
///
/// ```
/// use ntex_service::{Service, fn_service, service};
///
/// # async fn example() -> Result<(), ()> {
/// let pipeline = service(fn_service(async |req: u32| Ok::<_, ()>(req + 1)))
///     .map(|value| value.to_string())
///     .pipeline(());
///
/// assert_eq!(pipeline.call(41).await?, "42");
/// # Ok(())
/// # }
/// ```
pub fn service<S, St, Req>(service: impl IntoService<S, St, Req>) -> ServiceChain<S, St, Req>
where
    S: Service<St, Req>,
{
    ServiceChain {
        service: service.into_service(),
        st: PhantomData,
    }
}

/// Starts a [`ServiceChainFactory`] with one service factory.
///
/// Converts the input through [`IntoServiceFactory`] without creating a
/// service. Combinators configure how services will be created and composed.
/// Use [`ServiceChainFactory::pipeline`] to create a service and bind it to
/// its state, or [`ServiceFactory::create`] to obtain the service alone.
///
/// # Example
///
/// ```
/// use ntex_service::{factory, fn_factory, fn_service};
///
/// # async fn example() -> Result<(), ()> {
/// let factory = factory(fn_factory(async |offset: &u32| {
///     let offset = *offset;
///     Ok::<_, ()>(fn_service(async move |req: u32| Ok::<_, ()>(req + offset)))
/// }))
/// .and_then(fn_factory(async |_: &u32| {
///     Ok::<_, ()>(fn_service(async |value: u32| Ok::<_, ()>(value * 2)))
/// }));
///
/// let pipeline = factory.pipeline(10).await?;
/// assert_eq!(pipeline.call(2).await?, 24);
/// # Ok(())
/// # }
/// ```
pub fn factory<Sf, St, Req>(
    factory: impl IntoServiceFactory<Sf, St, Req>,
) -> ServiceChainFactory<Sf, St, Req>
where
    Sf: ServiceFactory<St, Req>,
{
    ServiceChainFactory {
        factory: factory.into_factory(),
        _t: PhantomData,
    }
}

/// A builder for composing services and combinators into one service.
///
/// Created by [`service()`] and service combinators. Each builder method
/// consumes the chain and returns a new chain containing the added adapter.
/// Use [`Self::and_then`] to process successful responses, [`Self::then`] to
/// handle the complete result, or [`Self::apply_fn`] to wrap a call with
/// asynchronous middleware.
///
/// The chain implements [`Service`] when its enclosed composition does.
/// It owns the enclosed service but does not store pipeline state; supply
/// state through [`Service::pipeline`].
///
/// Binding, calls, readiness, and shutdown are forwarded to the enclosed
/// service. The builder adds no independent readiness cache. Raw children
/// share the enclosing readiness scope; use [`crate::ready()`] around a child
/// that needs its own cache and call-time readiness check.
///
/// Cloning clones the enclosed service. Shared state and readiness depend on
/// that service's clone implementation.
///
/// See the framework guide's [Service Pipelines](https://docs.ntex.rs/2-pipeline.html)
/// chapter for composition and readiness semantics.
pub struct ServiceChain<S, St, Req> {
    service: S,
    st: PhantomData<(St, Req)>,
}

/// A builder for composing service factories and combinators.
///
/// Created by [`factory()`] and factory combinators. Builder methods consume
/// the chain and configure how future services are constructed; they do not
/// create services immediately.
///
/// The chain implements [`ServiceFactory`] by forwarding creation to its
/// enclosed factory. [`Self::pipeline`] additionally stores the supplied state
/// with the created service and binds it for readiness notifications.
///
/// [`Self::map_err`] transforms call and readiness errors from created
/// services, while [`Self::map_init_err`] transforms construction errors.
/// [`Self::apply`] installs a [`Middleware`] wrapper on each created service.
///
/// Cloning clones the enclosed factory without creating a service. Whether
/// the clones share factory state depends on the enclosed implementation.
pub struct ServiceChainFactory<Sf, St, Req> {
    pub(crate) factory: Sf,
    pub(crate) _t: PhantomData<(St, Req)>,
}

impl<S: Service<St, Req>, St, Req> ServiceChain<S, St, Req> {
    /// Calls another service after this service completes successfully.
    ///
    /// The current service's response becomes the next service's request. If
    /// the current service returns an error, the next service is not called.
    /// Otherwise, the next service's result becomes the chain's result.
    ///
    /// Both services must use the same state and error types for the resulting
    /// chain to implement [`Service`]. Binding, readiness, and shutdown cover
    /// both children. See [`AndThen`] for readiness behavior between calls.
    pub fn and_then<Next, F>(self, service: F) -> ServiceChain<AndThen<S, Next>, St, Req>
    where
        Self: Sized,
        F: IntoService<Next, St, S::Res>,
        Next: Service<St, S::Res>,
    {
        ServiceChain {
            service: AndThen::new(self.service, service.into_service()),
            st: PhantomData,
        }
    }

    /// Calls another service after this service completes.
    ///
    /// The next service receives the current service's complete `Result`, so it
    /// can handle either a response or an error.
    ///
    /// Both services must use the same state and error types for the resulting
    /// chain to implement [`Service`]. The next service determines the final
    /// response or error. Errors from the chain's readiness check are returned
    /// directly, not passed to the next service as a request.
    ///
    /// See [`Then`] for an error-recovery example and lifecycle behavior.
    pub fn then<Next, F>(self, service: F) -> ServiceChain<Then<S, Next>, St, Req>
    where
        Self: Sized,
        F: IntoService<Next, St, Result<S::Res, S::Error>>,
        Next: Service<St, Result<S::Res, S::Error>>,
    {
        ServiceChain {
            service: Then::new(self.service, service.into_service()),
            st: PhantomData,
        }
    }

    /// Maps this service's response to a different type.
    ///
    /// Like [`Result::map`], this invokes the synchronous function only for
    /// successful responses. Errors, requests, state, and lifecycle behavior
    /// are unchanged. Use [`Self::and_then`] for asynchronous processing.
    pub fn map<F, Res>(self, f: F) -> ServiceChain<Map<F, S, Res>, St, Req>
    where
        Self: Sized,
        F: Fn(S::Res) -> Res,
    {
        ServiceChain {
            service: Map::new(f, self.service),
            st: PhantomData,
        }
    }

    /// Maps this service's error to a different type.
    ///
    /// The synchronous function transforms errors from both calls and
    /// readiness checks, making this useful for normalizing error types before
    /// composing services. Successful responses, requests, and state are
    /// unchanged. Binding and shutdown are forwarded without modification.
    pub fn map_err<F, Err>(self, f: F) -> ServiceChain<MapErr<F, S, Err>, St, Req>
    where
        Self: Sized,
        F: Fn(S::Error) -> Err,
    {
        ServiceChain {
            service: MapErr::new(f, self.service),
            st: PhantomData,
        }
    }

    /// Adds a custom readiness check to the service chain.
    ///
    /// The callback and the wrapped service's readiness futures are polled
    /// together; the callback does not wait for the service to become ready.
    /// The chain is ready only when both succeed. Requests and responses pass
    /// through unchanged.
    ///
    /// The callback must implement [`Clone`] for the resulting chain to
    /// implement [`Service`]. A successful check can be cached by the enclosing
    /// pipeline, so this is not a per-request callback.
    ///
    /// The callback receives only pipeline state. For scoped readiness and
    /// change notifications, implement [`Service`] directly.
    pub fn readiness<F>(
        self,
        ready: F,
    ) -> ServiceChain<AndThen<S, FnReadiness<F, S::Error>>, St, Req>
    where
        Self: Sized,
        F: AsyncFn(&St) -> Result<(), S::Error>,
    {
        ServiceChain {
            service: AndThen::new(self.service, FnReadiness::new(ready)),
            st: PhantomData,
        }
    }

    /// Adds a callback invoked at most once when the service shuts down.
    ///
    /// The callback receives pipeline state and is polled alongside the
    /// wrapped service's shutdown future, not after it completes. Shutdown
    /// waits for both. Requests and responses are unchanged.
    ///
    /// The callback is consumed when first polled; cancellation does not make
    /// it available for a later shutdown attempt. Dropping the chain alone
    /// does not invoke it.
    pub fn shutdown<F>(self, sh: F) -> ServiceChain<AndThen<S, FnShutdown<F, S::Error>>, St, Req>
    where
        Self: Sized,
        F: AsyncFnOnce(&St),
    {
        ServiceChain {
            service: AndThen::new(self.service, FnShutdown::new(sh)),
            st: PhantomData,
        }
    }

    /// Applies an asynchronous function as middleware to this service.
    ///
    /// This is shorthand for calling [`crate::apply_fn`] on the chained service.
    /// The function receives an input request and an [`ApplyCtx`] for state
    /// access and inner calls. It may change request, response, and error types,
    /// or decide not to call the inner service.
    ///
    /// State is unchanged. Inner readiness errors are converted with [`From`];
    /// errors from [`ApplyCtx::call`] retain their inner type. See
    /// [`crate::apply_fn`] for call-time readiness behavior.
    pub fn apply_fn<F, In, Out, Err>(
        self,
        f: F,
    ) -> ServiceChain<Apply<S, St, Req, F, In, Out, Err>, St, In>
    where
        F: AsyncFn(In, &ApplyCtx<'_, S, St, Req>) -> Result<Out, Err>,
        Err: From<S::Error>,
    {
        crate::apply_fn(self.service, f)
    }
}

impl<S: Service<St, Req>, St, Req> Clone for ServiceChain<S, St, Req>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        ServiceChain {
            service: self.service.clone(),
            st: PhantomData,
        }
    }
}

impl<S: Service<St, Req>, St, Req> fmt::Debug for ServiceChain<S, St, Req>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceChain")
            .field("service", &self.service)
            .finish()
    }
}

impl<S: Service<St, Req>, St, Req> Service<St, Req> for ServiceChain<S, St, Req> {
    type Res = S::Res;
    type Error = S::Error;

    crate::forward_bind!(St, service);

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        self.service.call(req, ctx.cast()).await
    }

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        self.service.ready(ctx.cast()).await
    }

    #[inline]
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        self.service.shutdown(ctx.cast()).await;
    }
}

impl<Sf: ServiceFactory<St, Req>, St, Req> ServiceChainFactory<Sf, St, Req> {
    /// Chains another factory after this factory's services.
    ///
    /// Each created service passes its response to the next service as its request.
    /// If the first service returns an error, the next service is not called.
    ///
    /// Factories use the same state, service-error, and initialization-error
    /// types. They are created sequentially with the same state reference; a
    /// first-factory error prevents creation of the second service.
    /// Initialization errors are returned unchanged. See [`AndThenFactory`].
    pub fn and_then<U>(
        self,
        factory: impl IntoServiceFactory<U, St, Sf::Res>,
    ) -> ServiceChainFactory<AndThenFactory<Sf, U>, St, Req>
    where
        Self: Sized,
        U: ServiceFactory<St, Sf::Res, Error = Sf::Error, InitError = Sf::InitError>,
    {
        ServiceChainFactory {
            factory: AndThenFactory::new(self.factory, factory.into_factory()),
            _t: PhantomData,
        }
    }

    /// Applies middleware to this service factory.
    ///
    /// This is shorthand for calling [`crate::apply`] on the chained factory.
    /// The inner factory creates a service first, then the middleware wraps it
    /// synchronously with the same state reference. Initialization errors
    /// bypass the middleware.
    ///
    /// For the result to implement [`ServiceFactory`], the middleware's service
    /// must accept the same state and request types. It may change response and
    /// service-error types.
    pub fn apply<U>(self, tr: U) -> ServiceChainFactory<ApplyMiddleware<U, Sf>, St, Req>
    where
        U: Middleware<Sf::Service, St>,
    {
        crate::apply(tr, self.factory)
    }

    /// Applies an asynchronous function as middleware to this service factory.
    ///
    /// This is shorthand for calling [`crate::apply_fn_factory`] on the chained
    /// factory. Each successful creation wraps the service with a clone of the
    /// function, which receives an input request and an [`ApplyCtx`].
    ///
    /// Request, response, and service-error types may change; state and
    /// initialization errors remain unchanged. See [`crate::apply_fn`] for
    /// lifecycle forwarding and readiness semantics.
    pub fn apply_fn<F, In, Out, Err>(
        self,
        f: F,
    ) -> ServiceChainFactory<ApplyFactory<F, Sf, St, Req, In, Out, Err>, St, In>
    where
        F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St, Req>) -> Result<Out, Err> + Clone,
        Err: From<Sf::Error>,
    {
        crate::apply_fn_factory(self.factory, f)
    }

    /// Chains a factory whose services receive the preceding service's complete
    /// `Result`.
    ///
    /// The second service can recover from a failed first call or transform a
    /// successful response. Both services use the same state and error types.
    /// Errors from the chain's readiness check are returned directly, rather
    /// than passed to the second service.
    ///
    /// Factories must also share their initialization-error type. Creation is
    /// sequential with the same state reference, and an initialization error
    /// stops construction rather than being passed to the next factory.
    /// See [`ThenFactory`] for an example.
    pub fn then<F, U>(self, factory: F) -> ServiceChainFactory<ThenFactory<Sf, U>, St, Req>
    where
        Self: Sized,
        F: IntoServiceFactory<U, St, Result<Sf::Res, Sf::Error>>,
        U: ServiceFactory<
                St,
                Result<Sf::Res, Sf::Error>,
                Error = Sf::Error,
                InitError = Sf::InitError,
            >,
    {
        ServiceChainFactory {
            factory: ThenFactory::new(self.factory, factory.into_factory()),
            _t: PhantomData,
        }
    }

    /// Maps responses produced by this factory's services.
    ///
    /// Each successful creation clones the synchronous mapping function into
    /// the new service. The function runs only on successful responses, not
    /// during initialization. Request, state, service-error, and
    /// initialization-error types are unchanged. See [`MapFactory`].
    pub fn map<F, Res>(self, f: F) -> ServiceChainFactory<MapFactory<F, Sf, Res>, St, Req>
    where
        Self: Sized,
        F: Fn(Sf::Res) -> Res + Clone,
    {
        ServiceChainFactory {
            factory: MapFactory::new(f, self.factory),
            _t: PhantomData,
        }
    }

    /// Maps errors produced by this factory's services.
    ///
    /// Each successful creation clones the synchronous mapping function into
    /// the new service. It transforms call and readiness errors, leaving
    /// successful responses unchanged. Initialization errors are not mapped;
    /// use [`Self::map_init_err`] for those. See [`MapErrFactory`].
    pub fn map_err<F, E>(self, f: F) -> ServiceChainFactory<MapErrFactory<F, Sf, E>, St, Req>
    where
        Self: Sized,
        F: Fn(Sf::Error) -> E + Clone,
    {
        ServiceChainFactory {
            factory: MapErrFactory::new(f, self.factory),
            _t: PhantomData,
        }
    }

    /// Maps this factory's initialization error.
    ///
    /// The synchronous function runs only when creating the inner service
    /// fails. Successful services are returned unchanged, including their
    /// call and readiness error types. Use [`Self::map_err`] to map those.
    pub fn map_init_err<F, E>(self, f: F) -> ServiceChainFactory<MapInitErr<F, Sf, E>, St, Req>
    where
        Self: Sized,
        F: Fn(Sf::InitError) -> E + Clone,
    {
        ServiceChainFactory {
            factory: MapInitErr::new(f, self.factory),
            _t: PhantomData,
        }
    }

    /// Adds a custom readiness check to each created service.
    ///
    /// Each created service gets a clone of the callback. Its readiness future
    /// is polled alongside the wrapped service's readiness future; the service
    /// is ready only when both succeed. Requests and responses are unchanged.
    ///
    /// The inner factory must have [`std::convert::Infallible`] initialization
    /// errors for this composition to implement [`ServiceFactory`].
    /// Successful readiness can be cached, so the callback need not run for
    /// every request.
    ///
    /// The callback receives only pipeline state. For scoped readiness and
    /// change notifications, implement [`Service`] directly.
    pub fn readiness<F>(
        self,
        ready: F,
    ) -> ServiceChainFactory<AndThenFactory<Sf, FnReadiness<F, Sf::Error>>, St, Req>
    where
        Self: Sized,
        F: AsyncFn(&St) -> Result<(), Sf::Error> + Clone,
    {
        ServiceChainFactory {
            factory: AndThenFactory::new(self.factory, FnReadiness::new(ready)),
            _t: PhantomData,
        }
    }

    /// Adds a shutdown callback to each created service.
    ///
    /// Each service gets its own callback, invoked at most once with pipeline
    /// state. It is polled alongside the inner service's shutdown, and shutdown
    /// waits for both. Cancellation does not restore a consumed callback;
    /// dropping a service alone does not invoke it.
    ///
    /// The inner factory must have [`std::convert::Infallible`] initialization
    /// errors for this composition to implement [`ServiceFactory`].
    pub fn shutdown<F>(
        self,
        sh: F,
    ) -> ServiceChainFactory<AndThenFactory<Sf, FnShutdown<F, Sf::Error>>, St, Req>
    where
        Self: Sized,
        F: AsyncFnOnce(&St) + Clone,
    {
        ServiceChainFactory {
            factory: AndThenFactory::new(self.factory, FnShutdown::new(sh)),
            _t: PhantomData,
        }
    }

    /// Creates a service and wraps it with its state in a [`Pipeline`].
    ///
    /// The factory first borrows `st` for creation, then the pipeline takes
    /// ownership of both state and service and binds readiness notifications.
    /// Creation errors are returned unchanged.
    ///
    /// This does not wait for service readiness. Use [`Pipeline::ready`] to
    /// check it explicitly; [`Pipeline::call`] checks it before dispatch.
    /// Each invocation creates a new service and enclosing pipeline readiness
    /// scope, subject to any sharing introduced by the inner factory.
    pub async fn pipeline(&self, st: St) -> Result<Pipeline<Req, Sf::Res, Sf::Error>, Sf::InitError>
    where
        Sf: 'static,
        St: 'static,
        Req: 'static,
    {
        let svc = self.factory.create(&st).await?;
        Ok(Pipeline::new(st, svc))
    }
}

impl<Sf, St, Req> Clone for ServiceChainFactory<Sf, St, Req>
where
    Sf: Clone,
{
    fn clone(&self) -> Self {
        ServiceChainFactory {
            factory: self.factory.clone(),
            _t: PhantomData,
        }
    }
}

impl<Sf, St, Req> fmt::Debug for ServiceChainFactory<Sf, St, Req>
where
    Sf: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceChainFactory")
            .field("factory", &self.factory)
            .finish()
    }
}

impl<Sf: ServiceFactory<St, Req>, St, Req> ServiceFactory<St, Req>
    for ServiceChainFactory<Sf, St, Req>
{
    type Res = Sf::Res;
    type Error = Sf::Error;

    type Service = Sf::Service;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, st: &St) -> Result<Sf::Service, Sf::InitError> {
        self.factory.create(st).await
    }
}
