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
pub struct ServiceChain<S, St, Req> {
    service: S,
    st: PhantomData<(St, Req)>,
}

/// A builder for composing service factories and combinators.
pub struct ServiceChainFactory<Sf, St, Req> {
    pub(crate) factory: Sf,
    pub(crate) _t: PhantomData<(St, Req)>,
}

impl<S: Service<St, Req>, St, Req> ServiceChain<S, St, Req> {
    /// Calls another service after this service completes successfully.
    ///
    /// The current service's response becomes the next service's request. If
    /// the current service returns an error, the next service is not called.
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
    /// This is analogous to [`Option::map`] or [`Result::map`].
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
    /// This is analogous to [`Result::map_err`] and is useful for normalizing
    /// error types across composed services.
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

    /// Adds a callback that runs once when the service shuts down.
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

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        ctx.call(&self.service, req).await
    }

    crate::forward_ready!(St, service);
    crate::forward_shutdown!(St, service);
}

impl<Sf: ServiceFactory<St, Req>, St, Req> ServiceChainFactory<Sf, St, Req> {
    /// Chains another factory after this factory's services.
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
    pub fn apply<U>(self, tr: U) -> ServiceChainFactory<ApplyMiddleware<U, Sf>, St, Req>
    where
        U: Middleware<Sf::Service, St>,
    {
        crate::apply(tr, self.factory)
    }

    /// Applies an asynchronous function as middleware to this service factory.
    ///
    /// This is shorthand for calling [`crate::apply_fn_factory`] on the chained
    /// factory.
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
