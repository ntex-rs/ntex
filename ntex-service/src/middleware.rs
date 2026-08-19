use std::{fmt, marker::PhantomData, rc::Rc};

use crate::dev::{Apply, ApplyCtx};
use crate::{IntoServiceFactory, Service, ServiceChainFactory, ServiceFactory};

/// Apply middleware to a service.
pub fn apply<Sf, St, Req, M>(
    mw: M,
    factory: impl IntoServiceFactory<Sf, St, Req>,
) -> ServiceChainFactory<ApplyMiddleware<M, Sf>, St, Req>
where
    Sf: ServiceFactory<St, Req>,
    M: Middleware<Sf::Service, Sf::InitCfg>,
{
    ServiceChainFactory {
        factory: ApplyMiddleware::new(mw, factory.into_factory()),
        _t: PhantomData,
    }
}

/// The `Middleware` trait defines the interface for a service factory
/// that wraps an inner service during construction.
///
/// Middleware runs during inbound and/or outbound processing in the
/// request/response lifecycle, and may modify the request and/or response.
///
/// For example, timeout middleware:
///
/// ```rust
/// use ntex_service::{Service, Ctx, ReadyCtx};
/// use ntex::{time::sleep, util::Either, util::select};
///
/// pub struct Timeout<S> {
///     service: S,
///     timeout: std::time::Duration,
/// }
///
/// pub enum TimeoutError<E> {
///    Service(E),
///    Timeout,
/// }
///
/// impl<S> Service for Timeout<S>
/// where
///     S: Service,
/// {
///     type Req = S::Req;
///     type Res = S::Res;
///     type Error = TimeoutError<S::Error>;
///
///     async fn ready(&self, ctx: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
///         ctx.ready(&self.service).await.map_err(TimeoutError::Service)
///     }
///
///     async fn call(&self, req: S::Req, ctx: Ctx<'_, Self>) -> Result<Self::Res, Self::Error> {
///         match select(sleep(self.timeout), ctx.call(&self.service, req)).await {
///             Either::Left(_) => Err(TimeoutError::Timeout),
///             Either::Right(res) => res.map_err(TimeoutError::Service),
///         }
///     }
/// }
/// ```
///
/// The timeout service in the example above is decoupled from the underlying
/// service implementation and can be applied to any service.
///
/// The `Middleware` trait defines the interface for a middleware factory,
/// specifying how to construct a middleware `Service`. A service constructed
/// by the factory takes the following service in the execution chain as a
/// parameter, assuming ownership of that service.
///
/// Factory for `Timeout` middleware from the above example could look like this:
///
/// ```rust,ignore
/// pub struct TimeoutMiddleware {
///     timeout: std::time::Duration,
/// }
///
/// impl<S> Middleware<S> for TimeoutMiddleware
/// {
///     type Service = Timeout<S>;
///
///     fn create(&self, service: S) -> Self::Service {
///         Timeout {
///             service,
///             timeout: self.timeout,
///         }
///     }
/// }
/// ```
pub trait Middleware<Svc, Cfg = ()> {
    /// The middleware `Service` value created by this factory
    type Service;

    /// Creates and returns a new middleware service.
    fn create(&self, service: Svc, cfg: &Cfg) -> Self::Service;

    /// Creates a service factory that instantiates a service and applies
    /// the current middleware to it.
    ///
    /// This is equivalent to `apply(self, factory)`.
    fn apply<Sf, St, Req>(
        self,
        factory: Sf,
    ) -> ServiceChainFactory<ApplyMiddleware<Self, Sf>, St, Req>
    where
        Sf: ServiceFactory<St, Req, Service = Svc, InitCfg = Cfg>,
        Self: Sized,
        Self::Service: Service<St, Req = Req>,
    {
        crate::factory_with_st(ApplyMiddleware::new(self, factory))
    }
}

impl<M, S, Cfg> Middleware<S, Cfg> for Rc<M>
where
    M: Middleware<S, Cfg>,
{
    type Service = M::Service;

    fn create(&self, service: S, cfg: &Cfg) -> M::Service {
        self.as_ref().create(service, cfg)
    }
}

/// `Apply` middleware to a service factory.
pub struct ApplyMiddleware<M, Sf>(Rc<(M, Sf)>);

impl<M, Sf> ApplyMiddleware<M, Sf> {
    /// Create new `ApplyMiddleware` service factory instance
    pub(crate) fn new(mw: M, sf: Sf) -> Self {
        Self(Rc::new((mw, sf)))
    }
}

impl<M, Sf> Clone for ApplyMiddleware<M, Sf> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<M, Sf> fmt::Debug for ApplyMiddleware<M, Sf>
where
    M: fmt::Debug,
    Sf: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplyMiddleware")
            .field("factory", &self.0.1)
            .field("middleware", &self.0.0)
            .finish()
    }
}

impl<M, Sf, St, Req> ServiceFactory<St, Req> for ApplyMiddleware<M, Sf>
where
    Sf: ServiceFactory<St, Req>,
    M: Middleware<Sf::Service, Sf::InitCfg>,
    M::Service: Service<St, Req = Req>,
{
    type Res = <M::Service as Service<St>>::Res;
    type Error = <M::Service as Service<St>>::Error;

    type Service = M::Service;
    type InitCfg = Sf::InitCfg;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, cfg: &Sf::InitCfg) -> Result<Self::Service, Self::InitError> {
        Ok(self.0.0.create(self.0.1.create(cfg).await?, cfg))
    }
}

/// Identity is a middleware.
///
/// It returns service without modifications.
#[derive(Debug, Clone, Copy)]
pub struct Identity;

impl<S, Cfg> Middleware<S, Cfg> for Identity {
    type Service = S;

    #[inline]
    fn create(&self, service: S, _: &Cfg) -> Self::Service {
        service
    }
}

/// Stack of middlewares.
#[derive(Debug, Clone)]
pub struct Stack<Inner, Outer> {
    inner: Inner,
    outer: Outer,
}

impl<Inner, Outer> Stack<Inner, Outer> {
    pub fn new(inner: Inner, outer: Outer) -> Self {
        Stack { inner, outer }
    }
}

impl<S, Inner, Outer, C> Middleware<S, C> for Stack<Inner, Outer>
where
    Inner: Middleware<S, C>,
    Outer: Middleware<Inner::Service, C>,
{
    type Service = Outer::Service;

    fn create(&self, service: S, cfg: &C) -> Self::Service {
        self.outer.create(self.inner.create(service, cfg), cfg)
    }
}

#[doc(hidden)]
/// Service factory that produces `middleware` from `Fn`.
pub fn fn_layer<S, St, F, In, Out, Err>(f: F) -> FnMiddleware<S, St, F, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, S, St>) -> Result<Out, Err> + Clone,
{
    FnMiddleware { f, r: PhantomData }
}

#[allow(clippy::type_complexity)]
/// `FnMiddleware` service combinator
pub struct FnMiddleware<S, St, F, In, Out, Err> {
    f: F,
    r: PhantomData<fn(S, St) -> (In, Out, Err)>,
}

impl<S, St, F, In, Out, Err> Clone for FnMiddleware<S, St, F, In, Out, Err>
where
    F: Clone,
{
    fn clone(&self) -> Self {
        FnMiddleware {
            f: self.f.clone(),
            r: PhantomData,
        }
    }
}

impl<S, St, F, In, Out, Err> fmt::Debug for FnMiddleware<S, St, F, In, Out, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnMiddleware")
            .field("layer", &std::any::type_name::<F>())
            .finish()
    }
}

impl<S, St, F, In, Out, Err, C> Middleware<S, C> for FnMiddleware<S, St, F, In, Out, Err>
where
    S: Service<St>,
    F: AsyncFn(In, &ApplyCtx<'_, S, St>) -> Result<Out, Err> + Clone,
    Err: From<S::Error>,
{
    type Service = Apply<S, St, F, In, Out, Err>;

    fn create(&self, service: S, _: &C) -> Self::Service {
        Apply::new(service, self.f.clone())
    }
}

#[cfg(test)]
#[allow(clippy::redundant_clone)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::{Ctx, Pipeline, ReadyCtx, factory, fn_service};

    #[derive(Debug, Clone)]
    struct Mw(Rc<Cell<usize>>);

    impl<S, C> Middleware<S, C> for Mw {
        type Service = Srv<S>;

        fn create(&self, service: S, _: &C) -> Self::Service {
            self.0.set(self.0.get() + 1);
            Srv(service, self.0.clone())
        }
    }

    #[derive(Debug, Clone)]
    struct Srv<S>(S, Rc<Cell<usize>>);

    impl<S: Service> Service<()> for Srv<S> {
        type Req = S::Req;
        type Res = S::Res;
        type Error = S::Error;

        async fn ready(&self, ctx: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
            ctx.ready(&self.0).await
        }

        async fn call(&self, req: S::Req, ctx: Ctx<'_, Self>) -> Result<S::Res, S::Error> {
            ctx.call(&self.0, req).await
        }

        async fn shutdown(&self) {
            self.1.set(self.1.get() + 1);
        }
    }

    #[ntex::test]
    async fn middleware() {
        let cnt_sht = Rc::new(Cell::new(0));
        let fac = apply(
            Rc::new(Mw(cnt_sht.clone()).clone()),
            fn_service(|i: usize| async move { Ok::<_, ()>(i * 2) }),
        )
        .clone();

        let srv = Pipeline::new(fac.create(&()).await.unwrap().clone());
        let res = srv.call(10).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 20);
        let _ = format!("{fac:?} {srv:?}");

        assert_eq!(srv.ready().await, Ok(()));
        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);

        let fac = factory(fn_service(|i: usize| async move { Ok::<_, ()>(i * 2) }))
            .apply(Rc::new(Mw(Rc::new(Cell::new(0))).clone()))
            .clone();

        let srv = Pipeline::new(fac.create(&()).await.unwrap().clone());
        let res = srv.call(10).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 20);
        let _ = format!("{fac:?} {srv:?}");

        assert_eq!(srv.ready().await, Ok(()));
    }

    #[ntex::test]
    async fn middleware_apply() {
        let cnt_sht = Rc::new(Cell::new(0));
        let fac = Mw(cnt_sht.clone())
            .apply(factory(async |i: usize| Ok::<_, ()>(i * 2)))
            .boxed();

        let srv = Pipeline::new(fac.create(&()).await.unwrap());
        let res = srv.call(10).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 20);
        let _ = format!("{fac:?} {srv:?}");

        assert_eq!(srv.ready().await, Ok(()));
        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);
    }

    #[ntex::test]
    async fn middleware_chain() {
        let cnt_sht = Rc::new(Cell::new(0));
        let fac = factory(fn_service(async move |i: usize| Ok::<_, ()>(i * 2)))
            .apply(Mw(cnt_sht.clone()).clone());

        let srv = Pipeline::new(fac.create(&()).await.unwrap().clone());
        let res = srv.call(10).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 20);
        let _ = format!("{fac:?} {srv:?}");

        assert_eq!(srv.ready().await, Ok(()));
        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);
    }

    #[ntex::test]
    async fn stack() {
        let cnt_sht = Rc::new(Cell::new(0));
        let mw = Stack::new(Identity, Mw(cnt_sht.clone()));
        let _ = format!("{mw:?}");

        let pl = Pipeline::new(Middleware::create(
            &mw,
            fn_service(|i: usize| async move { Ok::<_, ()>(i * 2) }),
            &(),
        ));
        let res = pl.call(10).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 20);
        assert_eq!(pl.ready().await, Ok(()));
        pl.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);
    }

    #[ntex::test]
    async fn fn_middleware_service() {
        let cnt_sht = Rc::new(Cell::new(0));
        let cnt_sht2 = cnt_sht.clone();
        let mw = fn_layer(async move |req: &'static str, svc| {
            cnt_sht2.set(cnt_sht2.get() + 1);
            let result = svc.call(1).await?;
            Ok::<_, ()>((req, result))
        })
        .clone();
        let _ = format!("{mw:?}");

        let svc =
            Pipeline::new(mw.create(fn_service(async move |i: usize| Ok::<_, ()>(i * 2)), &()));

        let res = svc.call("test").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("test", 2));
        let _ = format!("{svc:?}");

        assert_eq!(svc.ready().await, Ok(()));
        svc.shutdown().await;
        assert_eq!(cnt_sht.get(), 1);
    }
}
