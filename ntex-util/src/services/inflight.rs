//! Service that limits number of in-flight async requests.
use std::{future::Future, marker::PhantomData, pin::Pin, task::Context, task::Poll};

use ntex_service::{Ctx, IntoService, Middleware, Service, ServiceCall};

use super::counter::{Counter, CounterGuard};

/// InFlight - service factory for service that can limit number of in-flight
/// async requests.
///
/// Default number of in-flight requests is 15
#[derive(Copy, Clone, Debug)]
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

impl<S> Middleware<S> for InFlight {
    type Service = InFlightService<S>;

    fn create(&self, service: S) -> Self::Service {
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
    type Future<'f> = InFlightServiceResponse<'f, T, R> where Self: 'f, R: 'f;

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
    fn call<'a>(&'a self, req: R, ctx: Ctx<'a, Self>) -> Self::Future<'a> {
        InFlightServiceResponse {
            fut: ctx.call(&self.service, req),
            _guard: self.count.get(),
            _t: PhantomData,
        }
    }

    ntex_service::forward_poll_shutdown!(service);
}

pin_project_lite::pin_project! {
    #[doc(hidden)]
    pub struct InFlightServiceResponse<'f, T: Service<R>, R>
    where T: 'f, R: 'f
    {
        #[pin]
        fut: ServiceCall<'f, T, R>,
        _guard: CounterGuard,
        _t: PhantomData<R>
    }
}

impl<'f, T: Service<R>, R> Future for InFlightServiceResponse<'f, T, R> {
    type Output = Result<T::Response, T::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().fut.poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use ntex_service::{apply, fn_factory, Container, Ctx, Service, ServiceFactory};
    use std::{cell::RefCell, task::Poll, time::Duration};

    use super::*;
    use crate::{channel::oneshot, future::lazy, future::BoxFuture};

    struct SleepService(oneshot::Receiver<()>);

    impl Service<()> for SleepService {
        type Response = ();
        type Error = ();
        type Future<'f> = BoxFuture<'f, Result<(), ()>>;

        fn call<'a>(&'a self, _: (), _: Ctx<'a, Self>) -> Self::Future<'a> {
            Box::pin(async move {
                let _ = self.0.recv().await;
                Ok::<_, ()>(())
            })
        }
    }

    #[ntex_macros::rt_test2]
    async fn test_service() {
        let (tx, rx) = oneshot::channel();

        let srv = Container::new(InFlightService::new(1, SleepService(rx)));
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv2 = srv.clone();
        ntex::rt::spawn(async move {
            let _ = srv2.call(()).await;
        });
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        let _ = tx.send(());
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(lazy(|cx| srv.poll_shutdown(cx)).await.is_ready());
    }

    #[ntex_macros::rt_test2]
    async fn test_middleware() {
        assert_eq!(InFlight::default().max_inflight, 15);
        assert_eq!(
            format!("{:?}", InFlight::new(1)),
            "InFlight { max_inflight: 1 }"
        );

        let (tx, rx) = oneshot::channel();
        let rx = RefCell::new(Some(rx));
        let srv = apply(
            InFlight::new(1),
            fn_factory(move || {
                let rx = rx.borrow_mut().take().unwrap();
                async move { Ok::<_, ()>(SleepService(rx)) }
            }),
        );

        let srv = srv.container(&()).await.unwrap();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv2 = srv.clone();
        ntex::rt::spawn(async move {
            let _ = srv2.call(()).await;
        });
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        let _ = tx.send(());
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
    }
}
