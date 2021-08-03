use std::{future::Future, pin::Pin, task::{Context, Poll}};

use ntex_service::boxed::BoxFuture;

use crate::service::{Service, ServiceFactory};

pub enum Backpressure {
    Full,
    None,
}

pub trait BackpressureServiceFactory: Send + Clone + 'static {
    type Factory: ServiceFactory<Config = (), Request = (), Response = Backpressure>;

    fn create(&self) -> Self::Factory;
}

impl<F, T> BackpressureServiceFactory for F
where
    F: Fn() -> T + Send + Clone + 'static,
    T: ServiceFactory<Config = (), Request = (), Response = Backpressure>,
{
    type Factory = T;

    #[inline]
    fn create(&self) -> T {
        (self)()
    }
}

pub(super) trait BackpressureHandlerFactory: Send {
    fn clone_factory(&self) -> Box<dyn BackpressureHandlerFactory>;

    fn create(
        &self,
    ) -> BoxFuture<BoxedBackpressureHandler, ()>;
}

pub(super) struct BackpressureFactory<F: BackpressureServiceFactory>(F);

impl<F> BackpressureFactory<F>
where
    F: BackpressureServiceFactory,
{
    pub(super) fn new(factory: F) -> Self {
        Self(factory)
    }
}

impl<F> BackpressureHandlerFactory for BackpressureFactory<F>
where F: BackpressureServiceFactory 
{
    fn clone_factory(&self) -> Box<dyn BackpressureHandlerFactory> {
        Box::new(Self(self.0.clone()))
    }

    fn create(
        &self,
    ) -> BoxFuture<BoxedBackpressureHandler, ()> {
        let fut = self.0.create().new_service(());

        Box::pin(async move {
            fut.await
                .map(|srv| {
                    Box::new(ServiceHandler {
                        service: srv,
                        fut: None,
                    }) as Box<dyn BackpressureServiceHandler>
                })
                .map_err(|_| ())
        })
    }
}

pub(super) trait BackpressureServiceHandler {
    fn poll_ready(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Backpressure, ()>>;
}

pub(super) type BoxedBackpressureHandler = Box<dyn BackpressureServiceHandler>;

pin_project_lite::pin_project! {
    struct ServiceHandler<T>
    where
        T: Service<Request = (), Response = Backpressure>,
    {
        service: T,
        #[pin]
        fut: Option<T::Future>,
    }
}

impl<T> BackpressureServiceHandler for ServiceHandler<T>
where
    T: Service<Request = (), Response = Backpressure>,
{
    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Backpressure, ()>> {
        let mut this = self.as_mut().project();

        // if this backpressure service is not ready, it can not provide backpressure
        // or block availability any longer.
        match this.service.poll_ready(cx) {
            Poll::Ready(Ok(_)) => (),
            Poll::Ready(Err(_)) => return Poll::Ready(Err(())),
            Poll::Pending => {
                this.fut.set(None);
                return Poll::Ready(Ok(Backpressure::None));
            }
        }

        this.fut.set(Some(this.service.call(())));
        let result = this
            .fut
            .as_pin_mut()
            .expect("set above")
            .poll(cx)
            .map_err(|_| ());
        if result.is_ready() {
            self.project().fut.set(None)
        }
        result
    }
}
