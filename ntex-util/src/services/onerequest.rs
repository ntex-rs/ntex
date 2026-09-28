//! Middleware that permits one service call at a time.
use std::{cell::Cell, future::poll_fn, task::Poll};

use ntex_service::{Ctx, Middleware, Service};

use crate::channel::condition::Condition;

/// Middleware that serializes calls to its wrapped service.
#[derive(Copy, Clone, Default, Debug)]
pub struct OneRequest;

impl<S, St> Middleware<S, St> for OneRequest {
    type Service = OneRequestService<S>;

    fn create(&self, _: &St, service: S) -> Self::Service {
        OneRequestService {
            service,
            ready: Cell::new(true),
            waiters: Condition::new(),
        }
    }
}

/// Service wrapper that allows only one call to run at a time.
///
/// This type is intentionally not cloneable. Cloning its readiness state would
/// create another independent gate and allow calls to overlap.
#[derive(Debug)]
pub struct OneRequestService<S> {
    waiters: Condition,
    service: S,
    ready: Cell<bool>,
}

impl<S> OneRequestService<S> {
    /// Wraps a service so that concurrent callers wait for the active call.
    pub fn new<St, Req>(service: S) -> Self
    where
        S: Service<St, Req>,
    {
        Self {
            service,
            ready: Cell::new(true),
            waiters: Condition::new(),
        }
    }
}

impl<S> OneRequestService<S> {
    /// Waits until no call is active.
    async fn acquire(&self) {
        if self.ready.get() {
            return;
        }

        let waiter = self.waiters.wait();
        poll_fn(|cx| {
            // the waiter stays registered after a notification, another task
            // may have started a call before this one was polled
            let _ = waiter.poll_ready(cx);
            if self.ready.get() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
}

/// Releases the call slot even if the call future is dropped or panics.
struct Release<'a, S>(&'a OneRequestService<S>);

impl<S> Drop for Release<'_, S> {
    fn drop(&mut self) {
        self.0.ready.set(true);
        self.0.waiters.notify(());
    }
}

impl<S: Service<St, Req>, St, Req> Service<St, Req> for OneRequestService<S> {
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), S::Error> {
        self.acquire().await;
        ctx.ready(&self.service).await
    }

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, S::Error> {
        // several callers can observe readiness before any of them calls
        self.acquire().await;
        self.ready.set(false);
        let _release = Release(self);

        ctx.call(&self.service, req).await
    }

    ntex_service::forward_shutdown!(St, service);
}

#[cfg(test)]
mod tests {
    use ntex_service::{Pipeline, apply, fn_factory};
    use std::{cell::RefCell, rc::Rc, time::Duration};

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

        let srv = Pipeline::new((), OneRequestService::new(SleepService(rx)));
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
            fn_factory(move |(): &()| {
                let rx = rx.borrow_mut().take().unwrap();
                async move { Ok::<_, ()>(SleepService(rx)) }
            }),
        );

        let srv = sf.pipeline(()).await.unwrap();
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
            fn_factory(move |(): &()| {
                let rx = rx.borrow_mut().take().unwrap();
                async move { Ok::<_, ()>(SleepService(rx)) }
            }),
        );

        let srv = sf.pipeline(()).await.unwrap();
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
    async fn test_cancelled_call_releases() {
        let (_tx, rx) = oneshot::channel();
        let srv = Pipeline::new((), OneRequestService::new(SleepService(rx)));

        let res = crate::time::timeout(Duration::from_millis(25), srv.call(())).await;
        assert!(res.is_err());
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
    }

    struct CountService {
        active: Rc<Cell<usize>>,
        max: Rc<Cell<usize>>,
    }

    impl Service<(), ()> for CountService {
        type Res = ();
        type Error = ();

        async fn call(&self, _r: (), _: Ctx<'_, Self>) -> Result<(), ()> {
            self.active.set(self.active.get() + 1);
            self.max.set(self.max.get().max(self.active.get()));
            crate::time::sleep(Duration::from_millis(10)).await;
            self.active.set(self.active.get() - 1);
            Ok(())
        }
    }

    fn count_srv() -> (Pipeline<(), (), ()>, Rc<Cell<usize>>) {
        let max = Rc::new(Cell::new(0));
        let srv = Pipeline::new(
            (),
            OneRequestService::new(CountService {
                active: Rc::new(Cell::new(0)),
                max: max.clone(),
            }),
        );
        (srv, max)
    }

    #[ntex::test]
    async fn test_many_waiters() {
        let (srv, max) = count_srv();
        let done = Rc::new(Cell::new(0));
        for _ in 0..4 {
            let (srv, done) = (srv.bind(), done.clone());
            ntex::rt::spawn(async move {
                srv.call(()).await.unwrap();
                done.set(done.get() + 1);
            });
        }

        // calls run one after another, wait for all of them with a generous
        // deadline, timers on loaded CI runners overshoot a lot
        for _ in 0..100 {
            if done.get() == 4 {
                break;
            }
            crate::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(done.get(), 4);
        assert_eq!(max.get(), 1);
    }

    #[ntex::test]
    async fn test_no_overlap_after_ready() {
        let (srv, max) = count_srv();

        srv.ready().await.unwrap();
        srv.ready().await.unwrap();
        let (r1, r2) = crate::future::join(srv.call_nowait(()), srv.call_nowait(())).await;
        assert!(r1.is_ok() && r2.is_ok());
        assert_eq!(max.get(), 1);
    }
}
