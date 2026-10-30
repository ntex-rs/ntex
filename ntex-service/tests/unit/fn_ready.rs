use std::{cell::Cell, rc::Rc};

use crate::{Pipeline, factory, service};

use super::*;

/// STATEMENT: Readiness closures run for both wrapped services and factory-created services.
///
/// HOW:
/// - Attach a flag-setting readiness closure to each construction form.
/// - Call and check readiness, verifying the response and that the closure ran.
#[ntex::test]
async fn test_fn_readiness() {
    // Service
    let is_called = Rc::new(Cell::new(false));
    let is_called2 = is_called.clone();

    let svc = service(async |()| Ok::<_, ()>("pipe")).readiness(async move |()| {
        is_called2.set(true);
        Ok(())
    });
    let _ = format!("{svc:?}");
    let pipe = Pipeline::new((), svc);

    let res = pipe.call(()).await;
    assert_eq!(pipe.ready().await, Ok(()));
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), "pipe");
    assert!(is_called.get());

    // Service factory
    let is_called = Rc::new(Cell::new(false));
    let is_called2 = is_called.clone();

    let factory = factory(|()| async { Ok::<_, ()>("pipe") }).readiness(async move |()| {
        is_called2.set(true);
        Ok(())
    });
    let pipe = Pipeline::new((), factory.create(&()).await.unwrap());

    let res = pipe.call(()).await;
    assert_eq!(pipe.ready().await, Ok(()));
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), "pipe");
    assert!(is_called.get());
}
