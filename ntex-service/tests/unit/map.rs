use std::{cell::Cell, rc::Rc};

use crate::{Ctx, Pipeline, Service, ServiceFactory, fn_factory, service};

#[derive(Debug, Default, Clone)]
struct Srv(Rc<Cell<usize>>);

impl Service<(), ()> for Srv {
    type Res = ();
    type Error = ();

    async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn call(&self, _r: (), _: Ctx<'_, Self>) -> Result<(), ()> {
        Ok(())
    }

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        self.0.set(self.0.get() + 1);
    }
}

/// STATEMENT: Response mapping preserves readiness and shutdown for chained and direct services.
///
/// HOW:
/// - Map unit responses to a string using both service construction forms.
/// - Check mapped responses, readiness, debug output, and one shutdown per inner service.
#[ntex::test]
async fn test_service() {
    let cnt_sht = Rc::new(Cell::new(0));
    let srv = Pipeline::new((), service(Srv(cnt_sht.clone())).map(|()| "ok").clone());
    let res = srv.call(()).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), "ok");

    let res = srv.ready().await;
    assert_eq!(res, Ok(()));

    srv.shutdown().await;
    assert_eq!(cnt_sht.get(), 1);
    let _ = format!("{srv:?}");

    let cnt_sht = Rc::new(Cell::new(0));
    let svc = Srv(cnt_sht.clone()).map(|()| "ok");
    let srv = Pipeline::new((), svc);
    let res = srv.call(()).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), "ok");

    let res = srv.ready().await;
    assert_eq!(res, Ok(()));

    srv.shutdown().await;
    assert_eq!(cnt_sht.get(), 1);
    let _ = format!("{srv:?}");
}

/// STATEMENT: A pipeline built from a cloned mapped service returns the mapped response.
///
/// HOW:
/// - Clone a mapped service chain and construct its pipeline.
/// - Verify the response transformation and successful readiness.
#[ntex::test]
async fn test_pipeline() {
    let srv = Pipeline::new((), crate::service(Srv::default()).map(|()| "ok").clone());
    let res = srv.call(()).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), "ok");

    let res = srv.ready().await;
    assert_eq!(res, Ok(()));
}

/// STATEMENT: Mapping a factory transforms responses of its created service.
///
/// HOW:
/// - Clone a response-mapped function factory and create a pipeline.
/// - Call the service and check the mapped value and factory debug formatting.
#[ntex::test]
async fn test_factory() {
    let new_srv = fn_factory(|(): &()| async { Ok::<_, ()>(Srv::default()) })
        .map(|()| "ok")
        .clone();
    let srv = Pipeline::new((), new_srv.create(&()).await.unwrap());
    let res = srv.call(()).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), ("ok"));

    let _ = format!("{new_srv:?}");
}

/// STATEMENT: A cloned mapped factory retains response mapping in a new pipeline.
///
/// HOW:
/// - Create a pipeline from a cloned function factory with a response mapper.
/// - Verify its mapped response and the factory's debug representation.
#[ntex::test]
async fn test_pipeline_factory() {
    let new_srv = crate::fn_factory(|(): &()| async { Ok::<_, ()>(Srv::default()) })
        .map(|()| "ok")
        .clone();
    let srv = Pipeline::new((), new_srv.create(&()).await.unwrap());
    let res = srv.call(()).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), ("ok"));

    let _ = format!("{new_srv:?}");
}
