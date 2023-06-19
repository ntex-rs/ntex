use std::{future::Future, marker, pin::Pin, rc::Rc, task::Context, task::Poll};

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
/// impl<S> Service for Timeout<S>
/// where
///     S: Service,
/// {
///     type Request = S::Request;
///     type Response = S::Response;
///     type Error = TimeoutError<S::Error>;
///     type Future = TimeoutResponse<S>;
///
///     fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
///         ready!(self.service.poll_ready(cx)).map_err(TimeoutError::Service)
///     }
///
///     fn call(&self, req: S::Request) -> Self::Future {
///         TimeoutServiceResponse {
///             fut: self.service.call(req),
///             sleep: Delay::new(clock::now() + self.timeout),
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
/// where
///     S: Service,
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
pub struct ApplyMiddleware<T, S, C>(Rc<(T, S)>, marker::PhantomData<C>);

impl<T, S, C> ApplyMiddleware<T, S, C> {
    /// Create new `ApplyMiddleware` service factory instance
    pub(crate) fn new(mw: T, svc: S) -> Self {
        Self(Rc::new((mw, svc)), marker::PhantomData)
    }
}

impl<T, S, C> Clone for ApplyMiddleware<T, S, C> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), marker::PhantomData)
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
    type Future<'f> = ApplyMiddlewareFuture<'f, T, S, R, C> where Self: 'f, C: 'f;

    #[inline]
    fn create(&self, cfg: C) -> Self::Future<'_> {
        ApplyMiddlewareFuture {
            slf: self.0.clone(),
            fut: self.0 .1.create(cfg),
        }
    }
}

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct ApplyMiddlewareFuture<'f, T, S, R, C>
    where
        S: ServiceFactory<R, C>,
        S: 'f,
        T: Middleware<S::Service>,
        C: 'f,
    {
        slf: Rc<(T, S)>,
        #[pin]
        fut: S::Future<'f>,
    }
}

impl<'f, T, S, R, C> Future for ApplyMiddlewareFuture<'f, T, S, R, C>
where
    S: ServiceFactory<R, C>,
    T: Middleware<S::Service>,
{
    type Output = Result<T::Service, S::InitError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        match this.fut.poll(cx)? {
            Poll::Ready(srv) => Poll::Ready(Ok(this.slf.0.create(srv))),
            Poll::Pending => Poll::Pending,
        }
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
    use std::marker;

    use super::*;
    use crate::{fn_service, Container, Service, ServiceCall, ServiceCtx, ServiceFactory};

    #[derive(Clone)]
    struct Tr<R>(marker::PhantomData<R>);

    impl<S, R> Middleware<S> for Tr<R> {
        type Service = Srv<S, R>;

        fn create(&self, service: S) -> Self::Service {
            Srv(service, marker::PhantomData)
        }
    }

    #[derive(Clone)]
    struct Srv<S, R>(S, marker::PhantomData<R>);

    impl<S: Service<R>, R> Service<R> for Srv<S, R> {
        type Response = S::Response;
        type Error = S::Error;
        type Future<'f> = ServiceCall<'f, S, R> where Self: 'f, R: 'f;

        fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.poll_ready(cx)
        }

        fn call<'a>(&'a self, req: R, ctx: ServiceCtx<'a, Self>) -> Self::Future<'a> {
            ctx.call(&self.0, req)
        }
    }

    #[ntex::test]
    async fn middleware() {
        let factory = apply(
            Rc::new(Tr(marker::PhantomData).clone()),
            fn_service(|i: usize| Ready::<_, ()>::Ok(i * 2)),
        )
        .clone();

        let srv = Container::new(factory.create(&()).await.unwrap().clone());
        let res = srv.call(10).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 20);

        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));

        let res = lazy(|cx| srv.poll_shutdown(cx)).await;
        assert_eq!(res, Poll::Ready(()));

        let factory =
            crate::pipeline_factory(fn_service(|i: usize| Ready::<_, ()>::Ok(i * 2)))
                .apply(Rc::new(Tr(marker::PhantomData).clone()))
                .clone();

        let srv = Container::new(factory.create(&()).await.unwrap().clone());
        let res = srv.call(10).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 20);

        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));

        let res = lazy(|cx| srv.poll_shutdown(cx)).await;
        assert_eq!(res, Poll::Ready(()));
    }
}
