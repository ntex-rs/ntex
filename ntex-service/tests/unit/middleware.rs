use std::{cell::Cell, rc::Rc};

use super::*;
use crate::{Ctx, Pipeline, factory, fn_service};

#[derive(Debug, Clone)]
struct Mw(Rc<Cell<usize>>);

impl<S, St> Middleware<S, St> for Mw {
    type Service = Srv<S>;

    fn create(&self, _: &St, service: S) -> Self::Service {
        self.0.set(self.0.get() + 1);
        Srv(service, self.0.clone())
    }
}

#[derive(Debug, Clone)]
struct Srv<S>(S, Rc<Cell<usize>>);

impl<S: Service<(), R>, R> Service<(), R> for Srv<S> {
    type Res = S::Res;
    type Error = S::Error;

    crate::forward_bind!((), 0);

    async fn ready(&self, ctx: Ctx<'_, Self>) -> Result<(), Self::Error> {
        ctx.ready(&self.0).await
    }

    async fn call(&self, req: R, ctx: Ctx<'_, Self>) -> Result<S::Res, S::Error> {
        ctx.call(&self.0, req).await
    }

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        self.1.set(self.1.get() + 1);
    }
}

/// STATEMENT: Reference-counted middleware preserves service behavior through both apply forms.
///
/// HOW:
/// - Build cloned factories with the apply function and factory apply method.
/// - Verify doubled responses, readiness, formatting, and creation/shutdown counts.
#[ntex::test]
async fn middleware() {
    let cnt_sht = Rc::new(Cell::new(0));
    let fac = apply(
        Rc::new(Mw(cnt_sht.clone()).clone()),
        fn_service(|i: usize| async move { Ok::<_, ()>(i * 2) }),
    )
    .clone();

    let srv = Pipeline::new((), fac.create(&()).await.unwrap().clone());
    let res = srv.call(10).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), 20);
    let _ = format!("{fac:?} {srv:?}");

    assert_eq!(srv.ready().await, Ok(()));
    srv.shutdown().await;
    assert_eq!(cnt_sht.get(), 2);

    let fac = factory(fn_service(|i: usize| async move { Ok::<_, ()>(i * 2) }))
        .apply(Rc::new(Mw(Rc::new(Cell::new(0))).clone()))
        .clone();

    let srv = Pipeline::new((), fac.create(&()).await.unwrap().clone());
    let res = srv.call(10).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), 20);
    let _ = format!("{fac:?} {srv:?}");

    assert_eq!(srv.ready().await, Ok(()));
}

/// STATEMENT: Middleware applied to a boxed factory retains lifecycle forwarding.
///
/// HOW:
/// - Apply counted middleware to a function factory and box the result.
/// - Check its response, readiness, and one creation plus one shutdown.
#[ntex::test]
async fn middleware_apply() {
    let cnt_sht = Rc::new(Cell::new(0));
    let fac = Mw(cnt_sht.clone())
        .apply_to(factory(async |i: usize| Ok::<_, ()>(i * 2)))
        .boxed();

    let srv = Pipeline::new((), fac.create(&()).await.unwrap());
    let res = srv.call(10).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), 20);
    let _ = format!("{fac:?} {srv:?}");

    assert_eq!(srv.ready().await, Ok(()));
    srv.shutdown().await;
    assert_eq!(cnt_sht.get(), 2);
}

/// STATEMENT: Factory middleware chains preserve calls and lifecycle behavior after cloning.
///
/// HOW:
/// - Apply cloned middleware to a factory and clone the resulting service.
/// - Verify its response, readiness, formatting, and creation/shutdown count.
#[ntex::test]
async fn middleware_chain() {
    let cnt_sht = Rc::new(Cell::new(0));
    let fac = factory(fn_service(async move |i: usize| Ok::<_, ()>(i * 2)))
        .apply(Mw(cnt_sht.clone()).clone());

    let srv = Pipeline::new((), fac.create(&()).await.unwrap().clone());
    let res = srv.call(10).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), 20);
    let _ = format!("{fac:?} {srv:?}");

    assert_eq!(srv.ready().await, Ok(()));
    srv.shutdown().await;
    assert_eq!(cnt_sht.get(), 2);
}

/// STATEMENT: Identity middleware composes with a counted middleware layer without changing responses.
///
/// HOW:
/// - Create a service through a stack containing Identity and counted middleware.
/// - Check its call result, readiness, and creation/shutdown counter.
#[ntex::test]
async fn stack() {
    let cnt_sht = Rc::new(Cell::new(0));
    let mw = Stack::new(Identity, Mw(cnt_sht.clone()));
    let _ = format!("{mw:?}");

    let pl = Pipeline::new(
        (),
        Middleware::create(
            &mw,
            &(),
            fn_service(|i: usize| async move { Ok::<_, ()>(i * 2) }),
        ),
    );
    let res = pl.call(10).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), 20);
    assert_eq!(pl.ready().await, Ok(()));
    pl.shutdown().await;
    assert_eq!(cnt_sht.get(), 2);
}

/// STATEMENT: Function middleware can adapt requests and responses around an inner service.
///
/// HOW:
/// - Clone a layer whose closure calls the inner service and combines its result with the request.
/// - Verify the combined response, lifecycle completion, and one closure invocation.
#[ntex::test]
async fn fn_middleware_service() {
    let cnt_sht = Rc::new(Cell::new(0));
    let cnt_sht2 = cnt_sht.clone();
    let mw = fn_layer(async move |req: &'static str, svc| {
        cnt_sht2.set(cnt_sht2.get() + 1);
        let result = svc.call(1).await?;
        Ok::<_, ()>((req, result))
    })
    .clone();
    let _ = format!("{mw:?}");

    let svc = Pipeline::new(
        (),
        mw.create(&(), fn_service(async move |i: usize| Ok::<_, ()>(i * 2))),
    );

    let res = svc.call("test").await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), ("test", 2));
    let _ = format!("{svc:?}");

    assert_eq!(svc.ready().await, Ok(()));
    svc.shutdown().await;
    assert_eq!(cnt_sht.get(), 1);
}
