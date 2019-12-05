use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::transform_err::TransformMapInitErr;
use crate::{IntoServiceFactory, Service, ServiceFactory};

/// Apply transform to a service. Function returns
/// services factory that in initialization creates
/// service and applies transform to this service.
pub fn apply<T, S, U>(t: T, service: U) -> ApplyTransform<T, S>
where
    S: ServiceFactory,
    T: Transform<S::Service, InitError = S::InitError>,
    U: IntoServiceFactory<S>,
{
    ApplyTransform::new(t, service.into_factory())
}

/// The `Transform` trait defines the interface of a Service factory. `Transform`
/// is often implemented for middleware, defining how to construct a
/// middleware Service. A Service that is constructed by the factory takes
/// the Service that follows it during execution as a parameter, assuming
/// ownership of the next Service.
pub trait Transform<S> {
    /// Requests handled by the service.
    type Request;

    /// Responses given by the service.
    type Response;

    /// Errors produced by the service.
    type Error;

    /// The `TransformService` value created by this factory
    type Transform: Service<
        Request = Self::Request,
        Response = Self::Response,
        Error = Self::Error,
    >;

    /// Errors produced while building a service.
    type InitError;

    /// The future response value.
    type Future: Future<Output = Result<Self::Transform, Self::InitError>>;

    /// Creates and returns a new Service component, asynchronously
    fn new_transform(&self, service: S) -> Self::Future;

    /// Map this transforms's factory error to a different error,
    /// returning a new transform service factory.
    fn map_init_err<F, E>(self, f: F) -> TransformMapInitErr<Self, S, F, E>
    where
        Self: Sized,
        F: Fn(Self::InitError) -> E + Clone,
    {
        TransformMapInitErr::new(self, f)
    }
}

impl<T, S> Transform<S> for Rc<T>
where
    T: Transform<S>,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type InitError = T::InitError;
    type Transform = T::Transform;
    type Future = T::Future;

    fn new_transform(&self, service: S) -> T::Future {
        self.as_ref().new_transform(service)
    }
}

impl<T, S> Transform<S> for Arc<T>
where
    T: Transform<S>,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type InitError = T::InitError;
    type Transform = T::Transform;
    type Future = T::Future;

    fn new_transform(&self, service: S) -> T::Future {
        self.as_ref().new_transform(service)
    }
}

/// `Apply` transform to new service
pub struct ApplyTransform<T, S>(Rc<(T, S)>);

impl<T, S> ApplyTransform<T, S>
where
    S: ServiceFactory,
    T: Transform<S::Service, InitError = S::InitError>,
{
    /// Create new `ApplyTransform` new service instance
    fn new(t: T, service: S) -> Self {
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
    T: Transform<S::Service, InitError = S::InitError>,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;

    type Config = S::Config;
    type Service = T::Transform;
    type InitError = T::InitError;
    type Future = ApplyTransformFuture<T, S>;

    fn new_service(&self, cfg: S::Config) -> Self::Future {
        ApplyTransformFuture {
            store: self.0.clone(),
            state: ApplyTransformFutureState::A(self.0.as_ref().1.new_service(cfg)),
        }
    }
}

#[pin_project::pin_project]
pub struct ApplyTransformFuture<T, S>
where
    S: ServiceFactory,
    T: Transform<S::Service, InitError = S::InitError>,
{
    store: Rc<(T, S)>,
    #[pin]
    state: ApplyTransformFutureState<T, S>,
}

#[pin_project::pin_project]
pub enum ApplyTransformFutureState<T, S>
where
    S: ServiceFactory,
    T: Transform<S::Service, InitError = S::InitError>,
{
    A(#[pin] S::Future),
    B(#[pin] T::Future),
}

impl<T, S> Future for ApplyTransformFuture<T, S>
where
    S: ServiceFactory,
    T: Transform<S::Service, InitError = S::InitError>,
{
    type Output = Result<T::Transform, T::InitError>;

    #[pin_project::project]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        #[project]
        match this.state.as_mut().project() {
            ApplyTransformFutureState::A(fut) => match fut.poll(cx)? {
                Poll::Ready(srv) => {
                    let fut = this.store.0.new_transform(srv);
                    this.state.set(ApplyTransformFutureState::B(fut));
                    self.poll(cx)
                }
                Poll::Pending => Poll::Pending,
            },
            ApplyTransformFutureState::B(fut) => fut.poll(cx),
        }
    }
}
