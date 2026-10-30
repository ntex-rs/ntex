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

/// STATEMENT: An and-then chain forwards readiness and shutdown to both services.
///
/// HOW:
/// - Compose reference-counted and boxed services with shared counters.
/// - Check readiness and shutdown each increment their counter twice.
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

/// STATEMENT: Boxing an and-then chain preserves both readiness checks.
///
/// HOW:
/// - Box a chain of two services sharing a readiness counter.
/// - Await readiness and verify both services were checked.
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

/// STATEMENT: An and-then chain passes the first response to the second service.
///
/// HOW:
/// - Call a two-service chain and check the composed response.
/// - Shut it down and verify both services receive shutdown.
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

/// STATEMENT: Factory-created chains cache readiness independently per pipeline.
///
/// HOW:
/// - Create a chain from two factories and call it twice without invalidation.
/// - Verify readiness is reused, then create another pipeline and check both services again.
#[ntex::test]
async fn test_factory() {
    let cnt = Rc::new(Cell::new(0));
    let checks = cnt.clone();
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
    assert_eq!(checks.get(), 2);
    assert!(srv.call("again").await.is_ok());
    assert_eq!(checks.get(), 2);
    let other = new_srv.pipeline(()).await.unwrap();
    assert!(other.call("fresh").await.is_ok());
    assert_eq!(checks.get(), 4);
}

mod readiness {
    use std::{cell::Cell, cell::RefCell, future::poll_fn, rc::Rc, task::Poll, task::Waker};

    use ntex::channel::oneshot::{self, Receiver};

    use crate::util::tests::{Req, Single, State, concurrent_entry, start, tick};
    use crate::{Ctx, Pipeline, ReadyNotifier, Service, and_then::AndThen, ready};

    #[derive(Default)]
    struct Gate {
        changed: ReadyNotifier,
        closed: Cell<bool>,
        waker: RefCell<Option<Waker>>,
    }

    /// Readiness is gated, a call waits for its first sender
    struct Svc1(Rc<Gate>);

    impl Service<(), Req> for Svc1 {
        type Res = Receiver<()>;
        type Error = ();

        fn bind(&self, ctx: crate::BindCtx<'_>) {
            ctx.subscribe(&self.0.changed);
        }

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
        let pl = Pipeline::new(
            (),
            AndThen::new(ready(Svc1(gate.clone())), ready(Single(st.clone()))),
        );
        (gate, st, pl)
    }

    /// STATEMENT: Cached downstream readiness cannot bypass capacity after another call enters.
    ///
    /// HOW:
    /// - Park calls at a gated first service while the second service's readiness is cached.
    /// - Release the first call, reopen the gate, and verify downstream concurrency stays at one.
    #[ntex::test]
    async fn cached_svc2_readiness() {
        let (gate, st, pl) = setup();
        let (a1, _a2) = start(&pl);
        tick().await; // A: ready, waits in svc1
        gate.closed.set(true);
        gate.changed.notify();
        let (b1, _b2) = start(&pl);
        tick().await; // B: svc2 readiness is cached while svc1 is not ready
        let _ = b1.send(());
        let _ = a1.send(());
        tick().await; // A enters svc2
        assert_eq!(st.active.get(), 1);
        gate.closed.set(false);
        gate.changed.notify();
        if let Some(w) = gate.waker.borrow_mut().take() {
            w.wake();
        }
        tick().await; // B: readiness completes, B leaves svc1
        assert_eq!(st.max.get(), 1, "svc2 capacity exceeded");
    }

    /// STATEMENT: Concurrent and-then calls respect downstream readiness after the first step.
    ///
    /// HOW:
    /// - Interleave three calls through a waiting first step and a single-capacity second service.
    /// - Release the steps in competing order and verify downstream concurrency never exceeds one.
    #[ntex::test]
    async fn concurrent_svc2_entry() {
        let (_gate, st, pl) = setup();
        concurrent_entry(&pl, &st).await;
    }

    /// STATEMENT: Sequential and-then calls recheck downstream readiness after invalidation.
    ///
    /// HOW:
    /// - Complete three calls through a downstream service that notifies on entry and exit.
    /// - Verify the downstream readiness counter records one check per call.
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
