use std::{fmt, marker::PhantomData, rc::Rc};

use crate::dev::{Apply, ApplyCtx, ServiceChainFactory};
use crate::{IntoServiceFactory, Service, ServiceFactory};

/// Apply middleware to a service.
pub fn apply<M, S, R, C, U>(
    mw: M,
    factory: U,
) -> ServiceChainFactory<ApplyMiddleware<M, S, C>, R, C>
where
    S: ServiceFactory<R, C>,
    M: Middleware<S::Service, C>,
    U: IntoServiceFactory<S, R, C>,
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
/// use ntex_service::{Service, ServiceCtx};
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
/// impl<S, R> Service<R> for Timeout<S>
/// where
///     S: Service<R>,
/// {
///     type Response = S::Response;
///     type Error = TimeoutError<S::Error>;
///
///     async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
///         ctx.ready(&self.service).await.map_err(TimeoutError::Service)
///     }
///
///     async fn call(&self, req: R, ctx: ServiceCtx<'_, Self>) -> Result<Self::Response, Self::Error> {
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
    fn create(&self, service: Svc, cfg: Cfg) -> Self::Service;

    /// Creates a service factory that instantiates a service and applies
    /// the current middleware to it.
    ///
    /// This is equivalent to `apply(self, factory)`.
    fn apply<Fac, Req>(
        self,
        factory: Fac,
    ) -> ServiceChainFactory<ApplyMiddleware<Self, Fac, Cfg>, Req, Cfg>
    where
        Fac: ServiceFactory<Req, Cfg, Service = Svc>,
        Cfg: Clone,
        Self: Sized,
        Self::Service: Service<Req>,
    {
        crate::chain_factory(ApplyMiddleware::new(self, factory))
    }
}

impl<M, Svc, Cfg> Middleware<Svc, Cfg> for Rc<M>
where
    M: Middleware<Svc, Cfg>,
{
    type Service = M::Service;

    fn create(&self, service: Svc, cfg: Cfg) -> M::Service {
        self.as_ref().create(service, cfg)
    }
}

/// `Apply` middleware to a service factory.
pub struct ApplyMiddleware<M, Fac, Cfg>(Rc<(M, Fac)>, PhantomData<Cfg>);

impl<M, Fac, Cfg> ApplyMiddleware<M, Fac, Cfg> {
    /// Create new `ApplyMiddleware` service factory instance
    pub(crate) fn new(mw: M, fac: Fac) -> Self {
        Self(Rc::new((mw, fac)), PhantomData)
    }
}

impl<M, Fac, Cfg> Clone for ApplyMiddleware<M, Fac, Cfg> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), PhantomData)
    }
}

impl<M, Fac, Cfg> fmt::Debug for ApplyMiddleware<M, Fac, Cfg>
where
    M: fmt::Debug,
    Fac: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplyMiddleware")
            .field("factory", &self.0.1)
            .field("middleware", &self.0.0)
            .finish()
    }
}

impl<M, Fac, Req, Cfg> ServiceFactory<Req, Cfg> for ApplyMiddleware<M, Fac, Cfg>
where
    Fac: ServiceFactory<Req, Cfg>,
    M: Middleware<Fac::Service, Cfg>,
    M::Service: Service<Req>,
    Cfg: Clone,
{
    type Response = <M::Service as Service<Req>>::Response;
    type Error = <M::Service as Service<Req>>::Error;

    type Service = M::Service;
    type InitError = Fac::InitError;

    #[inline]
    async fn create(&self, cfg: Cfg) -> Result<Self::Service, Self::InitError> {
        Ok(self.0.0.create(self.0.1.create(cfg.clone()).await?, cfg))
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
    fn create(&self, service: S, _: Cfg) -> Self::Service {
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
    C: Clone,
{
    type Service = Outer::Service;

    fn create(&self, service: S, cfg: C) -> Self::Service {
        self.outer
            .create(self.inner.create(service, cfg.clone()), cfg)
    }
}

#[doc(hidden)]
/// Service factory that produces `middleware` from `Fn`.
pub fn fn_layer<T, Req, F, In, Out, Err>(f: F) -> FnMiddleware<T, Req, F, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, T>) -> Result<Out, Err> + Clone,
{
    FnMiddleware { f, r: PhantomData }
}

#[allow(clippy::type_complexity)]
/// `FnMiddleware` service combinator
pub struct FnMiddleware<T, Req, F, In, Out, Err> {
    f: F,
    r: PhantomData<fn(T, Req) -> (In, Out, Err)>,
}

impl<T, Req, F, In, Out, Err> Clone for FnMiddleware<T, Req, F, In, Out, Err>
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

impl<T, Req, F, In, Out, Err> fmt::Debug for FnMiddleware<T, Req, F, In, Out, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnMiddleware")
            .field("layer", &std::any::type_name::<F>())
            .finish()
    }
}

impl<T, C, R, F, In, Out, Err> Middleware<T, C> for FnMiddleware<T, R, F, In, Out, Err>
where
    T: Service<R>,
    F: AsyncFn(In, &ApplyCtx<'_, T>) -> Result<Out, Err> + Clone,
    Err: From<T::Error>,
{
    type Service = Apply<T, R, F, In, Out, Err>;

    fn create(&self, service: T, _: C) -> Self::Service {
        Apply::new(service, self.f.clone())
    }
}

#[cfg(test)]
#[allow(clippy::redundant_clone)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::{Pipeline, ServiceCtx, fn_service};

    #[derive(Debug, Clone)]
    struct Mw<R>(PhantomData<R>, Rc<Cell<usize>>);

    impl<S, R, C> Middleware<S, C> for Mw<R> {
        type Service = Srv<S, R>;

        fn create(&self, service: S, _: C) -> Self::Service {
            self.1.set(self.1.get() + 1);
            Srv(service, PhantomData, self.1.clone())
        }
    }

    #[derive(Debug, Clone)]
    struct Srv<S, R>(S, PhantomData<R>, Rc<Cell<usize>>);

    impl<S: Service<R>, R> Service<R> for Srv<S, R> {
        type Response = S::Response;
        type Error = S::Error;

        async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
            ctx.ready(&self.0).await
        }

        async fn call(
            &self,
            req: R,
            ctx: ServiceCtx<'_, Self>,
        ) -> Result<S::Response, S::Error> {
            ctx.call(&self.0, req).await
        }

        async fn shutdown(&self) {
            self.2.set(self.2.get() + 1);
        }
    }

    #[ntex::test]
    async fn middleware() {
        let cnt_sht = Rc::new(Cell::new(0));
        let factory = apply(
            Rc::new(Mw(PhantomData, cnt_sht.clone()).clone()),
            fn_service(|i: usize| async move { Ok::<_, ()>(i * 2) }),
        )
        .clone();

        let srv = Pipeline::new(factory.create(&()).await.unwrap().clone());
        let res = srv.call(10).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 20);
        let _ = format!("{factory:?} {srv:?}");

        assert_eq!(srv.ready().await, Ok(()));
        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);

        let factory =
            crate::chain_factory(fn_service(|i: usize| async move { Ok::<_, ()>(i * 2) }))
                .apply(Rc::new(Mw(PhantomData, Rc::new(Cell::new(0))).clone()))
                .clone();

        let srv = Pipeline::new(factory.create(&()).await.unwrap().clone());
        let res = srv.call(10).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 20);
        let _ = format!("{factory:?} {srv:?}");

        assert_eq!(srv.ready().await, Ok(()));
    }

    #[ntex::test]
    async fn middleware_apply() {
        let cnt_sht = Rc::new(Cell::new(0));
        let factory = Mw(PhantomData, cnt_sht.clone())
            .apply(fn_service(|i: usize| async move { Ok::<_, ()>(i * 2) }))
            .boxed();

        let srv = factory.pipeline(&()).await.unwrap();
        let res = srv.call(10).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 20);
        let _ = format!("{factory:?} {srv:?}");

        assert_eq!(srv.ready().await, Ok(()));
        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);
    }

    #[ntex::test]
    async fn middleware_chain() {
        let cnt_sht = Rc::new(Cell::new(0));
        let factory =
            crate::chain_factory(fn_service(|i: usize| async move { Ok::<_, ()>(i * 2) }))
                .apply(Mw(PhantomData, cnt_sht.clone()).clone());

        let srv = Pipeline::new(factory.create(&()).await.unwrap().clone());
        let res = srv.call(10).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 20);
        let _ = format!("{factory:?} {srv:?}");

        assert_eq!(srv.ready().await, Ok(()));
        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);
    }

    #[ntex::test]
    async fn stack() {
        let cnt_sht = Rc::new(Cell::new(0));
        let mw = Stack::new(Identity, Mw(PhantomData, cnt_sht.clone()));
        let _ = format!("{mw:?}");

        let pl = Pipeline::new(Middleware::create(
            &mw,
            fn_service(|i: usize| async move { Ok::<_, ()>(i * 2) }),
            (),
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

        let svc = Pipeline::new(
            mw.create(fn_service(async move |i: usize| Ok::<_, ()>(i * 2)), ()),
        );

        let res = svc.call("test").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("test", 2));
        let _ = format!("{svc:?}");

        assert_eq!(svc.ready().await, Ok(()));
        svc.shutdown().await;
        assert_eq!(cnt_sht.get(), 1);
    }
}
