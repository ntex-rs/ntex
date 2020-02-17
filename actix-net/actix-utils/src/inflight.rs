use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use actix_service::{IntoService, Service, Transform};
use futures::future::{ok, Ready};

use super::counter::{Counter, CounterGuard};

/// InFlight - new service for service that can limit number of in-flight
/// async requests.
///
/// Default number of in-flight requests is 15
pub struct InFlight {
    max_inflight: usize,
}

impl InFlight {
    pub fn new(max: usize) -> Self {
        Self { max_inflight: max }
    }
}

impl Default for InFlight {
    fn default() -> Self {
        Self::new(15)
    }
}

impl<S> Transform<S> for InFlight
where
    S: Service,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type InitError = Infallible;
    type Transform = InFlightService<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(InFlightService::new(self.max_inflight, service))
    }
}

pub struct InFlightService<S> {
    count: Counter,
    service: S,
}

impl<S> InFlightService<S>
where
    S: Service,
{
    pub fn new<U>(max: usize, service: U) -> Self
    where
        U: IntoService<S>,
    {
        Self {
            count: Counter::new(max),
            service: service.into_service(),
        }
    }
}

impl<T> Service for InFlightService<T>
where
    T: Service,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type Future = InFlightServiceResponse<T>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if let Poll::Pending = self.service.poll_ready(cx)? {
            Poll::Pending
        } else if !self.count.available(cx) {
            log::trace!("InFlight limit exceeded");
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn call(&mut self, req: T::Request) -> Self::Future {
        InFlightServiceResponse {
            fut: self.service.call(req),
            _guard: self.count.get(),
        }
    }
}

#[doc(hidden)]
#[pin_project::pin_project]
pub struct InFlightServiceResponse<T: Service> {
    #[pin]
    fut: T::Future,
    _guard: CounterGuard,
}

impl<T: Service> Future for InFlightServiceResponse<T> {
    type Output = Result<T::Response, T::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().fut.poll(cx)
    }
}

#[cfg(test)]
mod tests {

    use std::task::{Context, Poll};
    use std::time::Duration;

    use super::*;
    use actix_service::{apply, fn_factory, Service, ServiceFactory};
    use futures::future::{lazy, ok, FutureExt, LocalBoxFuture};

    struct SleepService(Duration);

    impl Service for SleepService {
        type Request = ();
        type Response = ();
        type Error = ();
        type Future = LocalBoxFuture<'static, Result<(), ()>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: ()) -> Self::Future {
            actix_rt::time::delay_for(self.0)
                .then(|_| ok::<_, ()>(()))
                .boxed_local()
        }
    }

    #[actix_rt::test]
    async fn test_transform() {
        let wait_time = Duration::from_millis(50);

        let mut srv = InFlightService::new(1, SleepService(wait_time));
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let res = srv.call(());
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        let _ = res.await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
    }

    #[actix_rt::test]
    async fn test_newtransform() {
        let wait_time = Duration::from_millis(50);

        let srv = apply(InFlight::new(1), fn_factory(|| ok(SleepService(wait_time))));

        let mut srv = srv.new_service(&()).await.unwrap();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let res = srv.call(());
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        let _ = res.await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
    }
}
