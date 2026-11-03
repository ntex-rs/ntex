use std::{cell::Cell, task::Poll};

use ntex::util::lazy;

use super::*;
use crate::dev::FnServiceSt;
use crate::pipeline::PipelineFactory;

#[derive(Clone, Debug, Default)]
struct Srv(Rc<Cell<usize>>);

impl Service<usize, usize> for Srv {
    type Res = usize;
    type Error = &'static str;

    async fn ready(&self, _: Ctx<'_, Self, usize>) -> Result<(), Self::Error> {
        self.0.set(self.0.get() + 1);
        Ok(())
    }

    async fn call(&self, req: usize, ctx: Ctx<'_, Self, usize>) -> Result<usize, Self::Error> {
        if req == 0 { Err("zero") } else { Ok(req + *ctx) }
    }

    async fn shutdown(&self, _: Ctx<'_, Self, usize>) {
        self.0.set(self.0.get() + 100);
    }
}

struct Fwd {
    inner: Srv,
    pl: Pipeline<usize, usize, &'static str>,
}

impl Service<usize, usize> for Fwd {
    type Res = usize;
    type Error = usize;

    crate::forward_bind!(usize, inner);
    crate::forward_ready!(usize, inner, str::len);

    async fn call(&self, req: usize, ctx: Ctx<'_, Self, usize>) -> Result<usize, usize> {
        let res = ctx.call(&self.inner, req).await.map_err(str::len)?;
        self.pl.call(res).await.map_err(str::len)
    }

    crate::forward_shutdown!(usize, inner);
}

struct FwdPl {
    pl: Pipeline<usize, usize, &'static str>,
}

impl Service<usize, usize> for FwdPl {
    type Res = usize;
    type Error = &'static str;

    fn bind(&self, ctx: BindCtx<'_>) {
        self.pl.bind_to(&ctx);
    }

    async fn ready(&self, _: Ctx<'_, Self, usize>) -> Result<(), &'static str> {
        self.pl.ready().await
    }

    async fn shutdown(&self, _: Ctx<'_, Self, usize>) {
        self.pl.shutdown().await;
    }

    async fn call(&self, req: usize, _: Ctx<'_, Self, usize>) -> Result<usize, &'static str> {
        self.pl.call(req).await
    }
}

struct FwdPlErr {
    pl: Pipeline<usize, usize, &'static str>,
}

impl Service<usize, usize> for FwdPlErr {
    type Res = usize;
    type Error = usize;

    fn bind(&self, ctx: BindCtx<'_>) {
        self.pl.bind_to(&ctx);
    }

    async fn ready(&self, _: Ctx<'_, Self, usize>) -> Result<(), usize> {
        self.pl.ready().await.map_err(str::len)
    }

    async fn call(&self, req: usize, _: Ctx<'_, Self, usize>) -> Result<usize, usize> {
        self.pl.call(req).await.map_err(str::len)
    }
}

/// STATEMENT: Service wrappers and forwarding macros preserve calls, errors, and lifecycle delegation.
///
/// HOW:
/// - Exercise borrowed, boxed, reference-counted, service-forwarding, and pipeline-forwarding wrappers.
/// - Verify state-dependent responses, mapped errors, and readiness/shutdown counters.
#[ntex::test]
async fn service_wrappers() {
    let cnt = Rc::new(Cell::new(0));

    let srv: &'static Srv = Box::leak(Box::new(Srv(cnt.clone())));
    let pl = Pipeline::new(1, srv);
    assert_eq!(pl.call(1).await, Ok(2));
    assert_eq!(pl.call(0).await, Err("zero"));
    pl.shutdown().await;
    assert_eq!(cnt.get(), 101);

    let pl = Pipeline::new(2, Box::new(Srv(cnt.clone())));
    assert_eq!(pl.call(1).await, Ok(3));
    pl.shutdown().await;
    assert_eq!(cnt.get(), 202);

    let pl = Pipeline::new(3, Rc::new(Srv(cnt.clone())));
    assert_eq!(pl.call(1).await, Ok(4));
    pl.shutdown().await;
    assert_eq!(cnt.get(), 303);

    let pl = Pipeline::new(
        1,
        Fwd {
            inner: Srv(cnt.clone()),
            pl: Pipeline::new(10, Srv(cnt.clone())),
        },
    );
    assert_eq!(pl.call(1).await, Ok(12));
    assert_eq!(pl.call(0).await, Err(4));
    pl.shutdown().await;
    assert_eq!(cnt.get(), 405);

    let pl = Pipeline::new(
        1,
        FwdPl {
            pl: Pipeline::new(10, Srv(cnt.clone())),
        },
    );
    assert_eq!(pl.call(1).await, Ok(11));
    pl.shutdown().await;
    // inner pipeline readiness is checked once, by `FwdPl::ready`
    assert_eq!(cnt.get(), 506);

    let pl = Pipeline::new(
        1,
        FwdPlErr {
            pl: Pipeline::new(10, Srv(cnt.clone())),
        },
    );
    assert_eq!(pl.call(0).await, Err(4));
    pl.shutdown().await;
    assert_eq!(cnt.get(), 507);
}

struct NotReady;

impl Service<usize, usize> for NotReady {
    type Res = usize;
    type Error = &'static str;

    async fn ready(&self, _: Ctx<'_, Self, usize>) -> Result<(), Self::Error> {
        Err("down")
    }

    async fn call(&self, req: usize, _: Ctx<'_, Self, usize>) -> Result<usize, Self::Error> {
        Ok(req)
    }
}

struct FwdAll(ReadyBinding<Srv>);

impl Service<usize, usize> for FwdAll {
    type Res = usize;
    type Error = &'static str;

    crate::forward!(usize, 0);

    async fn call(&self, req: usize, ctx: Ctx<'_, Self, usize>) -> Result<usize, Self::Error> {
        ctx.call(&self.0, req).await
    }
}

struct FwdAllErr {
    inner: NotReady,
}

impl Service<usize, usize> for FwdAllErr {
    type Res = usize;
    type Error = usize;

    crate::forward!(usize, inner, map_err = str::len);

    async fn call(&self, req: usize, ctx: Ctx<'_, Self, usize>) -> Result<usize, usize> {
        ctx.call(&self.inner, req).await.map_err(str::len)
    }
}

struct FwdMany {
    a: ReadyBinding<Srv>,
    b: ReadyBinding<Srv>,
    c: Srv,
}

impl Service<usize, usize> for FwdMany {
    type Res = usize;
    type Error = &'static str;

    crate::forward!(usize, a, b, c);

    async fn call(&self, req: usize, ctx: Ctx<'_, Self, usize>) -> Result<usize, Self::Error> {
        let res = ctx.call(&self.a, req).await?;
        ctx.call(&self.b, res).await
    }
}

struct FwdManyTuple(ReadyBinding<Srv>, ReadyBinding<Srv>);

impl Service<usize, usize> for FwdManyTuple {
    type Res = usize;
    type Error = &'static str;

    crate::forward!(usize, 0, 1,);

    async fn call(&self, req: usize, ctx: Ctx<'_, Self, usize>) -> Result<usize, Self::Error> {
        let res = ctx.call(&self.0, req).await?;
        ctx.call(&self.1, res).await
    }
}

struct FwdManyErr {
    a: Srv,
    b: NotReady,
}

impl Service<usize, usize> for FwdManyErr {
    type Res = usize;
    type Error = usize;

    crate::forward!(usize, a, b, map_err = str::len);

    async fn call(&self, req: usize, ctx: Ctx<'_, Self, usize>) -> Result<usize, usize> {
        ctx.call(&self.b, req).await.map_err(str::len)
    }
}

/// STATEMENT: `forward!` wires bind, readiness, error mapping, and shutdown for one or several service fields.
///
/// HOW:
/// - Call through a `ReadyBinding` tuple field, which panics if `bind` is not forwarded.
/// - Repeat with several named and tuple fields, checking that every field is readied and shut down.
/// - Check readiness and shutdown counters, and verify mapped readiness errors.
#[ntex::test]
async fn forward_macro() {
    let cnt = Rc::new(Cell::new(0));

    let pl = Pipeline::new(1, FwdAll(ReadyBinding::new(Srv(cnt.clone()))));
    assert_eq!(pl.call(1).await, Ok(2));
    assert_eq!(cnt.get(), 1);
    pl.shutdown().await;
    assert_eq!(cnt.get(), 101);

    let pl = Pipeline::new(1, FwdAllErr { inner: NotReady });
    assert_eq!(pl.call(1).await, Err(4));

    let cnt = Rc::new(Cell::new(0));
    let pl = Pipeline::new(
        1,
        FwdMany {
            a: ReadyBinding::new(Srv(cnt.clone())),
            b: ReadyBinding::new(Srv(cnt.clone())),
            c: Srv(cnt.clone()),
        },
    );
    assert_eq!(pl.call(1).await, Ok(3));
    assert_eq!(cnt.get(), 3);
    pl.shutdown().await;
    assert_eq!(cnt.get(), 303);

    let cnt = Rc::new(Cell::new(0));
    let pl = Pipeline::new(
        1,
        FwdManyTuple(
            ReadyBinding::new(Srv(cnt.clone())),
            ReadyBinding::new(Srv(cnt.clone())),
        ),
    );
    assert_eq!(pl.call(1).await, Ok(3));
    assert_eq!(cnt.get(), 2);
    pl.shutdown().await;
    assert_eq!(cnt.get(), 202);

    let pl = Pipeline::new(
        1,
        FwdManyErr {
            a: Srv(cnt.clone()),
            b: NotReady,
        },
    );
    assert_eq!(pl.call(1).await, Err(4));
}

/// STATEMENT: Pipeline call entry points share cached readiness until shutdown invalidates it.
///
/// HOW:
/// - Mix owned, borrowed, bound, and trait-based calls, including reversed polling order.
/// - Check a single readiness count, then shut down and verify the next call rechecks.
#[ntex::test]
async fn pipeline_calls() {
    let cnt = Rc::new(Cell::new(0));
    let pl = Srv(cnt.clone()).pipeline(1);

    assert_eq!(pl.call_static(1).await, Ok(2));
    assert_eq!(cnt.get(), 1);

    // all entry points reuse the same successful readiness check
    assert_eq!(pl.ready().await, Ok(()));
    assert_eq!(cnt.get(), 1);
    assert_eq!(pl.call(2).await, Ok(3));
    assert_eq!(cnt.get(), 1);
    assert_eq!(ServiceCaller::call_service(&pl, 3).await, Ok(4));
    assert_eq!(cnt.get(), 1);

    assert_eq!(lazy(|cx| pl.poll_ready(cx)).await, Poll::Ready(Ok(())));
    assert_eq!(cnt.get(), 1);
    assert_eq!(pl.call_static(3).await, Ok(4));
    assert_eq!(pl.call_static(3).await, Ok(4));
    assert_eq!(cnt.get(), 1);

    let b = pl.bind();
    assert!(format!("{b:?}").contains("PipelineBinding"));
    assert_eq!(b.call_static(4).await, Ok(5));
    assert_eq!(cnt.get(), 1);
    assert_eq!(b.ready().await, Ok(()));
    assert_eq!(cnt.get(), 1);
    assert_eq!(pl.call(4).await, Ok(5));
    assert_eq!(b.call(4).await, Ok(5));
    assert_eq!(cnt.get(), 1);

    // owned calls also share readiness regardless of polling order
    assert_eq!(pl.ready().await, Ok(()));
    let fut1 = pl.call_static(1);
    let fut2 = pl.call_static(2);
    assert_eq!(fut2.await, Ok(3));
    assert_eq!(cnt.get(), 1);
    assert_eq!(fut1.await, Ok(2));
    assert_eq!(cnt.get(), 1);

    // shutdown invalidates cached readiness
    assert_eq!(pl.ready().await, Ok(()));
    assert_eq!(cnt.get(), 1);
    pl.shutdown().await;
    assert_eq!(pl.call(1).await, Ok(2));
    assert_eq!(cnt.get(), 102);

    let svc = apply_fn(
        Srv(cnt.clone()),
        async |req: usize, svc: &dev::ApplyCtx<'_, Srv, usize, usize>| {
            svc.call_service(req * 2).await
        },
    );
    let pl = Pipeline::new(1, svc);
    assert_eq!(pl.call(2).await, Ok(5));
    assert_eq!(cnt.get(), 103);
}

/// STATEMENT: Async closures convert into state-aware services and configuration-based factories.
///
/// HOW:
/// - Call a pipeline built directly from a state-aware closure and exercise `IntoService` conversion.
/// - Create a pipeline from an async factory and check its state-dependent response.
#[ntex::test]
async fn fn_conversions() {
    let pl = Pipeline::new(5, async |st: &usize, req: usize| Ok::<_, ()>(st + req));
    assert_eq!(pl.call(1).await, Ok(6));

    let _: FnServiceSt<_, usize, usize, usize, ()> =
        IntoService::into_service(async |st: &usize, req: usize| Ok::<_, ()>(st + req));

    let f = factory(async |st: &usize| Ok::<_, ()>(Srv(Rc::new(Cell::new(*st)))));
    let pl = f.pipeline(1).await.unwrap();
    assert_eq!(pl.call(1).await, Ok(2));
}

/// STATEMENT: Factory combinators preserve construction errors and transform created service behavior.
///
/// HOW:
/// - Exercise reference-counted, initialization-error-mapped, chained, and response/error-mapped factories.
/// - Check resulting responses and failures, including cloned `PipelineFactory` construction.
#[ntex::test]
async fn factory_combinators() {
    let cnt = Rc::new(Cell::new(0));
    let c = cnt.clone();
    let f = fn_factory(async move |st: &usize| {
        if *st == 0 {
            Err(())
        } else {
            Ok::<_, ()>(Srv(c.clone()))
        }
    });

    let rc = Rc::new(f.clone());
    let pl = rc.pipeline(1).await.unwrap();
    assert_eq!(pl.call(1).await, Ok(2));
    assert!(rc.pipeline(0).await.is_err());

    let f2 = f.clone().map_init_err(|()| "init");
    assert_eq!(f2.create(&0).await.err(), Some("init"));

    let f2 = f.clone().and_then(f.clone());
    let pl = f2.pipeline(1).await.unwrap();
    assert_eq!(pl.call(1).await, Ok(3));

    let f2 = factory(f.clone()).map(|r| r * 10).map_err(|_| ());
    let pl = f2.pipeline(1).await.unwrap();
    assert_eq!(pl.call(1).await, Ok(20));
    assert_eq!(pl.call(0).await, Err(()));

    let pf = PipelineFactory::new(f.clone());
    let pf2 = pf.clone();
    assert!(format!("{pf2:?}").contains("PipelineFactory"));
    let pl = pf2.create(2).await.unwrap();
    assert_eq!(pl.call(1).await, Ok(3));
    assert!(pf.create(0).await.is_err());
}

/// STATEMENT: Boxed services and factories retain call behavior and combinator debug descriptions.
///
/// HOW:
/// - Clone boxed service and factory handles and call their pipelines.
/// - Verify responses and the `Map`/`MapErr` names in a composed debug representation.
#[ntex::test]
async fn boxed_and_debug() {
    let cnt = Rc::new(Cell::new(0));

    let svc = boxed::service(Srv(cnt.clone()));
    let pl = Pipeline::new(1, svc.clone());
    assert_eq!(pl.call(1).await, Ok(2));

    let f = boxed::factory(fn_factory(async |_: &usize| Ok::<_, ()>(Srv::default())));
    let pl = f.clone().pipeline(1).await.unwrap();
    assert_eq!(pl.call(1).await, Ok(2));

    let s = format!("{:?}", service(Srv::default()).map(|r| r).map_err(|e| e));
    assert!(s.contains("Map") && s.contains("MapErr"));
}

/// STATEMENT: State wrappers and state/request tuples unpack in state-first order.
///
/// HOW:
/// - Unpack an explicit State value and a tuple representation.
/// - Compare both results with their expected state and request pairs.
#[test]
fn request_state() {
    let st = State {
        req: 1,
        state: "st",
    };
    assert_eq!(st.unpack(), ("st", 1));
    assert_eq!(("st", 2).unpack(), ("st", 2));
}
