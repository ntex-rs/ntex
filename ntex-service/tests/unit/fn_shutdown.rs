use std::{future::poll_fn, rc::Rc};

use crate::{Pipeline, factory, service};

use super::*;

/// STATEMENT: Shutdown closures run and leave both service and factory pipelines shut down.
///
/// HOW:
/// - Attach flag-setting shutdown closures to both construction forms and call each pipeline.
/// - Await shutdown, then poll it again and verify the callback flag and shutdown state.
#[ntex::test]
async fn test_fn_shutdown() {
    // Service factory
    let is_called = Rc::new(Cell::new(false));
    let is_called2 = is_called.clone();
    let fac = factory(|()| async { Ok::<_, ()>("pipe") }).shutdown(async move |()| {
        is_called2.set(true);
    });
    let _ = format!("{fac:?}");

    let pipe = Pipeline::new((), fac.clone().create(&()).await.unwrap());

    let res = pipe.call(()).await;
    assert_eq!(pipe.ready().await, Ok(()));
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), "pipe");
    assert!(!pipe.is_shutdown());
    pipe.shutdown().await;
    assert!(is_called.get());
    assert!(pipe.is_shutdown());

    poll_fn(|cx| pipe.poll_shutdown(cx)).await;
    assert!(pipe.is_shutdown());

    // Service
    let is_called = Rc::new(Cell::new(false));
    let is_called2 = is_called.clone();
    let svc = service(|()| async { Ok::<_, ()>("pipe") }).shutdown(async move |()| {
        is_called2.set(true);
    });
    let _ = format!("{fac:?}");

    let pipe = Pipeline::new((), svc);

    let res = pipe.call(()).await;
    assert_eq!(pipe.ready().await, Ok(()));
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), "pipe");
    assert!(!pipe.is_shutdown());
    pipe.shutdown().await;
    assert!(is_called.get());
    assert!(pipe.is_shutdown());

    poll_fn(|cx| pipe.poll_shutdown(cx)).await;
    assert!(pipe.is_shutdown());
}
