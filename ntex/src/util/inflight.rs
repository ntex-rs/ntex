//! Service that limits number of in-flight async requests.
use std::{convert::Infallible, future::Future, pin::Pin, task::Context, task::Poll};

use super::counter::{Counter, CounterGuard};
use crate::{util::Ready, IntoService, Service, Transform};

/// InFlight - service factory for service that can limit number of in-flight
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
    type Future = Ready<Self::Transform, Self::InitError>;

    fn new_transform(&self, service: S) -> Self::Future {
        Ready::Ok(InFlightService::new(self.max_inflight, service))
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

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.service.poll_ready(cx)?.is_pending() {
            Poll::Pending
        } else if !self.count.available(cx) {
            log::trace!("InFlight limit exceeded");
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.service.poll_shutdown(cx, is_error)
    }

    #[inline]
    fn call(&self, req: T::Request) -> Self::Future {
        InFlightServiceResponse {
            fut: self.service.call(req),
            _guard: self.count.get(),
        }
    }
}

pin_project_lite::pin_project! {
    #[doc(hidden)]
    pub struct InFlightServiceResponse<T: Service> {
        #[pin]
        fut: T::Future,
        _guard: CounterGuard,
    }
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
    use crate::service::{apply, fn_factory, Service, ServiceFactory};
    use crate::util::lazy;

    struct SleepService(Duration);

    impl Service for SleepService {
        type Request = ();
        type Response = ();
        type Error = ();
        type Future = Pin<Box<dyn Future<Output = Result<(), ()>>>>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&self, _: ()) -> Self::Future {
            let fut = crate::rt::time::sleep(self.0);
            Box::pin(async move {
                let _ = fut.await;
                Ok::<_, ()>(())
            })
        }
    }

    #[crate::rt_test]
    async fn test_transform() {
        let wait_time = Duration::from_millis(50);

        let srv = InFlightService::new(1, SleepService(wait_time));
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let res = srv.call(());
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        let _ = res.await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        assert!(lazy(|cx| srv.poll_shutdown(cx, false)).await.is_ready());
    }

    #[crate::rt_test]
    async fn test_newtransform() {
        let wait_time = Duration::from_millis(50);

        let srv = apply(
            InFlight::new(1),
            fn_factory(|| async { Ok(SleepService(wait_time)) }),
        );

        let srv = srv.new_service(&()).await.unwrap();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let res = srv.call(());
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        let _ = res.await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
    }
}
