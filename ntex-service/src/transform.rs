use std::{future::Future, marker, pin::Pin, rc::Rc, task::Context, task::Poll};

use crate::{IntoServiceFactory, Service, ServiceFactory};

/// Apply middleware to a service.
pub fn apply<T, S, R, C, U>(t: T, factory: U) -> ApplyMiddleware<T, S, R, C>
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
pub struct ApplyMiddleware<T, S, R, C>(Rc<(T, S)>, marker::PhantomData<(R, C)>);

impl<T, S, R, C> ApplyMiddleware<T, S, R, C>
where
    S: ServiceFactory<R, C>,
    T: Middleware<S::Service>,
{
    /// Create new `ApplyMiddleware` service factory instance
    pub(crate) fn new(t: T, service: S) -> Self {
        Self(Rc::new((t, service)), marker::PhantomData)
    }
}

impl<T, S, R, C> Clone for ApplyMiddleware<T, S, R, C> {
    fn clone(&self) -> Self {
        ApplyMiddleware(self.0.clone(), marker::PhantomData)
    }
}

impl<T, S, R, C> ServiceFactory<R, C> for ApplyMiddleware<T, S, R, C>
where
    S: ServiceFactory<R, C>,
    T: Middleware<S::Service>,
    T::Service: Service<R>,
{
    type Response = <T::Service as Service<R>>::Response;
    type Error = <T::Service as Service<R>>::Error;

    type Service = T::Service;
    type InitError = S::InitError;
    type Future = ApplyMiddlewareFuture<T, S, R, C>;

    fn new_service(&self, cfg: C) -> Self::Future {
        ApplyMiddlewareFuture {
            store: self.0.clone(),
            fut: self.0.as_ref().1.new_service(cfg),
            _t: marker::PhantomData,
        }
    }
}

pin_project_lite::pin_project! {
    pub struct ApplyMiddlewareFuture<T, S, R, C>
    where
        S: ServiceFactory<R, C>,
        T: Middleware<S::Service>,
    {
        store: Rc<(T, S)>,
        #[pin]
        fut: S::Future,
        _t: marker::PhantomData<C>
    }
}

impl<T, S, R, C> Future for ApplyMiddlewareFuture<T, S, R, C>
where
    S: ServiceFactory<R, C>,
    T: Middleware<S::Service>,
{
    type Output = Result<T::Service, S::InitError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        match this.fut.poll(cx)? {
            Poll::Ready(srv) => Poll::Ready(Ok(this.store.0.create(srv))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Identity is a transform.
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

#[cfg(test)]
#[allow(clippy::redundant_clone)]
mod tests {
    use ntex_util::future::{lazy, Ready};

    use super::*;
    use crate::{fn_service, Service, ServiceFactory};

    #[derive(Clone)]
    struct Tr<R>(marker::PhantomData<R>);

    impl<S, R> Transform<S> for Tr<R> {
        type Service = Srv<S, R>;

        fn new_transform(&self, service: S) -> Self::Service {
            Srv(service, marker::PhantomData)
        }
    }

    #[derive(Clone)]
    struct Srv<S, R>(S, marker::PhantomData<R>);

    impl<S: Service<R>, R> Service<R> for Srv<S, R> {
        type Response = S::Response;
        type Error = S::Error;
        type Future = S::Future;

        fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.poll_ready(cx)
        }

        fn call(&self, req: R) -> Self::Future {
            self.0.call(req)
        }
    }

    #[ntex::test]
    async fn transform() {
        let factory = apply(
            Rc::new(Tr(marker::PhantomData).clone()),
            fn_service(|i: usize| Ready::<_, ()>::Ok(i * 2)),
        )
        .clone();

        let srv = factory.new_service(()).await.unwrap();
        let res = srv.call(10).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 20);

        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));

        let res = lazy(|cx| srv.poll_shutdown(cx, true)).await;
        assert_eq!(res, Poll::Ready(()));
    }
}
