use std::{cell::Cell, rc::Rc};

use super::*;
use crate::{Pipeline, fn_factory, service};

#[derive(Debug, Clone)]
struct Srv(bool, Rc<Cell<usize>>);

impl Service<(), ()> for Srv {
    type Res = ();
    type Error = ();

    async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
        if self.0 { Err(()) } else { Ok(()) }
    }

    async fn call(&self, _m: (), _: Ctx<'_, Self>) -> Result<(), ()> {
        Err(())
    }

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        self.1.set(self.1.get() + 1);
    }
}

/// STATEMENT: Error mapping transforms readiness errors without suppressing shutdown.
///
/// HOW:
/// - Wrap a service that fails readiness and map its unit error to a string.
/// - Check the mapped error and verify the inner shutdown counter increments.
#[ntex::test]
async fn test_ready() {
    let cnt_sht = Rc::new(Cell::new(0));
    let srv = Pipeline::new(
        (),
        service(Srv(true, cnt_sht.clone())).map_err(|()| "error"),
    );
    let res = srv.ready().await;
    assert_eq!(res, Err("error"));

    srv.shutdown().await;
    assert_eq!(cnt_sht.get(), 1);
}

/// STATEMENT: A cloned error-mapped service transforms call failures.
///
/// HOW:
/// - Clone a service that maps unit call errors to a string and create a pipeline.
/// - Verify the mapped call error and debug formatting.
#[ntex::test]
async fn test_service() {
    let srv = Pipeline::new(
        (),
        Srv(false, Rc::new(Cell::new(0)))
            .map_err(|()| "error")
            .clone(),
    );
    let res = srv.call(()).await;
    assert!(res.is_err());
    assert_eq!(res.err().unwrap(), "error");

    let _ = format!("{srv:?}");
}

/// STATEMENT: A service chain retains its error mapper when cloned into a pipeline.
///
/// HOW:
/// - Wrap and clone a failing service chain with an error mapper.
/// - Call the pipeline and check its transformed error and debug representation.
#[ntex::test]
async fn test_pipeline() {
    let srv = Pipeline::new(
        (),
        crate::service(Srv(false, Rc::new(Cell::new(0))))
            .map_err(|()| "error")
            .clone(),
    );
    let res = srv.call(()).await;
    assert!(res.is_err());
    assert_eq!(res.err().unwrap(), "error");

    let _ = format!("{srv:?}");
}

/// STATEMENT: An error-mapped factory applies its mapper to created service failures.
///
/// HOW:
/// - Create a pipeline from a factory whose service returns a unit error.
/// - Verify the call returns the mapped string error and the factory can be formatted.
#[ntex::test]
async fn test_factory() {
    let new_srv =
        crate::fn_factory(|(): &()| async { Ok::<_, ()>(Srv(false, Rc::new(Cell::new(0)))) })
            .map_err(|()| "error");
    let srv = Pipeline::new((), new_srv.create(&()).await.unwrap());
    let res = srv.call(()).await;
    assert!(res.is_err());
    assert_eq!(res.err().unwrap(), "error");
    let _ = format!("{new_srv:?}");
}

/// STATEMENT: Cloning an error-mapped factory preserves call-error conversion.
///
/// HOW:
/// - Clone the factory, create its service, and call it through a pipeline.
/// - Check the mapped error and factory debug formatting.
#[ntex::test]
async fn test_pipeline_factory() {
    let new_srv =
        fn_factory(|(): &()| async move { Ok::<Srv, ()>(Srv(false, Rc::new(Cell::new(0)))) })
            .map_err(|()| "error")
            .clone();
    let srv = Pipeline::new((), new_srv.create(&()).await.unwrap());
    let res = srv.call(()).await;
    assert!(res.is_err());
    assert_eq!(res.err().unwrap(), "error");
    let _ = format!("{new_srv:?}");
}
