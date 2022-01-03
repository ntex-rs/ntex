//! Service that limits number of in-flight async requests.
use std::{future::Future, marker::PhantomData, pin::Pin, task::Context, task::Poll};

use ntex_service::{IntoService, Service, Transform};

use super::counter::{Counter, CounterGuard};

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

impl<S> Transform<S> for InFlight {
    type Service = InFlightService<S>;

    fn new_transform(&self, service: S) -> Self::Service {
        InFlightService {
            service,
            count: Counter::new(self.max_inflight),
        }
    }
}

pub struct InFlightService<S> {
    count: Counter,
    service: S,
}

impl<S> InFlightService<S> {
    pub fn new<U, R>(max: usize, service: U) -> Self
    where
        S: Service<R>,
        U: IntoService<S, R>,
    {
        Self {
            count: Counter::new(max),
            service: service.into_service(),
        }
    }
}

impl<T, R> Service<R> for InFlightService<T>
where
    T: Service<R>,
{
    type Response = T::Response;
    type Error = T::Error;
    type Future = InFlightServiceResponse<T, R>;

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
    fn call(&self, req: R) -> Self::Future {
        InFlightServiceResponse {
            fut: self.service.call(req),
            _guard: self.count.get(),
            _t: PhantomData,
        }
    }
}

pin_project_lite::pin_project! {
    #[doc(hidden)]
    pub struct InFlightServiceResponse<T: Service<R>, R> {
        #[pin]
        fut: T::Future,
        _guard: CounterGuard,
        _t: PhantomData<R>
    }
}

impl<T: Service<R>, R> Future for InFlightServiceResponse<T, R> {
    type Output = Result<T::Response, T::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().fut.poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use ntex_service::{apply, fn_factory, Service, ServiceFactory};
    use std::{task::Context, task::Poll, time::Duration};

    use super::*;
    use crate::future::lazy;

    struct SleepService(Duration);

    impl Service<()> for SleepService {
        type Response = ();
        type Error = ();
        type Future = Pin<Box<dyn Future<Output = Result<(), ()>>>>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&self, _: ()) -> Self::Future {
            let fut = crate::time::sleep(self.0);
            Box::pin(async move {
                let _ = fut.await;
                Ok::<_, ()>(())
            })
        }
    }

    #[ntex_macros::rt_test2]
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

    #[ntex_macros::rt_test2]
    async fn test_newtransform() {
        let wait_time = Duration::from_millis(50);

        let srv = apply(
            InFlight::new(1),
            fn_factory(|| async { Ok::<_, ()>(SleepService(wait_time)) }),
        );

        let srv = srv.new_service(&()).await.unwrap();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let res = srv.call(());
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        let _ = res.await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
    }
}
