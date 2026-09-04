//! Service that limits number of in-flight async requests to 1.
use std::{cell::Cell, future::poll_fn, task::Poll};

use ntex_service::{Ctx, Middleware, Service};

use crate::task::LocalWaker;

/// `OneRequest` - service factory for service that can limit number of in-flight
/// async requests to 1.
#[derive(Copy, Clone, Default, Debug)]
pub struct OneRequest;

impl<S, St, Cfg> Middleware<S, St, Cfg> for OneRequest {
    type Service = OneRequestService<S>;

    fn create(&self, service: S, _: &Cfg) -> Self::Service {
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
    pub fn new<St, Req>(service: S) -> Self
    where
        S: Service<St, Req>,
    {
        Self {
            service,
            ready: Cell::new(true),
            waker: LocalWaker::new(),
        }
    }
}

impl<S: Service<St, Req>, St, Req> Service<St, Req> for OneRequestService<S> {
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), S::Error> {
        if !self.ready.get() {
            poll_fn(|cx| {
                self.waker.register(cx.waker());
                if self.ready.get() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
        }
        ctx.ready(&self.service).await
    }

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, S::Error> {
        self.ready.set(false);

        let result = ctx.call(&self.service, req).await;
        self.ready.set(true);
        self.waker.wake();
        result
    }

    ntex_service::forward_shutdown!(St, service);
}

#[cfg(test)]
mod tests {
    use ntex_service::{Pipeline, apply, fn_factory_nocfg};
    use std::{cell::RefCell, time::Duration};

    use super::*;
    use crate::{channel::oneshot, future::lazy};

    struct SleepService(oneshot::Receiver<()>);

    impl Service<(), ()> for SleepService {
        type Res = ();
        type Error = ();

        async fn call(&self, _r: (), _: Ctx<'_, Self>) -> Result<(), ()> {
            let _ = self.0.recv().await;
            Ok::<_, ()>(())
        }
    }

    #[ntex::test]
    async fn test_oneshot() {
        let (tx, rx) = oneshot::channel();

        let srv = Pipeline::with((), OneRequestService::new(SleepService(rx)));
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv2 = srv.bind();
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
        let sf = apply(
            OneRequest,
            fn_factory_nocfg(move || {
                let rx = rx.borrow_mut().take().unwrap();
                async move { Ok::<_, ()>(SleepService(rx)) }
            }),
        );

        let srv = sf.pipeline(&()).await.unwrap();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv1 = srv.bind();
        ntex::rt::spawn(async move {
            let _ = srv1.call(()).await;
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
        let sf = apply(
            OneRequest,
            fn_factory_nocfg(move || {
                let rx = rx.borrow_mut().take().unwrap();
                async move { Ok::<_, ()>(SleepService(rx)) }
            }),
        );

        let srv = sf.pipeline(&()).await.unwrap();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv1 = srv.bind();
        ntex::rt::spawn(async move {
            let _ = srv1.call(()).await;
        });
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        let _ = tx.send(());
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
    }
}
