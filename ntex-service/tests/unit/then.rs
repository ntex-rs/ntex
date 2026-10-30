use std::{cell::Cell, rc::Rc};

use crate::{Ctx, Service, factory, fn_factory, service};

#[derive(Clone)]
struct Srv1(Rc<Cell<usize>>, Rc<Cell<usize>>);

impl Service<(), Result<&'static str, &'static str>> for Srv1 {
    type Res = &'static str;
    type Error = ();

    async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
        self.0.set(self.0.get() + 1);
        Ok(())
    }

    async fn call(
        &self,
        req: Result<&'static str, &'static str>,
        _: Ctx<'_, Self>,
    ) -> Result<&'static str, ()> {
        match req {
            Ok(msg) => Ok(msg),
            Err(_) => Err(()),
        }
    }

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        self.1.set(self.1.get() + 1);
    }
}

#[derive(Clone)]
struct Srv2(Rc<Cell<usize>>, Rc<Cell<usize>>);

impl Service<(), Result<&'static str, ()>> for Srv2 {
    type Res = (&'static str, &'static str);
    type Error = ();

    async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
        self.0.set(self.0.get() + 1);
        Ok(())
    }

    async fn call(&self, req: Result<&'static str, ()>, _: Ctx<'_, Self>) -> Result<Self::Res, ()> {
        match req {
            Ok(msg) => Ok((msg, "ok")),
            Err(()) => Ok(("srv2", "err")),
        }
    }

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        self.1.set(self.1.get() + 1);
    }
}

/// STATEMENT: A then chain checks readiness and shuts down both stages.
///
/// HOW:
/// - Compose two services with shared lifecycle counters.
/// - Await readiness and shutdown, verifying each counter increments twice.
#[ntex::test]
async fn test_ready() {
    let cnt = Rc::new(Cell::new(0));
    let cnt_sht = Rc::new(Cell::new(0));
    let srv = service(Srv1(cnt.clone(), cnt_sht.clone()))
        .then(Srv2(cnt.clone(), cnt_sht.clone()))
        .pipeline(());
    let res = srv.ready().await;
    assert_eq!(res, Ok(()));
    assert_eq!(cnt.get(), 2);

    srv.shutdown().await;
    assert_eq!(cnt_sht.get(), 2);
}

/// STATEMENT: A then chain passes both successful and failed results to its second stage.
///
/// HOW:
/// - Call a cloned chain with requests that make the first stage succeed and fail.
/// - Verify the second stage's distinct responses for both result variants.
#[ntex::test]
async fn test_call() {
    let cnt = Rc::new(Cell::new(0));
    let srv = service(Srv1(cnt.clone(), Rc::new(Cell::new(0))))
        .then(Srv2(cnt, Rc::new(Cell::new(0))))
        .clone()
        .pipeline(());

    let res = srv.call(Ok("srv1")).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), ("srv1", "ok"));

    let res = srv.call(Err("srv")).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), ("srv2", "err"));
}

/// STATEMENT: Factory-created then chains handle both result variants and cache readiness per pipeline.
///
/// HOW:
/// - Call the first pipeline with successful and failing first-stage requests.
/// - Verify two readiness checks are reused, then create another pipeline and observe two more.
#[ntex::test]
async fn test_factory() {
    let cnt = Rc::new(Cell::new(0));
    let checks = cnt.clone();
    let cnt2 = cnt.clone();
    let blank = fn_factory(move |(): &_| {
        let cnt = cnt2.clone();
        async move { Ok::<_, ()>(Srv1(cnt, Rc::new(Cell::new(0)))) }
    });
    let factory = factory(blank)
        .then(fn_factory(move |(): &()| {
            let cnt = cnt.clone();
            async move { Ok(Srv2(cnt.clone(), Rc::new(Cell::new(0)))) }
        }))
        .clone();
    let srv = factory.pipeline(()).await.unwrap();
    let res = srv.call(Ok("srv1")).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), ("srv1", "ok"));

    let res = srv.call(Err("srv")).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), ("srv2", "err"));
    assert_eq!(checks.get(), 2);
    let other = factory.pipeline(()).await.unwrap();
    assert!(other.call(Ok("fresh")).await.is_ok());
    assert_eq!(checks.get(), 4);
}

/// STATEMENT: A then chain respects second-stage capacity across concurrent first-stage completions.
///
/// HOW:
/// - Delay first-stage calls and adapt their results into a single-capacity second stage.
/// - Interleave three calls and verify the second stage never runs more than one at a time.
#[ntex::test]
async fn concurrent_second_stage_entry() {
    use ntex::channel::oneshot::Receiver;

    use crate::util::tests::{Req, Single, State, concurrent_entry};

    let st = Rc::new(State::default());
    let srv = service(crate::fn_service(async |(rx1, rx2): Req| {
        let _ = rx1.await;
        Ok::<_, ()>(rx2)
    }))
    .then(crate::apply_fn(
        crate::ready(Single(st.clone())),
        async |result: Result<Receiver<()>, ()>, ctx| ctx.call(result?).await,
    ))
    .pipeline(());
    concurrent_entry(&srv, &st).await;
}
