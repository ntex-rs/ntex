//! Service that limits number of in-flight async requests to 1.
use std::{cell::Cell, task::Context, task::Poll};

use ntex_service::{IntoService, Middleware, Service, ServiceCtx};

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

#[derive(Clone, Debug)]
pub struct OneRequestService<S> {
    waker: LocalWaker,
    service: S,
    ready: Cell<bool>,
}

impl<S> OneRequestService<S> {
    pub fn new<U, R>(service: U) -> Self
    where
        S: Service<R>,
        U: IntoService<S, R>,
    {
        Self {
            service: service.into_service(),
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
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.waker.register(cx.waker());
        if self.service.poll_ready(cx)?.is_pending() {
            Poll::Pending
        } else if self.ready.get() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
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

    ntex_service::forward_poll_shutdown!(service);
}

#[cfg(test)]
mod tests {
    use ntex_service::{apply, fn_factory, Pipeline, Service, ServiceCtx, ServiceFactory};
    use std::{cell::RefCell, task::Poll, time::Duration};

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

    #[ntex_macros::rt_test2]
    async fn test_oneshot() {
        let (tx, rx) = oneshot::channel();

        let srv = Pipeline::new(OneRequestService::new(SleepService(rx)));
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
        assert_eq!(format!("{:?}", OneRequest), "OneRequest");

        let (tx, rx) = oneshot::channel();
        let rx = RefCell::new(Some(rx));
        let srv = apply(
            OneRequest,
            fn_factory(move || {
                let rx = rx.borrow_mut().take().unwrap();
                async move { Ok::<_, ()>(SleepService(rx)) }
            }),
        );

        let srv = srv.pipeline(&()).await.unwrap();
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
