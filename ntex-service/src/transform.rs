use std::{future::Future, pin::Pin, rc::Rc, task::Context, task::Poll};

use crate::{IntoServiceFactory, Service, ServiceFactory};

/// Apply transform to a service.
pub fn apply<T, S, U>(t: T, factory: U) -> ApplyTransform<T, S>
where
    S: ServiceFactory,
    T: Transform<S::Service>,
    U: IntoServiceFactory<S>,
{
    ApplyTransform::new(t, factory.into_factory())
}

/// The `Transform` trait defines the interface of a service factory that wraps inner service
/// during construction.
///
/// Transform(middleware) wraps inner service and runs during
/// inbound and/or outbound processing in the request/response lifecycle.
/// It may modify request and/or response.
///
/// For example, timeout transform:
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
///     type Future = TimeoutServiceResponse<S>;
///
///     fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
///         ready!(self.service.poll_ready(cx)).map_err(TimeoutError::Service)
///     }
///
///     fn call(&mut self, req: S::Request) -> Self::Future {
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
/// The `Transform` trait defines the interface of a Service factory. `Transform`
/// is often implemented for middleware, defining how to construct a
/// middleware Service. A Service that is constructed by the factory takes
/// the Service that follows it during execution as a parameter, assuming
/// ownership of the next Service.
///
/// Factory for `Timeout` middleware from the above example could look like this:
///
/// ```rust,ignore
/// pub struct TimeoutTransform {
///     timeout: Duration,
/// }
///
/// impl<S> Transform<S> for TimeoutTransform<E>
/// where
///     S: Service,
/// {
///     type Transform = Timeout<S>;
///
///     fn new_transform(&self, service: S) -> Self::Transform {
///         ok(TimeoutService {
///             service,
///             timeout: self.timeout,
///         })
///     }
/// }
/// ```
pub trait Transform<S> {
    /// The `TransformService` value created by this factory
    type Transform: Service;

    /// Creates and returns a new Transform component, asynchronously
    fn new_transform(&self, service: S) -> Self::Transform;
}

impl<T, S> Transform<S> for Rc<T>
where
    T: Transform<S>,
{
    type Transform = T::Transform;

    fn new_transform(&self, service: S) -> T::Transform {
        self.as_ref().new_transform(service)
    }
}

/// `Apply` transform to new service
pub struct ApplyTransform<T, S>(Rc<(T, S)>);

impl<T, S> ApplyTransform<T, S>
where
    S: ServiceFactory,
    T: Transform<S::Service>,
{
    /// Create new `ApplyTransform` new service instance
    pub(crate) fn new(t: T, service: S) -> Self {
        Self(Rc::new((t, service)))
    }
}

impl<T, S> Clone for ApplyTransform<T, S> {
    fn clone(&self) -> Self {
        ApplyTransform(self.0.clone())
    }
}

impl<T, S> ServiceFactory for ApplyTransform<T, S>
where
    S: ServiceFactory,
    T: Transform<S::Service>,
{
    type Request = <T::Transform as Service>::Request;
    type Response = <T::Transform as Service>::Response;
    type Error = <T::Transform as Service>::Error;

    type Config = S::Config;
    type Service = T::Transform;
    type InitError = S::InitError;
    type Future = ApplyTransformFuture<T, S>;

    fn new_service(&self, cfg: S::Config) -> Self::Future {
        ApplyTransformFuture {
            store: self.0.clone(),
            fut: self.0.as_ref().1.new_service(cfg),
        }
    }
}

pin_project_lite::pin_project! {
    pub struct ApplyTransformFuture<T, S>
    where
        S: ServiceFactory,
        T: Transform<S::Service>,
    {
        store: Rc<(T, S)>,
        #[pin]
        fut: S::Future,
    }
}

impl<T, S> Future for ApplyTransformFuture<T, S>
where
    S: ServiceFactory,
    T: Transform<S::Service>,
{
    type Output = Result<T::Transform, S::InitError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        match this.fut.poll(cx)? {
            Poll::Ready(srv) => Poll::Ready(Ok(this.store.0.new_transform(srv))),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
#[allow(clippy::redundant_clone)]
mod tests {
    use ntex_util::future::{lazy, Ready};

    use super::*;
    use crate::{fn_service, Service, ServiceFactory};

    #[derive(Clone)]
    struct Tr;

    impl<S: Service> Transform<S> for Tr {
        type Transform = Srv<S>;

        fn new_transform(&self, service: S) -> Self::Transform {
            Srv(service)
        }
    }

    #[derive(Clone)]
    struct Srv<S>(S);

    impl<S: Service> Service for Srv<S> {
        type Request = S::Request;
        type Response = S::Response;
        type Error = S::Error;
        type Future = S::Future;

        fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.poll_ready(cx)
        }

        fn call(&self, req: S::Request) -> Self::Future {
            self.0.call(req)
        }
    }

    #[ntex::test]
    async fn transform() {
        let factory = apply(
            Rc::new(Tr.clone()),
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
