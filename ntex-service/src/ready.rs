use crate::{
    Ctx, IntoService, IntoServiceFactory, Service, ServiceBinding, ServiceChain,
    ServiceChainFactory, ServiceFactory,
};

/// Wraps a service with independent, notification-invalidated readiness.
///
/// Unlike raw children of combinators, the wrapped service retains its own
/// successful readiness and checks it on each call. Changes to a sibling do not
/// discard this cache. The service must subscribe to all readiness-change
/// sources in [`Service::bind`] and notify when readiness changes.
///
/// Calls do not consume readiness. Cloning the wrapper creates a fresh cache;
/// an [`std::rc::Rc`] around the wrapper shares the service and its cache.
///
/// ```
/// use ntex_service::{Service, fn_service, ready};
///
/// # async fn example() -> Result<(), ()> {
/// let pipeline = ready(fn_service(async |req: u8| Ok::<_, ()>(req)))
///     .and_then(ready(fn_service(async |req: u8| Ok::<_, ()>(req + 1))))
///     .pipeline(());
/// assert_eq!(pipeline.call(1).await?, 2);
/// # Ok(())
/// # }
/// ```
pub fn ready<S, St, Req>(service: impl IntoService<S, St, Req>) -> ServiceChain<Ready<S>, St, Req>
where
    S: Service<St, Req>,
{
    crate::service(Ready {
        service: ServiceBinding::new(service.into_service()),
    })
}

/// Wraps each service created by a factory with independent readiness.
///
/// Every successful creation gets a fresh readiness cache, even if the factory
/// returns shared handles to the same inner service. See [`ready()`].
pub fn ready_factory<Sf, St, Req>(
    factory: impl IntoServiceFactory<Sf, St, Req>,
) -> ServiceChainFactory<ReadyFactory<Sf>, St, Req>
where
    Sf: ServiceFactory<St, Req>,
{
    crate::factory(ReadyFactory {
        factory: factory.into_factory(),
    })
}

/// Service produced by [`ready()`].
///
/// Uses its own readiness cache, independently of the calling scope's cache.
/// Enclosing services must forward [`Service::bind`] to this wrapper.
/// Only the deprecated [`Ctx::call_nowait`] compatibility bypass skips its
/// readiness check.
#[derive(Debug, Clone)]
pub struct Ready<S> {
    service: ServiceBinding<S>,
}

impl<S, St, Req> Service<St, Req> for Ready<S>
where
    S: Service<St, Req>,
{
    type Res = S::Res;
    type Error = S::Error;

    crate::forward_bind!(St, service);

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        self.service.ready(ctx.cast()).await
    }

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        self.service.call(req, ctx.cast()).await
    }

    #[inline]
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        self.service.shutdown(ctx.cast()).await;
    }
}

/// Factory produced by [`ready_factory`].
#[derive(Debug, Clone)]
pub struct ReadyFactory<Sf> {
    factory: Sf,
}

impl<Sf, St, Req> ServiceFactory<St, Req> for ReadyFactory<Sf>
where
    Sf: ServiceFactory<St, Req>,
{
    type Res = Sf::Res;
    type Error = Sf::Error;
    type InitError = Sf::InitError;
    type Service = Ready<Sf::Service>;

    #[inline]
    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        Ok(Ready {
            service: ServiceBinding::new(self.factory.create(st).await?),
        })
    }
}
