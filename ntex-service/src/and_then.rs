use std::cell::Cell;

use super::{Ctx, Service, ServiceFactory, util};

#[derive(Debug)]
/// Service produced by the `and_then` combinator.
///
/// This is created by the `Service::and_then()` and `ServiceChain::and_then()` methods.
pub struct AndThen<A, B> {
    svc1: A,
    svc2: B,
    ready: Cell<bool>,
    entered: Cell<u32>,
}

impl<A, B> AndThen<A, B> {
    /// Creates a new `AndThen` service.
    pub(crate) fn new(svc1: A, svc2: B) -> Self {
        Self {
            svc1,
            svc2,
            ready: Cell::new(false),
            entered: Cell::new(0),
        }
    }
}

impl<A: Clone, B: Clone> Clone for AndThen<A, B> {
    fn clone(&self) -> Self {
        Self::new(self.svc1.clone(), self.svc2.clone())
    }
}

impl<A, B, St, Req> Service<St, Req> for AndThen<A, B>
where
    A: Service<St, Req>,
    B: Service<St, A::Res, Error = A::Error>,
{
    type Res = B::Res;
    type Error = A::Error;

    #[inline]
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<B::Res, A::Error> {
        let result = ctx.call_nowait(&self.svc1, req).await?;

        if !self.ready.get() {
            ctx.ready(&self.svc2).await?;
        }
        // entering svc2 invalidates all svc2 readiness observed so far
        self.ready.set(false);
        self.entered.set(self.entered.get().wrapping_add(1));
        ctx.call_nowait(&self.svc2, result).await
    }

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        let entered = self.entered.get();
        let res = util::ready(&self.svc1, &self.svc2, ctx).await;
        if res.is_err() {
            self.ready.set(false);
        } else if self.entered.get() == entered {
            // svc2 readiness is valid only if no call entered svc2 during the check
            self.ready.set(true);
        }
        res
    }

    #[inline]
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        util::shutdown(&self.svc1, &self.svc2, ctx).await;
    }
}

#[derive(Debug, Clone)]
/// Service factory produced by the `and_then` combinator.
///
/// This is created by the `ServiceChainFactory::and_then()` method.
pub struct AndThenFactory<A, B> {
    svc1: A,
    svc2: B,
}

impl<A, B> AndThenFactory<A, B> {
    /// Creates a new `AndThenFactory`.
    pub fn new(svc1: A, svc2: B) -> Self {
        Self { svc1, svc2 }
    }
}

impl<A, B, St, Req> ServiceFactory<St, Req> for AndThenFactory<A, B>
where
    A: ServiceFactory<St, Req>,
    B: ServiceFactory<St, A::Res, Error = A::Error, InitError = A::InitError>,
{
    type Res = B::Res;
    type Error = A::Error;

    type Service = AndThen<A::Service, B::Service>;
    type InitError = A::InitError;

    #[inline]
    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        Ok(AndThen {
            svc1: self.svc1.create(st).await?,
            svc2: self.svc2.create(st).await?,
            ready: Cell::new(false),
            entered: Cell::new(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use crate::{Ctx, Service, factory, fn_factory, service};

    #[derive(Debug, Clone)]
    struct Srv1(Rc<Cell<usize>>, Rc<Cell<usize>>);

    impl Service<(), &'static str> for Srv1 {
        type Res = &'static str;
        type Error = ();

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }

        async fn call(&self, req: &'static str, _: Ctx<'_, Self>) -> Result<Self::Res, ()> {
            Ok(req)
        }

        async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
            self.1.set(self.1.get() + 1);
        }
    }

    #[derive(Debug, Clone)]
    struct Srv2(Rc<Cell<usize>>, Rc<Cell<usize>>);

    impl Service<(), &'static str> for Srv2 {
        type Res = (&'static str, &'static str);
        type Error = ();

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }

        async fn call(&self, req: &'static str, _: Ctx<'_, Self>) -> Result<Self::Res, ()> {
            Ok((req, "srv2"))
        }

        async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
            self.1.set(self.1.get() + 1);
        }
    }

    #[ntex::test]
    async fn test_ready() {
        let cnt = Rc::new(Cell::new(0));
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = service(Rc::new(Srv1(cnt.clone(), cnt_sht.clone())))
            .clone()
            .and_then(crate::boxed::service(Srv2(cnt.clone(), cnt_sht.clone())));
        assert!(format!("{srv:?}").contains("AndThen"));

        let srv = srv.pipeline(());
        let res = srv.ready().await;
        assert_eq!(res, Ok(()));
        assert_eq!(cnt.get(), 2);

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);
    }

    #[ntex::test]
    async fn test_ready2() {
        let cnt = Rc::new(Cell::new(0));
        let srv = Box::new(
            service(Srv1(cnt.clone(), Rc::new(Cell::new(0))))
                .and_then(Srv2(cnt.clone(), Rc::new(Cell::new(0)))),
        )
        .pipeline(());
        let res = srv.ready().await;
        assert_eq!(res, Ok(()));
        assert_eq!(cnt.get(), 2);
    }

    #[ntex::test]
    async fn test_call() {
        let cnt = Rc::new(Cell::new(0));
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = Srv1(cnt.clone(), cnt_sht.clone())
            .and_then(Srv2(cnt, cnt_sht.clone()))
            .pipeline(());
        let res = srv.call("srv1").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv1", "srv2"));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);
    }

    #[ntex::test]
    async fn test_factory() {
        let cnt = Rc::new(Cell::new(0));
        let cnt2 = cnt.clone();
        let new_srv = factory(fn_factory(move |(): &()| {
            let cnt = cnt2.clone();
            async move { Ok::<_, ()>(Srv1(cnt, Rc::new(Cell::new(0)))) }
        }))
        .and_then(fn_factory(move |(): &()| {
            let cnt = cnt.clone();
            async move { Ok(Srv2(cnt.clone(), Rc::new(Cell::new(0)))) }
        }))
        .clone();

        let srv = new_srv.pipeline(()).await.unwrap();
        let res = srv.call("srv1").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv1", "srv2"));
    }

    mod ready_flag {
        use std::{cell::Cell, cell::RefCell, future::poll_fn, rc::Rc, task::Poll, task::Waker};

        use ntex::channel::oneshot::{self, Receiver};

        use crate::util::tests::{Req, Single, State, concurrent_entry, start, tick};
        use crate::{Ctx, Pipeline, Service, and_then::AndThen};

        #[derive(Default)]
        struct Gate {
            closed: Cell<bool>,
            waker: RefCell<Option<Waker>>,
        }

        /// Readiness is gated, a call waits for its first sender
        struct Svc1(Rc<Gate>);

        impl Service<(), Req> for Svc1 {
            type Res = Receiver<()>;
            type Error = ();

            async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), ()> {
                poll_fn(|cx| {
                    if self.0.closed.get() {
                        *self.0.waker.borrow_mut() = Some(cx.waker().clone());
                        Poll::Pending
                    } else {
                        Poll::Ready(Ok(()))
                    }
                })
                .await
            }

            async fn call(&self, (rx1, rx2): Req, _: Ctx<'_, Self>) -> Result<Receiver<()>, ()> {
                let _ = rx1.await;
                Ok(rx2)
            }
        }

        fn setup() -> (Rc<Gate>, Rc<State>, Pipeline<Req, (), ()>) {
            let gate = Rc::new(Gate::default());
            let st = Rc::new(State::default());
            let pl = Pipeline::new((), AndThen::new(Svc1(gate.clone()), Single(st.clone())));
            (gate, st, pl)
        }

        #[ntex::test]
        async fn cached_svc2_readiness() {
            let (gate, st, pl) = setup();
            let (a1, _a2) = start(&pl);
            tick().await; // A: ready, waits in svc1
            gate.closed.set(true);
            let (b1, _b2) = start(&pl);
            tick().await; // B: svc2 readiness is cached while svc1 is not ready
            let _ = b1.send(());
            let _ = a1.send(());
            tick().await; // A enters svc2
            assert_eq!(st.active.get(), 1);
            gate.closed.set(false);
            if let Some(w) = gate.waker.borrow_mut().take() {
                w.wake();
            }
            tick().await; // B: readiness completes, B leaves svc1
            assert_eq!(st.max.get(), 1, "svc2 capacity exceeded");
        }

        #[ntex::test]
        async fn concurrent_svc2_entry() {
            let (_gate, st, pl) = setup();
            concurrent_entry(&pl, &st).await;
        }

        #[ntex::test]
        async fn svc2_readiness_reused() {
            let (_gate, st, pl) = setup();
            for _ in 0..3 {
                let (tx1, rx1) = oneshot::channel();
                let (tx2, rx2) = oneshot::channel();
                let _ = tx1.send(());
                let _ = tx2.send(());
                pl.call((rx1, rx2)).await.unwrap();
            }
            assert_eq!(st.checks.get(), 3);
        }
    }
}
