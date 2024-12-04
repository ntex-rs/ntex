//! Service that limits number of in-flight async requests.
use ntex_service::{Middleware, Service, ServiceCtx};

use super::counter::Counter;

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

#[derive(Debug)]
pub struct InFlightService<S> {
    count: Counter,
    service: S,
}

impl<S> InFlightService<S> {
    pub fn new<R>(max: usize, service: S) -> Self
    where
        S: Service<R>,
    {
        Self {
            service,
            count: Counter::new(max),
        }
    }
}

impl<T, R> Service<R> for InFlightService<T>
where
    T: Service<R>,
{
    type Response = T::Response;
    type Error = T::Error;

    #[inline]
    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        if !self.count.is_available() {
            let (_, res) =
                crate::future::join(self.count.available(), ctx.ready(&self.service)).await;
            res
        } else {
            ctx.ready(&self.service).await
        }
    }

    #[inline]
    async fn call(
        &self,
        req: R,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let _guard = self.count.get();
        ctx.call(&self.service, req).await
    }

    ntex_service::forward_poll!(service);
    ntex_service::forward_shutdown!(service);
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, task::Poll, time::Duration};

    use ntex_service::{apply, fn_factory, Pipeline, ServiceFactory};

    use super::*;
    use crate::{channel::oneshot, future::lazy};

    struct SleepService(oneshot::Receiver<()>);

    impl Service<()> for SleepService {
        type Response = ();
        type Error = ();

        async fn call(&self, _: (), _: ServiceCtx<'_, Self>) -> Result<(), ()> {
            let _ = self.0.recv().await;
            Ok(())
        }
    }

    #[ntex_macros::rt_test2]
    async fn test_service() {
        let (tx, rx) = oneshot::channel();

        let srv = Pipeline::new(InFlightService::new(1, SleepService(rx))).bind();
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
