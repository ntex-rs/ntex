//! Middleware for limiting concurrent service calls.
use std::cell::Cell;

use ntex_service::{Ctx, Middleware, Service};

use super::counter::Counter;

/// Middleware that limits the number of concurrent calls to a service.
///
/// Readiness remains pending while every slot is in use. The default limit is
/// 15 concurrent calls.
#[derive(Copy, Clone, Debug)]
pub struct InFlight {
    max_inflight: usize,
}

impl InFlight {
    /// Creates middleware with the specified concurrency limit.
    ///
    /// A limit of zero keeps the service permanently unavailable.
    pub fn new(max: usize) -> Self {
        Self { max_inflight: max }
    }
}

impl Default for InFlight {
    fn default() -> Self {
        Self::new(15)
    }
}

impl<S, St> Middleware<S, St> for InFlight {
    type Service = InFlightService<S>;

    fn create(&self, _: &St, service: S) -> Self::Service {
        InFlightService::new(self.max_inflight, service)
    }
}

#[derive(Debug)]
/// Service wrapper that enforces a concurrent-call limit.
pub struct InFlightService<S> {
    count: Counter,
    service: S,
    ready: Cell<bool>,
    entered: Cell<u32>,
}

impl<S> InFlightService<S> {
    /// Wraps `service` with the specified concurrency limit.
    ///
    /// A limit of zero keeps the service permanently unavailable.
    pub fn new(max: usize, service: S) -> Self {
        Self {
            service,
            count: Counter::new(max),
            ready: Cell::new(false),
            entered: Cell::new(0),
        }
    }
}

impl<S, St, Req> Service<St, Req> for InFlightService<S>
where
    S: Service<St, Req>,
{
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), S::Error> {
        let entered = self.entered.get();
        let result = if self.count.is_available() {
            ctx.ready(&self.service).await
        } else {
            crate::future::join(self.count.available(), ctx.ready(&self.service))
                .await
                .1?;
            // the inner readiness can be stale after waiting for a free slot
            ctx.ready(&self.service).await
        };
        // valid only if no call entered the service during the check
        self.ready
            .set(result.is_ok() && entered == self.entered.get());
        result
    }

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, S::Error> {
        if !self.ready.get() {
            ctx.ready(self).await?;
        }
        self.ready.set(false);
        self.entered.set(self.entered.get().wrapping_add(1));
        let _guard = self.count.get();
        ctx.call_nowait(&self.service, req).await
    }

    ntex_service::forward_shutdown!(St, service);
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, cell::RefCell, rc::Rc, task::Poll, time::Duration};

    use async_channel as mpmc;
    use ntex_service::{Pipeline, apply, fn_factory};

    use super::*;
    use crate::{channel::oneshot, future::lazy};

    struct SleepService(mpmc::Receiver<()>);

    impl Service<(), ()> for SleepService {
        type Res = ();
        type Error = ();

        async fn call(&self, _r: (), _: Ctx<'_, Self>) -> Result<(), ()> {
            let _ = self.0.recv().await;
            Ok(())
        }
    }

    #[ntex::test]
    async fn test_service() {
        let (tx, rx) = mpmc::unbounded();
        let counter = Rc::new(Cell::new(0));

        let srv = Pipeline::new((), InFlightService::new(1, SleepService(rx)));
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let counter2 = counter.clone();
        let fut = srv.call_static(());
        ntex::rt::spawn(async move {
            let _ = fut.await;
            counter2.set(counter2.get() + 1);
        });
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        let counter2 = counter.clone();
        let fut = srv.call_static(());
        ntex::rt::spawn(async move {
            let _ = fut.await;
            counter2.set(counter2.get() + 1);
        });
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        let counter2 = counter.clone();
        let fut = srv.call_static(());
        let (stx, srx) = oneshot::channel::<()>();
        ntex::rt::spawn(async move {
            let _ = fut.await;
            counter2.set(counter2.get() + 1);
            let _ = stx.send(());
        });
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        let _ = tx.send(()).await;
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let _ = tx.send(()).await;
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        let _ = tx.send(()).await;
        let _ = srx.recv().await;
        assert_eq!(counter.get(), 3);
        srv.shutdown().await;
    }

    #[ntex::test]
    async fn test_middleware() {
        assert_eq!(InFlight::default().max_inflight, 15);
        assert_eq!(
            format!("{:?}", InFlight::new(1)),
            "InFlight { max_inflight: 1 }"
        );

        let (tx, rx) = mpmc::unbounded();
        let rx = RefCell::new(Some(rx));
        let sf = apply(
            InFlight::new(1),
            fn_factory(move |(): &()| {
                let rx = rx.borrow_mut().take().unwrap();
                async move { Ok::<_, ()>(SleepService(rx)) }
            }),
        );

        let srv = sf.pipeline(()).await.unwrap();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv2 = srv.bind();
        ntex::rt::spawn(async move {
            let _ = srv2.call(()).await;
        });
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        let _ = tx.send(()).await;
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
    }

    #[ntex::test]
    async fn test_middleware2() {
        assert_eq!(InFlight::default().max_inflight, 15);
        assert_eq!(
            format!("{:?}", InFlight::new(1)),
            "InFlight { max_inflight: 1 }"
        );

        let (tx, rx) = mpmc::unbounded();
        let rx = RefCell::new(Some(rx));
        let sf = apply(
            InFlight::new(1),
            fn_factory(move |(): &()| {
                let rx = rx.borrow_mut().take().unwrap();
                async move { Ok::<_, ()>(SleepService(rx)) }
            }),
        );

        let srv = sf.pipeline(()).await.unwrap();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv2 = srv.bind();
        ntex::rt::spawn(async move {
            let _ = srv2.call(()).await;
        });
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        let _ = tx.send(()).await;
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
    }

    #[derive(Default)]
    struct ProbeState {
        active: Cell<usize>,
        max: Cell<usize>,
        checks: Cell<usize>,
        waker: crate::task::LocalWaker,
    }

    /// Inner service with its own concurrency limit
    struct Probe(Rc<ProbeState>, usize, mpmc::Receiver<()>);

    impl Service<(), ()> for Probe {
        type Res = ();
        type Error = ();

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), ()> {
            self.0.checks.set(self.0.checks.get() + 1);
            std::future::poll_fn(|cx| {
                if self.0.active.get() < self.1 {
                    Poll::Ready(Ok(()))
                } else {
                    self.0.waker.register(cx.waker());
                    Poll::Pending
                }
            })
            .await
        }

        async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<(), ()> {
            let st = &self.0;
            st.active.set(st.active.get() + 1);
            st.max.set(st.max.get().max(st.active.get()));
            let _ = self.2.recv().await;
            st.active.set(st.active.get() - 1);
            st.waker.wake();
            Ok(())
        }
    }

    #[ntex::test]
    async fn test_inner_readiness_checked_once() {
        let (tx, rx) = mpmc::unbounded();
        let st = Rc::new(ProbeState::default());
        let srv = Pipeline::new((), InFlightService::new(4, Probe(st.clone(), 4, rx)));
        for _ in 0..3 {
            let _ = tx.send(()).await;
            srv.call(()).await.unwrap();
        }
        assert_eq!(st.checks.get(), 3);

        st.checks.set(0);
        for _ in 0..3 {
            let _ = tx.send(()).await;
            srv.ready().await.unwrap();
            srv.call(()).await.unwrap();
        }
        assert_eq!(st.checks.get(), 3);
    }

    /// Calls the inner service without checking its readiness, the inner
    /// service has to enforce its limits
    struct NoWait<S>(S);

    impl<S: Service<(), (), Res = (), Error = ()>> Service<(), ()> for NoWait<S> {
        type Res = ();
        type Error = ();

        async fn call(&self, req: (), ctx: Ctx<'_, Self>) -> Result<(), ()> {
            ctx.call_nowait(&self.0, req).await
        }
    }

    /// Returns the max number of concurrent calls of the inner service
    async fn run_concurrent(max: usize, inner: usize) -> usize {
        let (tx, rx) = mpmc::unbounded();
        let st = Rc::new(ProbeState::default());
        let srv = Pipeline::new(
            (),
            NoWait(InFlightService::new(max, Probe(st.clone(), inner, rx))),
        );
        let mut futs = Vec::new();
        for _ in 0..4 {
            futs.push(ntex::rt::spawn(srv.call_static(())));
        }
        crate::time::sleep(Duration::from_millis(20)).await;
        for _ in 0..4 {
            let _ = tx.send(()).await;
        }
        for f in futs {
            let _ = f.await;
        }
        st.max.get()
    }

    #[ntex::test]
    async fn test_limits_without_readiness_check() {
        assert_eq!(run_concurrent(1, 4).await, 1, "inflight limit");
        assert_eq!(run_concurrent(2, 1).await, 1, "inner limit");
        assert_eq!(run_concurrent(2, 4).await, 2, "inflight limit 2");
    }
}
