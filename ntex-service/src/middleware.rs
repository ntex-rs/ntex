use std::{fmt, marker::PhantomData, rc::Rc};

use crate::{IntoServiceFactory, Service, ServiceFactory};

/// Apply middleware to a service.
pub fn apply<T, S, R, C, U>(t: T, factory: U) -> ApplyMiddleware<T, S, C>
where
    S: ServiceFactory<R, C>,
    T: Middleware<S::Service>,
    U: IntoServiceFactory<S, R, C>,
{
    ApplyMiddleware::new(t, factory.into_factory())
}

/// The `Middleware` trait defines the interface of a service factory that wraps inner service
/// during construction.
///
/// Middleware wraps inner service and runs during
/// inbound and/or outbound processing in the request/response lifecycle.
/// It may modify request and/or response.
///
/// For example, timeout middleware:
///
/// ```rust,ignore
/// pub struct Timeout<S> {
///     service: S,
///     timeout: Duration,
/// }
///
/// impl<S, R> Service<R> for Timeout<S>
/// where
///     S: Service<R>,
/// {
///     type Response = S::Response;
///     type Error = TimeoutError<S::Error>;
///
///     fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
///         self.service.poll_ready(cx).map_err(TimeoutError::Service)
///     }
///
///     async fn call(&self, req: S::Request) -> Result<Self::Response, Self::Error> {
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
///     timeout: Duration,
/// }
///
/// impl<S> Middleware<S> for TimeoutMiddleware<E>
/// {
///     type Service = Timeout<S>;
///
///     fn create(&self, service: S) -> Self::Service {
///         ok(Timeout {
///             service,
///             timeout: self.timeout,
///         })
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
pub struct ApplyMiddleware<T, S, C>(Rc<(T, S)>, PhantomData<C>);

impl<T, S, C> ApplyMiddleware<T, S, C> {
    /// Create new `ApplyMiddleware` service factory instance
    pub(crate) fn new(mw: T, svc: S) -> Self {
        Self(Rc::new((mw, svc)), PhantomData)
    }
}

impl<T, S, C> Clone for ApplyMiddleware<T, S, C> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), PhantomData)
    }
}

impl<T, S, C> fmt::Debug for ApplyMiddleware<T, S, C>
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

impl<T, S, R, C> ServiceFactory<R, C> for ApplyMiddleware<T, S, C>
where
    S: ServiceFactory<R, C>,
    T: Middleware<S::Service>,
    T::Service: Service<R>,
{
    type Response = <T::Service as Service<R>>::Response;
    type Error = <T::Service as Service<R>>::Error;

    type Service = T::Service;
    type InitError = S::InitError;

    #[inline]
    async fn create(&self, cfg: C) -> Result<Self::Service, Self::InitError> {
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
    use ntex_util::future::{lazy, Ready};
    use std::task::{Context, Poll};

    use super::*;
    use crate::{fn_service, Pipeline, ServiceCtx};

    #[derive(Debug, Clone)]
    struct Tr<R>(PhantomData<R>);

    impl<S, R> Middleware<S> for Tr<R> {
        type Service = Srv<S, R>;

        fn create(&self, service: S) -> Self::Service {
            Srv(service, PhantomData)
        }
    }

    #[derive(Debug, Clone)]
    struct Srv<S, R>(S, PhantomData<R>);

    impl<S: Service<R>, R> Service<R> for Srv<S, R> {
        type Response = S::Response;
        type Error = S::Error;

        fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.poll_ready(cx)
        }

        async fn call(
            &self,
            req: R,
            ctx: ServiceCtx<'_, Self>,
        ) -> Result<S::Response, S::Error> {
            ctx.call(&self.0, req).await
        }
    }

    #[ntex::test]
    async fn middleware() {
        let factory = apply(
            Rc::new(Tr(PhantomData).clone()),
            fn_service(|i: usize| Ready::<_, ()>::Ok(i * 2)),
        )
        .clone();

        let srv = Pipeline::new(factory.create(&()).await.unwrap().clone());
        let res = srv.call(10).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 20);
        format!("{:?} {:?}", factory, srv);

        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));

        let res = lazy(|cx| srv.poll_shutdown(cx)).await;
        assert_eq!(res, Poll::Ready(()));

        let factory =
            crate::chain_factory(fn_service(|i: usize| Ready::<_, ()>::Ok(i * 2)))
                .apply(Rc::new(Tr(PhantomData).clone()))
                .clone();

        let srv = Pipeline::new(factory.create(&()).await.unwrap().clone());
        let res = srv.call(10).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 20);
        format!("{:?} {:?}", factory, srv);

        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));

        let res = lazy(|cx| srv.poll_shutdown(cx)).await;
        assert_eq!(res, Poll::Ready(()));
    }
}
