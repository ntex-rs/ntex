use std::{fmt, marker::PhantomData, rc::Rc};

use crate::{IntoServiceFactory, Service, ServiceFactory};

/// Apply middleware to a service.
pub fn apply<T, S, MidReq, Req, C, U>(t: T, factory: U) -> ApplyMiddleware<T, S, Req, C>
where
    S: ServiceFactory<Req, C>,
    T: Middleware<S::Service>,
    T::Service: Service<MidReq>,
    U: IntoServiceFactory<S, Req, C>,
{
    ApplyMiddleware::new(t, factory.into_factory())
}

/// The `Middleware` trait defines the interface of a service factory that wraps inner service
/// during construction.
///
/// Middleware wraps inner service and runs during
/// inbound and/or outbound processing in the request/response lifecycle.
/// It may modify or transform the request and/or response.
///
/// For example, timeout middleware:
///
/// ```rust
/// use ntex_service::{Service, ServiceCtx};
/// use ntex_util::{time::sleep, future::Either, future::select};
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
/// Timeout service in above example is decoupled from underlying service implementation
/// and could be applied to any service.
///
/// The `Middleware` trait defines the interface of a middleware factory, defining how to
/// construct a middleware Service. A Service that is constructed by the factory takes
/// the Service that follows it during execution as a parameter, assuming
/// ownership of the next Service.
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
pub trait Middleware<S> {
    /// The middleware `Service` value created by this factory
    type Service;

    /// Creates and returns a new middleware Service
    fn create(&self, service: S) -> Self::Service;
}

impl<T, S> Middleware<S> for Rc<T>
where
    T: Middleware<S>,
{
    type Service = T::Service;

    fn create(&self, service: S) -> T::Service {
        self.as_ref().create(service)
    }
}

/// `Apply` middleware to a service factory.
pub struct ApplyMiddleware<T, S, R, C>(Rc<(T, S)>, PhantomData<(C, R)>);

impl<T, S, R, C> ApplyMiddleware<T, S, R, C> {
    /// Create new `ApplyMiddleware` service factory instance
    pub(crate) fn new(mw: T, svc: S) -> Self {
        Self(Rc::new((mw, svc)), PhantomData)
    }
}

impl<T, S, R, C> Clone for ApplyMiddleware<T, S, R, C> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), PhantomData)
    }
}

impl<T, S, R, C> fmt::Debug for ApplyMiddleware<T, S, R, C>
where
    T: fmt::Debug,
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplyMiddleware")
            .field("service", &self.0 .1)
            .field("middleware", &self.0 .0)
            .finish()
    }
}

impl<Mid, SvcFact, Req, MidReq, Cfg> ServiceFactory<MidReq, Cfg> for ApplyMiddleware<Mid, SvcFact, Req, Cfg>
where
    SvcFact: ServiceFactory<Req, Cfg>,
    Mid: Middleware<SvcFact::Service>,
    Mid::Service: Service<MidReq>,
{
    type Response = <Mid::Service as Service<MidReq>>::Response;
    type Error = <Mid::Service as Service<MidReq>>::Error;

    type Service = Mid::Service;
    type InitError = SvcFact::InitError;

    #[inline]
    async fn create(&self, cfg: Cfg) -> Result<Self::Service, Self::InitError> {
        Ok(self.0 .0.create(self.0 .1.create(cfg).await?))
    }
}

/// Identity is a middleware.
///
/// It returns service without modifications.
#[derive(Debug, Clone, Copy)]
pub struct Identity;

impl<S> Middleware<S> for Identity {
    type Service = S;

    #[inline]
    fn create(&self, service: S) -> Self::Service {
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

impl<S, Inner, Outer> Middleware<S> for Stack<Inner, Outer>
where
    Inner: Middleware<S>,
    Outer: Middleware<Inner::Service>,
{
    type Service = Outer::Service;

    fn create(&self, service: S) -> Self::Service {
        self.outer.create(self.inner.create(service))
    }
}

#[cfg(test)]
#[allow(clippy::redundant_clone)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::{fn_service, Pipeline, ServiceCtx};

    #[derive(Debug, Clone)]
    struct Tr<R>(PhantomData<R>, Rc<Cell<usize>>);

    impl<S, R> Middleware<S> for Tr<R> {
        type Service = Srv<S, R>;

        fn create(&self, service: S) -> Self::Service {
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
            Rc::new(Tr(PhantomData, cnt_sht.clone()).clone()),
            fn_service(|i: usize| async move { Ok::<_, ()>(i * 2) }),
        )
        .clone();

        let srv = Pipeline::new(factory.create(&()).await.unwrap().clone());
        let res = srv.call(10).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 20);
        let _ = format!("{:?} {:?}", factory, srv);

        assert_eq!(srv.ready().await, Ok(()));
        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 1);

        let factory =
            crate::chain_factory(fn_service(|i: usize| async move { Ok::<_, ()>(i * 2) }))
                .apply(Rc::new(Tr(PhantomData, Rc::new(Cell::new(0))).clone()))
                .clone();

        let srv = Pipeline::new(factory.create(&()).await.unwrap().clone());
        let res = srv.call(10).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 20);
        let _ = format!("{:?} {:?}", factory, srv);

        assert_eq!(srv.ready().await, Ok(()));
    }
}
