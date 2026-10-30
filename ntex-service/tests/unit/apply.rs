use std::{cell::Cell, rc::Rc};

use super::*;
use crate::{factory, fn_factory, service};

#[derive(Debug, Default, Clone)]
struct Srv(Rc<Cell<usize>>, Rc<Cell<usize>>);

impl Service<(), ()> for Srv {
    type Res = ();
    type Error = ();

    fn bind(&self, _: crate::BindCtx<'_>) {
        self.1.set(self.1.get() + 1);
    }

    async fn call(&self, _r: (), _: Ctx<'_, Self>) -> Result<(), ()> {
        Ok(())
    }

    async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), ()> {
        self.0.set(self.0.get() + 1);
        Ok(())
    }

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        self.0.set(self.0.get() + 1);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Err;

impl From<()> for Err {
    fn from(_e: ()) -> Self {
        Err
    }
}

/// STATEMENT: An apply wrapper adapts calls while forwarding binding, readiness, and shutdown.
///
/// HOW:
/// - Wrap a counted service with a closure that calls it and constructs a tuple response.
/// - Verify pipeline construction binds the inner service once, before readiness is checked.
/// - Check readiness/shutdown counts and the adapted response without additional binding.
#[ntex::test]
async fn test_call() {
    let cnt_sht = Rc::new(Cell::new(0));
    let binds = Rc::new(Cell::new(0));
    let srv = service(
        apply_fn(
            Srv(cnt_sht.clone(), binds.clone()),
            async move |req: &'static str, svc| {
                svc.call(()).await.unwrap();
                Ok((req, ()))
            },
        )
        .clone(),
    )
    .pipeline(());

    assert_eq!(binds.get(), 1);
    assert_eq!(cnt_sht.get(), 0);
    assert_eq!(srv.ready().await, Ok::<_, Err>(()));

    srv.shutdown().await;
    assert_eq!(cnt_sht.get(), 2);

    let res = srv.call("srv").await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), ("srv", ()));
    assert_eq!(binds.get(), 1);
}

/// STATEMENT: A cloned apply chain retains service access and lifecycle forwarding.
///
/// HOW:
/// - Build and clone an apply chain whose closure accesses state and calls the inner service.
/// - Check its debug representation, readiness and shutdown counts, and response.
#[ntex::test]
async fn test_call_svc() {
    let cnt_sht = Rc::new(Cell::new(0));
    let srv = service(Srv(cnt_sht.clone(), Rc::default()))
        .apply_fn(async move |req: &'static str, svc| {
            svc.st();
            svc.call(()).await.unwrap();
            Ok((req, ()))
        })
        .clone();
    let s = format!("{srv:?}");
    assert!(s.contains("Apply"), "{}", s);

    let srv = srv.pipeline(());
    assert_eq!(srv.ready().await, Ok::<_, Err>(()));

    srv.shutdown().await;
    assert_eq!(cnt_sht.get(), 2);

    let res = srv.call("srv").await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), ("srv", ()));
    let _ = format!("{srv:?}");
}

/// STATEMENT: An apply factory constructs a service with adapted requests and errors.
///
/// HOW:
/// - Wrap a service factory, create its pipeline, and check readiness.
/// - Call it and verify the adapted response and unit-error conversion.
#[ntex::test]
async fn test_create() {
    let new_srv = factory(apply_fn_factory(
        fn_factory(|(): &()| async { Ok::<_, ()>(Srv::default()) }),
        async move |req: &'static str, srv| {
            srv.call(()).await.unwrap();
            Ok((req, ()))
        },
    ));

    let srv = new_srv.pipeline(()).await.unwrap();

    assert_eq!(srv.ready().await, Ok::<_, Err>(()));

    let res = srv.call("srv").await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), ("srv", ()));
    assert_eq!(Err, Err::from(()));
}

/// STATEMENT: Factory apply chains retain readiness caching separately for each pipeline.
///
/// HOW:
/// - Clone an apply factory and call its first pipeline after checking readiness.
/// - Create another pipeline and verify only it adds a new readiness check.
#[ntex::test]
async fn test_create_chain() {
    let checks = Rc::new(Cell::new(0));
    let created = checks.clone();
    let new_srv = factory(fn_factory(move |(): &()| {
        let created = created.clone();
        async move { Ok::<_, ()>(Srv(created, Rc::default())) }
    }))
    .apply_fn(async move |req: &'static str, srv| {
        srv.call(()).await.unwrap();
        Ok((req, ()))
    })
    .clone();

    let srv = new_srv.pipeline(()).await.unwrap();

    assert_eq!(srv.ready().await, Ok::<_, Err>(()));

    let res = srv.call("srv").await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), ("srv", ()));
    assert_eq!(checks.get(), 1);
    let other = new_srv.pipeline(()).await.unwrap();
    assert!(other.call("fresh").await.is_ok());
    assert_eq!(checks.get(), 2);
    let _ = format!("{new_srv:?}");
}

mod readiness {
    use std::rc::Rc;

    use crate::util::tests::{Req, Single, State, concurrent_entry};
    use crate::{Pipeline, apply::Apply};

    /// STATEMENT: An apply closure rechecks inner readiness after awaiting other work.
    ///
    /// HOW:
    /// - Delay each closure before it calls a single-capacity service.
    /// - Interleave competing calls and verify the inner service never exceeds capacity.
    #[ntex::test]
    async fn concurrent_entry_after_await() {
        let st = Rc::new(State::default());
        let pl = Pipeline::new(
            (),
            Apply::new(
                crate::ready(Single(st.clone())),
                async |(rx1, rx2): Req, svc| {
                    let _ = rx1.await;
                    svc.call(rx2).await
                },
            ),
        );
        concurrent_entry(&pl, &st).await;
    }
}
