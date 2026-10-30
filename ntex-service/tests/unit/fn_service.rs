use ntex::util::lazy;
use std::task::Poll;

use super::*;
use crate::{Pipeline, factory};

/// STATEMENT: Function services work directly and through factories with immediate lifecycle completion.
///
/// HOW:
/// - Construct pipelines from a function-service factory and a cloned function service.
/// - Check responses, readiness, debug formatting, and immediate shutdown.
#[ntex::test]
async fn test_fn_service() {
    let new_srv = factory(fn_service(async |()| Ok::<_, ()>("srv")).clone());
    let _ = format!("{new_srv:?}");

    let srv = Pipeline::new((), new_srv.create(&()).await.unwrap());
    let res = srv.call(()).await;
    assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), "srv");
    let _ = format!("{srv:?}");

    let new_srv = fn_service(async |()| Ok::<_, ()>("srv"));
    let srv = Pipeline::new((), new_srv.clone());
    let res = srv.call(()).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), "srv");
    let _ = format!("{srv:?}");

    assert_eq!(lazy(|cx| srv.poll_shutdown(cx)).await, Poll::Ready(()));
}

/// STATEMENT: Cloned function services retain their behavior when composed into factories.
///
/// HOW:
/// - Clone a function service before creating a factory pipeline and a direct pipeline.
/// - Verify their responses, readiness, debug output, and shutdown completion.
#[ntex::test]
async fn test_fn_service_comp() {
    let new_srv = fn_service(async |()| Ok::<_, ()>("srv")).clone();
    let _ = format!("{new_srv:?}");

    let srv = Pipeline::new((), factory(new_srv).create(&()).await.unwrap());
    let res = srv.call(()).await;
    assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), "srv");
    let _ = format!("{srv:?}");

    let new_srv = fn_service(async |()| Ok::<_, ()>("srv")).clone();
    let srv = Pipeline::new((), new_srv.clone());
    let res = srv.call(()).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), "srv");
    let _ = format!("{srv:?}");

    assert_eq!(lazy(|cx| srv.poll_shutdown(cx)).await, Poll::Ready(()));
}

/// STATEMENT: A cloned factory-created function service remains callable and ready.
///
/// HOW:
/// - Create and clone a function service through its factory, then put it in a pipeline.
/// - Check the response and immediate readiness and shutdown results.
#[ntex::test]
async fn test_fn_service_service() {
    let srv = Pipeline::new(
        (),
        factory(fn_service(async |()| Ok::<_, ()>("srv")).clone())
            .create(&())
            .await
            .unwrap()
            .clone(),
    );

    let res = srv.call(()).await;
    assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), "srv");
    assert_eq!(lazy(|cx| srv.poll_shutdown(cx)).await, Poll::Ready(()));
}

/// STATEMENT: A function factory can capture configuration in the service it creates.
///
/// HOW:
/// - Create a service from a factory receiving numeric configuration.
/// - Verify its response contains that configuration and readiness succeeds.
#[ntex::test]
async fn test_fn_factory() {
    let new_srv = factory(fn_factory(async move |cfg: &usize| {
        let cfg = *cfg;
        Ok::<_, ()>(fn_service(async move |()| Ok::<_, ()>(("srv", cfg))))
    }))
    .clone();

    let srv = Pipeline::new(1, new_srv.create(&1).await.unwrap());
    let res = srv.call(()).await;
    assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), ("srv", 1));
}

/// STATEMENT: State-aware function factories and plain function services expose working pipelines.
///
/// HOW:
/// - Create a pipeline from a state-aware closure factory and another from a plain function.
/// - Verify responses, readiness, debug formatting, and shutdown.
#[ntex::test]
async fn test_fn_service_st() {
    let new_srv = factory(async |(): &(), ()| Ok::<_, ()>("srv")).clone();
    let _ = format!("{new_srv:?}");

    let srv = Pipeline::new((), new_srv.create(&()).await.unwrap());
    let res = srv.call(()).await;
    assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), "srv");
    let _ = format!("{srv:?}");

    let new_srv = fn_service(async |()| Ok::<_, ()>("srv"));
    let srv = Pipeline::new((), new_srv.clone());
    let res = srv.call(()).await;
    assert!(res.is_ok());
    assert_eq!(res.unwrap(), "srv");
    let _ = format!("{srv:?}");

    assert_eq!(lazy(|cx| srv.poll_shutdown(cx)).await, Poll::Ready(()));
}
