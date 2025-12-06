//! Service that limits number of in-flight async requests to 1.
use std::{cell::Cell, future::poll_fn, task::Poll};

use ntex_service::{Middleware, Middleware2, Service, ServiceCtx};

use crate::task::LocalWaker;

/// OneRequest - service factory for service that can limit number of in-flight
/// async requests to 1.
#[derive(Copy, Clone, Default, Debug)]
pub struct OneRequest;

impl<S> Middleware<S> for OneRequest {
    type Service = OneRequestService<S>;

    fn create(&self, service: S) -> Self::Service {
        OneRequestService {
            service,
            ready: Cell::new(true),
            waker: LocalWaker::new(),
        }
    }
}

impl<S, C> Middleware2<S, C> for OneRequest {
    type Service = OneRequestService<S>;

    fn create(&self, service: S, _: C) -> Self::Service {
        OneRequestService {
            service,
            ready: Cell::new(true),
            waker: LocalWaker::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct OneRequestService<S> {
    waker: LocalWaker,
    service: S,
    ready: Cell<bool>,
}

impl<S> OneRequestService<S> {
    pub fn new<R>(service: S) -> Self
    where
        S: Service<R>,
    {
        Self {
            service,
            ready: Cell::new(true),
            waker: LocalWaker::new(),
        }
    }
}

impl<T, R> Service<R> for OneRequestService<T>
where
    T: Service<R>,
{
    type Response = T::Response;
    type Error = T::Error;

    #[inline]
    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        if !self.ready.get() {
            poll_fn(|cx| {
                self.waker.register(cx.waker());
                if self.ready.get() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await
        }
        ctx.ready(&self.service).await
    }

    #[inline]
    async fn call(
        &self,
        req: R,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        self.ready.set(false);

        let result = ctx.call(&self.service, req).await;
        self.ready.set(true);
        self.waker.wake();
        result
    }

    ntex_service::forward_poll!(service);
    ntex_service::forward_shutdown!(service);
}

#[cfg(test)]
mod tests {
    use ntex_service::{Pipeline, ServiceFactory, apply, apply2, fn_factory};
    use std::{cell::RefCell, time::Duration};

    use super::*;
    use crate::{channel::oneshot, future::lazy};

    struct SleepService(oneshot::Receiver<()>);

    impl Service<()> for SleepService {
        type Response = ();
        type Error = ();

        async fn call(&self, _: (), _: ServiceCtx<'_, Self>) -> Result<(), ()> {
            let _ = self.0.recv().await;
            Ok::<_, ()>(())
        }
    }

    #[ntex::test]
    async fn test_oneshot() {
        let (tx, rx) = oneshot::channel();

        let srv = Pipeline::new(OneRequestService::new(SleepService(rx))).bind();
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
        srv.shutdown().await;
    }

    #[ntex::test]
    async fn test_middleware() {
        assert_eq!(format!("{OneRequest:?}"), "OneRequest");

        let (tx, rx) = oneshot::channel();
        let rx = RefCell::new(Some(rx));
        let srv = apply(
            OneRequest,
            fn_factory(move || {
                let rx = rx.borrow_mut().take().unwrap();
                async move { Ok::<_, ()>(SleepService(rx)) }
            }),
        );

        let srv = srv.pipeline(&()).await.unwrap().bind();
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

    #[ntex::test]
    async fn test_middleware2() {
        assert_eq!(format!("{OneRequest:?}"), "OneRequest");

        let (tx, rx) = oneshot::channel();
        let rx = RefCell::new(Some(rx));
        let srv = apply2(
            OneRequest,
            fn_factory(move || {
                let rx = rx.borrow_mut().take().unwrap();
                async move { Ok::<_, ()>(SleepService(rx)) }
            }),
        );

        let srv = srv.pipeline(&()).await.unwrap().bind();
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
